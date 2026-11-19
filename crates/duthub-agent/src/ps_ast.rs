//! Codex-style PowerShell AST safety analyzer.
//!
//! Instead of fragile regex heuristics, this module invokes PowerShell's own
//! native parser (`[System.Management.Automation.Language.Parser]::ParseInput`)
//! without executing the script, extracts a structured AST summary as JSON, and
//! classifies every command, redirection, and .NET member invocation under the
//! strictest-wins [`Verdict`] lattice (`Forbidden > Prompt > Allow`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::policy::{
    AccessKind, SandboxMode, Verdict, WorkspaceScope, evaluate_path_access,
    has_disallowed_provider_prefix,
};

/// Maximum PowerShell script length accepted for AST inspection.
pub const MAX_AST_SCRIPT_CHARS: usize = 32_768;

/// Maximum time allowed for PowerShell's parser to emit the AST JSON.
const AST_PARSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum stdout bytes accepted from the AST parser helper.
const AST_JSON_LIMIT_BYTES: usize = 512 * 1024;

/// PowerShell script that parses `$env:DUTHUB_PS_AST_INPUT` purely via
/// `Parser::ParseInput` (zero execution) and prints a compact JSON summary.
const PS_AST_INSPECTOR: &str = r#"
$ErrorActionPreference = 'Stop'
$src = [Environment]::GetEnvironmentVariable('DUTHUB_PS_AST_INPUT')
if ($null -eq $src) { $src = '' }
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseInput($src, [ref]$tokens, [ref]$errors)

$parseErrors = @()
if ($errors) {
    foreach ($e in $errors) { $parseErrors += $e.Message }
}

$commands = @()
$cmdAsts = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.CommandAst] }, $true)
foreach ($c in $cmdAsts) {
    $name = $c.GetCommandName()
    $isDyn = [string]::IsNullOrEmpty($name) -or ($c.InvocationOperator -ne [System.Management.Automation.Language.TokenKind]::Unknown)
    $args = @()
    $elems = $c.CommandElements
    for ($i = 1; $i -lt $elems.Count; $i++) {
        $el = $elems[$i]
        if ($el -is [System.Management.Automation.Language.CommandParameterAst]) {
            $args += @{
                kind = 'param'
                name = $el.ParameterName
                value = if ($el.Argument -is [System.Management.Automation.Language.StringConstantExpressionAst]) { $el.Argument.Value } else { $null }
                isDynamic = ($null -ne $el.Argument -and -not ($el.Argument -is [System.Management.Automation.Language.StringConstantExpressionAst]))
            }
        } elseif ($el -is [System.Management.Automation.Language.StringConstantExpressionAst]) {
            $args += @{
                kind = 'literal'
                name = $null
                value = $el.Value
                isDynamic = $false
            }
        } elseif ($el -is [System.Management.Automation.Language.ConstantExpressionAst]) {
            $args += @{
                kind = 'literal'
                name = $null
                value = [string]$el.Value
                isDynamic = $false
            }
        } else {
            $args += @{
                kind = 'dynamic'
                name = $null
                value = $el.Extent.Text
                isDynamic = $true
            }
        }
    }
    $commands += @{
        name = $name
        isDynamic = $isDyn
        args = $args
    }
}

$redirections = @()
$redAsts = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.FileRedirectionAst] }, $true)
foreach ($r in $redAsts) {
    $loc = $r.Location
    $isDyn = -not ($loc -is [System.Management.Automation.Language.StringConstantExpressionAst])
    $val = if ($isDyn) { $loc.Extent.Text } else { $loc.Value }
    $redirections += @{
        location = $val
        isDynamic = $isDyn
        append = [bool]$r.Append
    }
}

$memberCalls = @()
$memAsts = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.InvokeMemberExpressionAst] }, $true)
foreach ($m in $memAsts) {
    $target = if ($m.Expression) { $m.Expression.Extent.Text } else { '' }
    $member = if ($m.Member -is [System.Management.Automation.Language.StringConstantExpressionAst]) { $m.Member.Value } else { $m.Member.Extent.Text }
    $memberCalls += @{
        target = $target
        member = $member
        isStatic = [bool]$m.Static
    }
}

$summary = @{
    parseErrors = $parseErrors
    commands = $commands
    redirections = $redirections
    memberCalls = $memberCalls
}
[Console]::Out.Write(($summary | ConvertTo-Json -Depth 8 -Compress))
"#;

/// Structured AST facts extracted by PowerShell's native parser.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AstSummary {
    #[serde(default)]
    pub parse_errors: Vec<String>,
    #[serde(default)]
    pub commands: Vec<AstCommand>,
    #[serde(default)]
    pub redirections: Vec<AstRedirection>,
    #[serde(default)]
    pub member_calls: Vec<AstMemberCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AstCommand {
    pub name: Option<String>,
    #[serde(default)]
    pub is_dynamic: bool,
    #[serde(default)]
    pub args: Vec<AstArg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AstArg {
    pub kind: String,
    pub name: Option<String>,
    pub value: Option<String>,
    #[serde(default)]
    pub is_dynamic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AstRedirection {
    pub location: Option<String>,
    #[serde(default)]
    pub is_dynamic: bool,
    #[serde(default)]
    pub append: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AstMemberCall {
    pub target: String,
    pub member: String,
    #[serde(default)]
    pub is_static: bool,
}

/// Parse `script` with PowerShell's native AST parser (zero execution) and
/// classify its safety verdict against `scope` and `mode`.
pub fn analyze_script(script: &str, scope: &WorkspaceScope, mode: SandboxMode) -> Verdict {
    if script.trim().is_empty() {
        return Verdict::Forbidden {
            reason: "PowerShell script is empty".to_owned(),
        };
    }
    if script.chars().count() > MAX_AST_SCRIPT_CHARS {
        return Verdict::Forbidden {
            reason: format!("PowerShell script exceeds {MAX_AST_SCRIPT_CHARS} character limit"),
        };
    }
    let summary = match parse_script_ast(script) {
        Ok(summary) => summary,
        Err(reason) => {
            return Verdict::Forbidden {
                reason: format!("PowerShell AST inspection failed (fail closed): {reason}"),
            };
        }
    };
    analyze_ast_summary(&summary, scope, mode)
}

/// Invoke `powershell.exe` purely to parse `script` via `Parser::ParseInput`.
pub fn parse_script_ast(script: &str) -> Result<AstSummary, String> {
    let ps = duthub_sandbox::windows_powershell();
    if !ps.is_file() {
        return Err(format!(
            "PowerShell executable not found at {}",
            ps.display()
        ));
    }
    let args = vec![
        "-NoLogo".to_owned(),
        "-NoProfile".to_owned(),
        "-NonInteractive".to_owned(),
        "-Command".to_owned(),
        PS_AST_INSPECTOR.to_owned(),
    ];
    let env = vec![(
        std::ffi::OsString::from("DUTHUB_PS_AST_INPUT"),
        std::ffi::OsString::from(script),
    )];
    let stdout = duthub_core::process::run_binary_with_env(
        &ps,
        &args,
        &env,
        AST_PARSE_TIMEOUT,
        AST_JSON_LIMIT_BYTES,
    )
    .map_err(|error| error.to_string())?;
    let raw =
        String::from_utf8(stdout).map_err(|error| format!("invalid UTF-8 AST JSON: {error}"))?;
    serde_json::from_str::<AstSummary>(raw.trim())
        .map_err(|error| format!("failed to decode PowerShell AST JSON: {error}"))
}

/// Pure-Rust classifier over [`AstSummary`] implementing Codex's strictest-wins
/// policy lattice.
pub fn analyze_ast_summary(
    summary: &AstSummary,
    scope: &WorkspaceScope,
    mode: SandboxMode,
) -> Verdict {
    if !summary.parse_errors.is_empty() {
        return Verdict::Forbidden {
            reason: format!(
                "PowerShell syntax error (fail closed): {}",
                summary.parse_errors[0]
            ),
        };
    }
    if summary.commands.is_empty()
        && summary.member_calls.is_empty()
        && summary.redirections.is_empty()
    {
        return Verdict::Allow;
    }

    let mut verdict = Verdict::Allow;

    // 1. Inspect .NET static/instance member invocations first (always dangerous
    //    bypass vector in PowerShell when cmdlets are restricted).
    for call in &summary.member_calls {
        verdict = verdict.combine(classify_member_call(call));
    }

    // 2. Inspect file redirections (`>`, `>>`, `2>`, etc.).
    for redir in &summary.redirections {
        if redir.is_dynamic {
            let v = if mode == SandboxMode::ReadOnly {
                Verdict::Forbidden {
                    reason: "file redirection is forbidden in ReadOnly mode".to_owned(),
                }
            } else {
                Verdict::Prompt {
                    reason: format!(
                        "dynamic redirection target requires approval: {:?}",
                        redir.location
                    ),
                }
            };
            verdict = verdict.combine(v);
        } else if let Some(location) = redir.location.as_deref() {
            verdict = verdict.combine(evaluate_path_access(
                scope,
                location,
                AccessKind::Write,
                mode,
            ));
        } else {
            verdict = verdict.combine(Verdict::Forbidden {
                reason: "redirection missing target location".to_owned(),
            });
        }
    }

    // 3. Inspect every command invocation in the AST.
    for cmd in &summary.commands {
        verdict = verdict.combine(classify_command(cmd, scope, mode));
    }

    verdict
}

fn classify_member_call(call: &AstMemberCall) -> Verdict {
    let target = call.target.to_ascii_lowercase();
    let member = call.member.to_ascii_lowercase();

    // Forbidden dangerous .NET classes/methods (file mutation, process launch,
    // network clients, reflection/dynamic code, registry).
    const FORBIDDEN_TARGETS: &[&str] = &[
        "io.file",
        "system.io.file",
        "io.directory",
        "system.io.directory",
        "diagnostics.process",
        "system.diagnostics.process",
        "net.webclient",
        "system.net.webclient",
        "net.http.httpclient",
        "system.net.http.httpclient",
        "reflection.assembly",
        "system.reflection",
        "win32.registry",
        "microsoft.win32.registry",
        "runtime.interopservices.marshal",
        "management.automation.powershell",
    ];
    if FORBIDDEN_TARGETS.iter().any(|bad| target.contains(bad)) {
        return Verdict::Forbidden {
            reason: format!(
                ".NET member invocation is forbidden: {}::{}",
                call.target, call.member
            ),
        };
    }

    const FORBIDDEN_METHODS: &[&str] = &[
        "writealltext",
        "writeallbytes",
        "writealllines",
        "appendalltext",
        "appendalllines",
        "delete",
        "move",
        "copy",
        "replace",
        "create",
        "openwrite",
        "downloadfile",
        "downloadstring",
        "uploadfile",
        "uploadstring",
        "start",
        "load",
        "loadfrom",
        "invoke",
    ];
    if FORBIDDEN_METHODS.contains(&member.as_str()) {
        return Verdict::Forbidden {
            reason: format!(
                ".NET mutating/execution method is forbidden: {}::{}",
                call.target, call.member
            ),
        };
    }

    // Safe pure string/path/encoding inspection helpers are fine; everything else prompts.
    const SAFE_METHODS: &[&str] = &[
        "trim",
        "trimstart",
        "trimend",
        "split",
        "replace",
        "tolower",
        "tolowerinvariant",
        "toupper",
        "toupperinvariant",
        "contains",
        "startswith",
        "endswith",
        "substring",
        "indexof",
        "lastindexof",
        "count",
        "tostring",
        "getfilename",
        "getextension",
        "getdirectoryname",
    ];
    if SAFE_METHODS.contains(&member.as_str()) && !call.is_static {
        Verdict::Allow
    } else {
        Verdict::Prompt {
            reason: format!(
                ".NET member call requires approval: {}::{}",
                call.target, call.member
            ),
        }
    }
}

fn classify_command(cmd: &AstCommand, scope: &WorkspaceScope, mode: SandboxMode) -> Verdict {
    // Reject dynamic invocation (`& $cmd`, `. $script`, expression command names).
    if cmd.is_dynamic {
        return Verdict::Forbidden {
            reason: format!(
                "dynamic or call-operator command invocation is forbidden: {:?}",
                cmd.name
            ),
        };
    }

    let Some(raw_name) = cmd.name.as_deref() else {
        return Verdict::Forbidden {
            reason: "unnamed dynamic command invocation is forbidden".to_owned(),
        };
    };

    // Normalize command name (strip module prefix like `Microsoft.PowerShell.Management\`).
    let normalized = raw_name
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(raw_name)
        .trim()
        .to_ascii_lowercase();
    let base = normalized.strip_suffix(".exe").unwrap_or(&normalized);

    // Check for forbidden flags anywhere (e.g. `-EncodedCommand`).
    for arg in &cmd.args {
        if let Some(param) = arg.name.as_deref() {
            let p = param.to_ascii_lowercase();
            if matches!(
                p.as_str(),
                "encodedcommand" | "enc" | "ec" | "e" | "encodedarguments"
            ) {
                return Verdict::Forbidden {
                    reason: format!("encoded command parameter -{param} is forbidden"),
                };
            }
        }
        if let Some(val) = arg.value.as_deref()
            && has_disallowed_provider_prefix(val)
        {
            return Verdict::Forbidden {
                reason: format!("PowerShell provider access is forbidden: {val:?}"),
            };
        }
    }

    // 1. Explicitly forbidden commands (code exec, shells, privilege/registry/system tools).
    const FORBIDDEN_COMMANDS: &[&str] = &[
        "invoke-expression",
        "iex",
        "invoke-command",
        "icm",
        "start-process",
        "saps",
        "start",
        "add-type",
        "new-object",
        "set-executionpolicy",
        "enter-pssession",
        "new-pssession",
        "register-scheduledtask",
        "set-itemproperty",
        "sp",
        "new-itemproperty",
        "remove-itemproperty",
        "rp",
        "set-acl",
        "cmd",
        "powershell",
        "pwsh",
        "wscript",
        "cscript",
        "mshta",
        "rundll32",
        "reg",
        "regedit",
        "certutil",
        "bitsadmin",
        "schtasks",
        "sc",
        "net",
        "net1",
        "netsh",
        "icacls",
        "takeown",
        "whoami",
        "runas",
        "wmic",
        "msiexec",
        "setx",
    ];
    if FORBIDDEN_COMMANDS.contains(&base) {
        return Verdict::Forbidden {
            reason: format!("command `{raw_name}` is forbidden by execution policy"),
        };
    }

    // 2. File-mutating cmdlets: strictly require resolved literal targets inside active_dut_dir.
    const WRITE_COMMANDS: &[&str] = &[
        "set-content",
        "add-content",
        "ac",
        "out-file",
        "remove-item",
        "rm",
        "del",
        "ri",
        "rd",
        "rmdir",
        "erase",
        "copy-item",
        "cp",
        "cpi",
        "copy",
        "move-item",
        "mv",
        "mi",
        "move",
        "rename-item",
        "rni",
        "ren",
        "new-item",
        "ni",
        "mkdir",
        "md",
        "clear-content",
        "clc",
        "tee-object",
        "tee",
        "export-csv",
        "export-clixml",
        "set-item",
        "si",
    ];
    if WRITE_COMMANDS.contains(&base) {
        return classify_write_command(raw_name, &cmd.args, scope, mode);
    }

    // 3. Pure read-only filesystem/text cmdlets: allow active + sibling DUT reads,
    //    forbid protected paths, prompt on arbitrary host reads or dynamic paths.
    const READ_PATH_COMMANDS: &[&str] = &[
        "get-content",
        "gc",
        "cat",
        "type",
        "get-childitem",
        "gci",
        "ls",
        "dir",
        "select-string",
        "sls",
        "get-item",
        "gi",
        "test-path",
        "resolve-path",
        "rvpa",
        "get-filehash",
        "get-itemproperty",
        "gp",
    ];
    if READ_PATH_COMMANDS.contains(&base) {
        return classify_read_command(raw_name, &cmd.args, scope, mode);
    }

    // 4. Pure in-memory pipeline / formatting cmdlets (no filesystem side effects).
    const PURE_PIPELINE_COMMANDS: &[&str] = &[
        "measure-object",
        "measure",
        "select-object",
        "select",
        "where-object",
        "where",
        "?",
        "sort-object",
        "sort",
        "group-object",
        "group",
        "format-table",
        "ft",
        "format-list",
        "fl",
        "out-string",
        "write-output",
        "echo",
        "write",
        "convertto-json",
        "convertfrom-json",
        "convertfrom-csv",
        "get-location",
        "pwd",
        "gl",
        "get-date",
        "compare-object",
        "diff",
        "join-path",
        "split-path",
    ];
    if PURE_PIPELINE_COMMANDS.contains(&base) {
        return Verdict::Allow;
    }

    // 5. Network cmdlets always prompt (never auto-allowed).
    const NETWORK_COMMANDS: &[&str] = &[
        "invoke-webrequest",
        "iwr",
        "curl",
        "wget",
        "invoke-restmethod",
        "irm",
        "test-netconnection",
        "tnc",
    ];
    if NETWORK_COMMANDS.contains(&base) {
        return Verdict::Prompt {
            reason: format!("network command `{raw_name}` requires explicit approval"),
        };
    }

    // 6. Anything else (unknown binary or cmdlet): prompt.
    Verdict::Prompt {
        reason: format!("unclassified command `{raw_name}` requires explicit approval"),
    }
}

fn classify_write_command(
    cmd_name: &str,
    args: &[AstArg],
    scope: &WorkspaceScope,
    mode: SandboxMode,
) -> Verdict {
    if mode == SandboxMode::ReadOnly {
        return Verdict::Forbidden {
            reason: format!("mutating command `{cmd_name}` is forbidden in ReadOnly mode"),
        };
    }
    let targets = extract_path_arguments(args, true);
    if targets.is_empty() {
        return Verdict::Prompt {
            reason: format!(
                "mutating command `{cmd_name}` has no statically resolvable target path"
            ),
        };
    }
    let mut verdict = Verdict::Allow;
    for target in targets {
        match target {
            PathArg::Literal(path) => {
                verdict =
                    verdict.combine(evaluate_path_access(scope, path, AccessKind::Write, mode));
            }
            PathArg::Dynamic(expr) => {
                verdict = verdict.combine(Verdict::Prompt {
                    reason: format!(
                        "mutating command `{cmd_name}` uses dynamic target path `{expr}`"
                    ),
                });
            }
        }
    }
    verdict
}

fn classify_read_command(
    cmd_name: &str,
    args: &[AstArg],
    scope: &WorkspaceScope,
    mode: SandboxMode,
) -> Verdict {
    let targets = extract_path_arguments(args, false);
    if targets.is_empty() {
        // E.g. `Get-ChildItem` with no path reads cwd (= active_dut_dir).
        return Verdict::Allow;
    }
    let mut verdict = Verdict::Allow;
    for target in targets {
        match target {
            PathArg::Literal(path) => {
                verdict =
                    verdict.combine(evaluate_path_access(scope, path, AccessKind::Read, mode));
            }
            PathArg::Dynamic(expr) => {
                verdict = verdict.combine(Verdict::Prompt {
                    reason: format!("read command `{cmd_name}` uses dynamic path `{expr}`"),
                });
            }
        }
    }
    verdict
}

enum PathArg<'a> {
    Literal(&'a str),
    Dynamic(&'a str),
}

/// Extract path-like arguments from a PowerShell command's AST argument list.
///
/// Recognizes explicit path parameters (`-Path`, `-LiteralPath`, `-FilePath`,
/// `-Destination`, `-OutFile`) as well as positional arguments not attached to
/// non-path flags (like `-Pattern`, `-Encoding`, `-Tail`, `-First`, `-Value`).
fn extract_path_arguments<'a>(args: &'a [AstArg], is_write: bool) -> Vec<PathArg<'a>> {
    const PATH_PARAMS: &[&str] = &[
        "path",
        "literalpath",
        "filepath",
        "destination",
        "outfile",
        "pspath",
        "newname",
    ];
    const NON_PATH_VALUE_PARAMS: &[&str] = &[
        "pattern",
        "encoding",
        "value",
        "inputobject",
        "tail",
        "totalcount",
        "head",
        "first",
        "last",
        "readcount",
        "filter",
        "include",
        "exclude",
        "depth",
        "context",
        "delimiter",
        "itemtype",
        "type",
        "width",
    ];
    const SWITCH_PARAMS: &[&str] = &[
        "raw",
        "recurse",
        "force",
        "file",
        "directory",
        "name",
        "append",
        "noclobber",
        "nonewline",
        "simplematch",
        "casesensitive",
        "quiet",
        "list",
        "notmatch",
        "allmatches",
        "passthru",
        "whatif",
        "confirm",
        "hidden",
    ];

    let mut out = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg.kind == "param" {
            let pname = arg.name.as_deref().unwrap_or("").to_ascii_lowercase();
            let is_path_param = PATH_PARAMS.contains(&pname.as_str());
            let is_non_path = NON_PATH_VALUE_PARAMS.contains(&pname.as_str());
            let is_switch = SWITCH_PARAMS.contains(&pname.as_str());

            if is_path_param {
                if arg.is_dynamic {
                    out.push(PathArg::Dynamic(arg.value.as_deref().unwrap_or("<expr>")));
                } else if let Some(val) = arg.value.as_deref() {
                    out.push(PathArg::Literal(val));
                } else if let Some(next) = args.get(i + 1)
                    && next.kind != "param"
                {
                    if next.is_dynamic {
                        out.push(PathArg::Dynamic(next.value.as_deref().unwrap_or("<expr>")));
                    } else if let Some(val) = next.value.as_deref() {
                        out.push(PathArg::Literal(val));
                    }
                    i += 1;
                }
            } else if is_non_path {
                // Skip the parameter's value token if it came as the next element.
                if arg.value.is_none()
                    && !arg.is_dynamic
                    && args.get(i + 1).is_some_and(|next| next.kind != "param")
                {
                    i += 1;
                }
            } else if !is_switch
                && is_write
                && arg.value.is_none()
                && !arg.is_dynamic
                && args.get(i + 1).is_some_and(|next| next.kind != "param")
            {
                // Unknown value parameter on a write command: conservatively skip
                // only if not dynamic; if dynamic, capture it.
                let next = &args[i + 1];
                if next.is_dynamic {
                    out.push(PathArg::Dynamic(next.value.as_deref().unwrap_or("<expr>")));
                }
                i += 1;
            }
        } else if arg.is_dynamic {
            out.push(PathArg::Dynamic(arg.value.as_deref().unwrap_or("<expr>")));
        } else if let Some(val) = arg.value.as_deref() {
            // For `Select-String "pattern" path.log`, positional arg 0 without
            // `-Pattern` is the regex pattern; still safe to check or treat as
            // path-like only when it looks like a path or is a write command.
            out.push(PathArg::Literal(val));
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_scope() -> (
        tempfile::TempDir,
        WorkspaceScope,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let dut_a = tmp.path().join("dut-a");
        let dut_b = tmp.path().join("dut-b");
        let cfg = tmp.path().join(".duthub");
        std::fs::create_dir_all(&dut_a).unwrap();
        std::fs::create_dir_all(&dut_b).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(dut_a.join("boot.log"), "ok\n").unwrap();
        std::fs::write(dut_b.join("serial.log"), "sibling\n").unwrap();
        std::fs::write(cfg.join("settings.json"), "{}\n").unwrap();
        let scope = WorkspaceScope::new(&dut_a)
            .with_sibling_dut(&dut_b)
            .with_protected_path(&cfg);
        (tmp, scope, dut_a, dut_b)
    }

    #[test]
    fn ast_summary_enforces_strictest_wins_on_synthetic_trees() {
        let (_tmp, scope, _dut_a, dut_b) = test_scope();

        // Parse error fails closed.
        let bad_parse = AstSummary {
            parse_errors: vec!["Unexpected token".to_owned()],
            ..Default::default()
        };
        assert!(
            analyze_ast_summary(&bad_parse, &scope, SandboxMode::DutWorkspaceWrite).is_forbidden()
        );

        // Redirection into sibling DUT is forbidden.
        let redir_sibling = AstSummary {
            redirections: vec![AstRedirection {
                location: Some(dut_b.join("pwn.txt").display().to_string()),
                is_dynamic: false,
                append: false,
            }],
            ..Default::default()
        };
        assert!(
            analyze_ast_summary(&redir_sibling, &scope, SandboxMode::DutWorkspaceWrite)
                .is_forbidden()
        );

        // .NET `[IO.File]::WriteAllText` is forbidden.
        let dotnet_write = AstSummary {
            member_calls: vec![AstMemberCall {
                target: "[System.IO.File]".to_owned(),
                member: "WriteAllText".to_owned(),
                is_static: true,
            }],
            ..Default::default()
        };
        assert!(
            analyze_ast_summary(&dotnet_write, &scope, SandboxMode::DutWorkspaceWrite)
                .is_forbidden()
        );
    }

    #[cfg(windows)]
    #[test]
    fn live_windows_powershell_ast_policy_matrix() {
        let (tmp, scope, _dut_a, dut_b) = test_scope();
        let sibling_log = dut_b.join("serial.log");
        let sibling_out = dut_b.join("tamper.txt");
        let secret_cfg = tmp.path().join(".duthub").join("settings.json");

        // 1. Reading active DUT log → Allow.
        assert_eq!(
            analyze_script(
                "Get-Content -Path boot.log -Tail 20 | Select-String -Pattern 'ok'",
                &scope,
                SandboxMode::DutWorkspaceWrite
            ),
            Verdict::Allow
        );

        // 2. Reading sibling DUT log → Allow (cross-DUT read-only analysis).
        let read_sibling = format!("Get-Content -LiteralPath '{}'", sibling_log.display());
        assert_eq!(
            analyze_script(&read_sibling, &scope, SandboxMode::DutWorkspaceWrite),
            Verdict::Allow
        );

        // 3. Writing inside active DUT → Allow in DutWorkspaceWrite, Forbidden in ReadOnly.
        assert_eq!(
            analyze_script(
                "Set-Content -Path report.md -Value 'diagnosis'",
                &scope,
                SandboxMode::DutWorkspaceWrite
            ),
            Verdict::Allow
        );
        assert!(
            analyze_script(
                "Set-Content -Path report.md -Value 'diagnosis'",
                &scope,
                SandboxMode::ReadOnly
            )
            .is_forbidden()
        );

        // 4. Writing to sibling DUT (cmdlet OR redirection) → Forbidden.
        let write_sibling = format!("Set-Content -Path '{}' -Value 'x'", sibling_out.display());
        assert!(
            analyze_script(&write_sibling, &scope, SandboxMode::DutWorkspaceWrite).is_forbidden()
        );
        let redir_sibling = format!("Write-Output 'x' > '{}'", sibling_out.display());
        assert!(
            analyze_script(&redir_sibling, &scope, SandboxMode::DutWorkspaceWrite).is_forbidden()
        );

        // 5. Reading `.duthub/settings.json` → Forbidden.
        let read_secret = format!("Get-Content '{}'", secret_cfg.display());
        assert!(
            analyze_script(&read_secret, &scope, SandboxMode::DutWorkspaceWrite).is_forbidden()
        );

        // 6. Code execution / bypass primitives → Forbidden.
        for hostile in [
            "Invoke-Expression 'Get-Date'",
            "iex '1+1'",
            "powershell -EncodedCommand SQBFAFgA",
            "[System.IO.File]::WriteAllText('x.txt', 'hi')",
            r"Get-ItemProperty 'HKCU:\Software\Microsoft'",
            "& $unknownCmd",
        ] {
            assert!(
                analyze_script(hostile, &scope, SandboxMode::DutWorkspaceWrite).is_forbidden(),
                "expected forbidden for hostile script: {hostile}"
            );
        }

        // 7. Dynamic write path → Prompt (never silently allowed).
        let dyn_write = analyze_script(
            "Set-Content -Path $target -Value 'hi'",
            &scope,
            SandboxMode::DutWorkspaceWrite,
        );
        assert!(
            matches!(dyn_write, Verdict::Prompt { .. }),
            "expected Prompt for dynamic write path, got {dyn_write:?}"
        );
    }
}
