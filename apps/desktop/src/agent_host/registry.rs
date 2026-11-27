//! Agent definition → launch spec (program, args, env, hash/version probe).
//!
//! - `kind=acp` can run any ACP program: `program + args + cwd(sandbox) + env`
//!   allow-list.
//! - `program = null` is only allowed for goose, resolved in priority order:
//!   explicitly configured external program (`program + expectedSha256`, see
//!   [`resolve_external`]) → downloaded install (`<config>/tools/goose/`, through
//!   `duthub_desktop::tool_resolver`) → bare `goose` on PATH.
//! - Nothing is embedded any more: goose is installed from the
//!   tool-downloads page like adb/ffmpeg. When no candidate is usable the launch
//!   fails with [`RegistryError::NotInstalled`] instead of unwrapping, and
//!   [`goose_install_state`] is the public status the Agent page renders
//!   ("go to tool downloads").
//! - External programs must pin `expectedSha256`; the full SHA-256 is recomputed
//!   at launch through [`bundle::resolve`], and goose additionally passes a
//!   `--version` probe accepting **>= 1.60.2** (newer versions are fine). A
//!   downloaded candidate is re-hashed against the digest the install kernel
//!   recorded in its manifest: stale bytes fall through to the next tier, while a
//!   present-but-rejected candidate fails loudly instead of silently running
//!   something else.
//! - `kind=cli` is schema/storage only this phase: launch fails with a clear
//!   "CLI profile not implemented" error.

use std::path::{Path, PathBuf};

use duthub_core::agent_config::{AgentDefinition, AgentKind};

use super::bundle;

/// The tool-downloads id for goose (`duthub_tooldown::spec_by_id("goose")`).
pub const GOOSE_TOOL_ID: &str = "goose";
/// Recommended/minimum goose version — the single literal lives in
/// [`bundle::GOOSE_VERSION`] so the UI and the launch gate cannot drift apart.
pub const MIN_GOOSE_VERSION: &str = bundle::GOOSE_VERSION;
pub const GOOSE_AGENT_ID: &str = "goose";

#[derive(Debug)]
pub enum RegistryError {
    Disabled(String),
    CliUnsupported(String),
    Program(String),
    Bundle(String),
    Version(String),
    Env(String),
    /// No built-in goose is available: neither a downloaded install nor a PATH
    /// binary. Carries a displayable Chinese reason — callers must surface it
    /// (and the tool-downloads entry point), never unwrap.
    NotInstalled(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Disabled(agent) => write!(formatter, "Agent {agent} 已停用"),
            RegistryError::CliUnsupported(agent) => write!(
                formatter,
                "Agent {agent} 是 CLI profile，本轮未实现（CLI profile 本轮未实现）"
            ),
            RegistryError::Program(reason) => write!(formatter, "Agent 程序无效: {reason}"),
            RegistryError::Bundle(reason) => write!(formatter, "Agent 资产校验失败: {reason}"),
            RegistryError::Version(reason) => write!(formatter, "Agent 版本不满足要求: {reason}"),
            RegistryError::Env(reason) => write!(formatter, "Agent 环境变量不可用: {reason}"),
            RegistryError::NotInstalled(reason) => write!(formatter, "{reason}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// The specifications actually used during startup (`Debug` mode does not display the values of
/// environment variables).
#[derive(Clone, PartialEq, Eq)]
pub struct AgentLaunchSpec {
    pub agent_id: String,
    pub agent_name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Legacy flag: duthub no longer embeds a goose build, so
    /// product paths always record `false`. Kept because the spec is public and
    /// the UI badge projection still reads it.
    pub embedded: bool,
    pub sha256: String,
    pub version: Option<String>,
    /// Currently, only the facts related to manifests/files are recorded. Signature chain
    /// verification is not performed. All external programs are marked as `"NotSigned"`.
    pub signature: String,
}

impl std::fmt::Debug for AgentLaunchSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env_keys: Vec<&str> = self.env.iter().map(|(key, _)| key.as_str()).collect();
        formatter
            .debug_struct("AgentLaunchSpec")
            .field("agent_id", &self.agent_id)
            .field("agent_name", &self.agent_name)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env_keys", &env_keys)
            .field("embedded", &self.embedded)
            .field("sha256", &self.sha256)
            .field("version", &self.version)
            .field("signature", &self.signature)
            .finish()
    }
}

impl AgentLaunchSpec {
    #[must_use]
    pub fn display(&self) -> String {
        let source = if self.embedded {
            "内嵌".to_owned()
        } else {
            self.program.display().to_string()
        };
        match &self.version {
            Some(version) => format!("{} · {} · {version}", self.agent_name, source),
            None => format!("{} · {}", self.agent_name, source),
        }
    }
}

/// UI Detection Results: SHA-256 hash value, version, and signature status of the external path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramProbe {
    pub path: PathBuf,
    pub sha256: String,
    pub version: Option<String>,
    pub signature: String,
    pub normalized_version: Option<(u64, u64, u64)>,
}

/// Determines whether synchronization verification is allowed to be initiated (without performing
/// any disk I/O operations). The CLI explicitly rejects this option.
pub fn ensure_launchable(definition: &AgentDefinition) -> Result<(), RegistryError> {
    if !definition.enabled {
        return Err(RegistryError::Disabled(definition.id.clone()));
    }
    match definition.kind {
        AgentKind::Cli => Err(RegistryError::CliUnsupported(definition.id.clone())),
        AgentKind::Acp => Ok(()),
    }
}

/// Parsing the definition to the startup specifications; external program hashes/versions fail to
/// be closed here.
pub fn resolve(
    work_dir: &Path,
    definition: &AgentDefinition,
) -> Result<AgentLaunchSpec, RegistryError> {
    ensure_launchable(definition)?;
    let env = duthub_core::agent_config::load_agent_env(definition)
        .map_err(|error| RegistryError::Env(error.to_string()))?;
    match definition.program_path() {
        Some(raw) => resolve_external(work_dir, definition, raw, env),
        None => resolve_builtin_goose(work_dir, definition, env),
    }
}

/// External programs must be local absolute paths (UNC / SMB network paths are
/// rejected to prevent NTLM credential leaks and remote binary TOCTOU).
#[must_use]
pub fn is_absolute_program(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    // Reject UNC / extended-device network paths (`\\server\share`, `//server/share`).
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        return false;
    }
    let bytes = raw.as_bytes();
    let drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    drive_absolute || Path::new(raw).is_absolute()
}

fn ensure_absolute_program(raw: &str) -> Result<(), RegistryError> {
    if is_absolute_program(raw) {
        return Ok(());
    }
    Err(RegistryError::Program(format!(
        "外部程序必须是本地绝对路径（禁止相对路径与 UNC 网络路径）: {raw}"
    )))
}

fn resolve_external(
    work_dir: &Path,
    definition: &AgentDefinition,
    raw: &str,
    env: Vec<(String, String)>,
) -> Result<AgentLaunchSpec, RegistryError> {
    ensure_absolute_program(raw)?;
    let expected = definition
        .expected_sha256
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .ok_or_else(|| {
            RegistryError::Program(format!(
                "{} 配置了外部程序但缺少 expectedSha256；请重新选择程序并保存",
                definition.id
            ))
        })?;
    let path = PathBuf::from(raw);
    let resolved = bundle::resolve(work_dir, Some((&path, expected)))
        .map_err(|error| RegistryError::Bundle(error.to_string()))?;
    let version = if definition.is_goose() {
        let line = bundle::probe_version(&resolved)
            .map_err(|error| RegistryError::Version(error.to_string()))?;
        ensure_goose_version(&line)?;
        Some(line)
    } else {
        None
    };
    // The same subcommand rule applies to any agent entry pointed at a program
    // literally named `goose.exe` (a second ACP entry can share the binary with
    // the built-in goose): without `acp` it drops into the interactive
    // configure flow and the process exits. Explicit `args` always win.
    let args = goose_acp_args(&resolved, &definition.args);
    Ok(AgentLaunchSpec {
        agent_id: definition.id.clone(),
        agent_name: definition.name.clone(),
        program: resolved,
        args,
        env,
        embedded: false,
        sha256: expected.to_ascii_lowercase(),
        version,
        signature: "NotSigned".into(),
    })
}

/// Where the built-in (not explicitly configured) goose program comes from.
///
/// Public on purpose: the Agent page renders this state and every failure path
/// that used to unwrap an embedded asset reports it instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GooseInstallState {
    /// Installed from the tool-downloads page; `sha256` is the digest the
    /// install kernel recorded in `<config>/tools/goose/manifest.json`.
    Downloaded { path: PathBuf, sha256: String },
    /// A bare `goose` resolves on PATH (nothing is pinned for it).
    Path { path: PathBuf },
    /// Nothing usable: the UI must offer the tool-downloads entry point.
    NotInstalled,
}

impl GooseInstallState {
    /// The program a session would launch, when there is one.
    #[must_use]
    pub fn program(&self) -> Option<&Path> {
        match self {
            GooseInstallState::Downloaded { path, .. } | GooseInstallState::Path { path } => {
                Some(path.as_path())
            }
            GooseInstallState::NotInstalled => None,
        }
    }

    /// Whether the Agent page should show the "go to tool downloads" guidance.
    #[must_use]
    pub fn needs_install(&self) -> bool {
        matches!(self, GooseInstallState::NotInstalled)
    }
}

impl std::fmt::Display for GooseInstallState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GooseInstallState::Downloaded { path, .. } => {
                write!(formatter, "已下载安装：{}", path.display())
            }
            GooseInstallState::Path { path } => write!(formatter, "PATH：{}", path.display()),
            GooseInstallState::NotInstalled => write!(
                formatter,
                "goose 未安装：请在「工具下载」页下载安装 goose，或配置外部程序路径"
            ),
        }
    }
}

/// Built-in goose availability for this process (downloaded install > PATH).
///
/// Cheap by design: it only classifies the candidates (no hashing, no version
/// probe), so the Agent page may call it on every render. The launch path
/// re-verifies the bytes it picks ([`resolve_builtin_from`]).
#[must_use]
pub fn goose_install_state() -> GooseInstallState {
    goose_install_state_from(
        downloaded_goose(),
        duthub_core::process::resolve_executable(GOOSE_TOOL_ID).ok(),
    )
}

/// [`goose_install_state`] with an explicit downloaded-tools root
/// (`<config dir>/tools`). Tests and embedders point this at a scratch dir.
#[must_use]
pub fn goose_install_state_in(tools_root: &Path) -> GooseInstallState {
    goose_install_state_from(
        downloaded_goose_in(tools_root),
        duthub_core::process::resolve_executable(GOOSE_TOOL_ID).ok(),
    )
}

/// Priority rule, written once: a downloaded install shadows PATH.
#[must_use]
pub fn goose_install_state_from(
    downloaded: Option<(PathBuf, String)>,
    path_program: Option<PathBuf>,
) -> GooseInstallState {
    match (downloaded, path_program) {
        (Some((path, sha256)), _) => GooseInstallState::Downloaded { path, sha256 },
        (None, Some(path)) => GooseInstallState::Path { path },
        (None, None) => GooseInstallState::NotInstalled,
    }
}

/// Downloaded goose under `<config>/tools/goose/`, with the digest recorded by
/// the install kernel. `None` = not installed / corrupt manifest / missing exe.
#[must_use]
pub fn downloaded_goose() -> Option<(PathBuf, String)> {
    let tools_root = crate::tool_resolver::config_tools_root()?;
    downloaded_goose_in(&tools_root)
}

/// [`downloaded_goose`] anchored at an explicit tools root.
#[must_use]
pub fn downloaded_goose_in(tools_root: &Path) -> Option<(PathBuf, String)> {
    crate::tool_resolver::downloaded_install_in(tools_root, GOOSE_TOOL_ID)
}

/// Resolve the built-in goose from explicit candidates.
///
/// Priority: downloaded install > PATH. Public so the tier order can be tested
/// without touching the process config dir; [`resolve_builtin_goose`] gathers
/// the real candidates and calls this.
///
/// A downloaded candidate whose bytes no longer match the install record falls
/// through to PATH (unusable); one whose bytes match but which does not answer
/// like goose fails loudly.
pub fn resolve_builtin_from(
    definition: &AgentDefinition,
    env: Vec<(String, String)>,
    downloaded: Option<&(PathBuf, String)>,
    path_program: Option<PathBuf>,
) -> Result<AgentLaunchSpec, RegistryError> {
    if !definition.is_goose() {
        return Err(RegistryError::Program(format!(
            "{} 不是 goose，必须提供外部程序路径",
            definition.id
        )));
    }
    // 1. Downloaded install: re-verify the current bytes against the digest the
    //    install kernel recorded, then apply the version gate. duthub never
    //    downloads or pins goose itself — tooldown owns that.
    if let Some((path, recorded)) = downloaded {
        match verify_downloaded_goose(path, recorded)? {
            Some((program, sha256, line)) => {
                let args = goose_acp_args(&program, &definition.args);
                return Ok(AgentLaunchSpec {
                    agent_id: definition.id.clone(),
                    agent_name: definition.name.clone(),
                    program,
                    args,
                    env,
                    embedded: false,
                    sha256,
                    version: Some(line),
                    signature: "NotSigned".into(),
                });
            }
            None => {
                // Fail closed when a managed download's bytes no longer match its
                // install manifest rather than silently executing an unpinned PATH binary.
                return Err(RegistryError::Program(format!(
                    "已下载 goose 哈希与安装记录不一致（{}），已拒绝回退到 PATH；请在「工具下载」页重新安装",
                    path.display()
                )));
            }
        }
    }
    // 2. PATH fallback: bare-name search (the shared resolver rejects
    //    .bat/.cmd wrappers). Nothing is pinned for a PATH binary — its current
    //    bytes are hashed for the record and the version gate applies.
    let Some(program) = path_program else {
        let stale = if downloaded.is_some() {
            "；已下载候选的字节与安装记录不一致，需要重新下载"
        } else {
            ""
        };
        return Err(RegistryError::NotInstalled(format!(
            "goose 未安装：请在「工具下载」页下载安装 goose，或配置外部程序路径\
             （已检查 <config>/tools/goose/ 的下载安装与 PATH 上的 goose{stale}）"
        )));
    };
    let line = bundle::probe_version(&program).map_err(|error| {
        // Keep the candidate path in the message: a launch failure must say
        // *which* goose was rejected (download vs PATH vs external).
        RegistryError::Version(format!("{}：{error}", program.display()))
    })?;
    ensure_goose_version(&line)?;
    let sha256 = duthub_core::store::sha256_file(&program).map_err(|error| {
        RegistryError::Program(format!("读取 {} 哈希失败: {error}", program.display()))
    })?;
    let args = goose_acp_args(&program, &definition.args);
    Ok(AgentLaunchSpec {
        agent_id: definition.id.clone(),
        agent_name: definition.name.clone(),
        program,
        args,
        env,
        embedded: false,
        sha256,
        version: Some(line),
        signature: "NotSigned".into(),
    })
}

/// Arguments for a resolved goose program.
///
/// The released `goose.exe` is a multi-command CLI: the ACP server is the `acp`
/// subcommand, and starting it with no arguments drops into the interactive
/// `configure` flow (`Error: goose configure requires an interactive terminal`),
/// so a session would never initialize. A dedicated `goose-acp.exe` binary is
/// already the server and takes no arguments. An explicit `args` list in the
/// definition always wins, so either shape stays configurable without editing
/// defaults.
///
/// `pub(crate)` so the ACP client applies the same rule when a caller spawns
/// `goose.exe` directly (tests and any future launcher), not just the registry.
pub(crate) fn goose_acp_args(program: &Path, configured: &[String]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    let stem = program
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if stem.eq_ignore_ascii_case("goose") {
        vec!["acp".to_owned()]
    } else {
        Vec::new()
    }
}

/// Goose without an explicit external program: gather the real candidates and
/// apply [`resolve_builtin_from`]'s priority (downloaded install > PATH).
fn resolve_builtin_goose(
    _work_dir: &Path,
    definition: &AgentDefinition,
    env: Vec<(String, String)>,
) -> Result<AgentLaunchSpec, RegistryError> {
    resolve_builtin_from(
        definition,
        env,
        downloaded_goose().as_ref(),
        duthub_core::process::resolve_executable(GOOSE_TOOL_ID).ok(),
    )
}

/// Accept a downloaded goose candidate: `Some((path, sha256, version line))`
/// when the on-disk bytes still match the install record and the version gate
/// passes; `Ok(None)` when the bytes changed since install (unusable — the
/// caller falls through); `Err` when the candidate is present but rejected
/// (unreadable for hashing is unusable, a failed `--version` probe or an old
/// version is a loud policy failure).
fn verify_downloaded_goose(
    path: &Path,
    recorded: &str,
) -> Result<Option<(PathBuf, String, String)>, RegistryError> {
    let recorded = recorded.trim().to_ascii_lowercase();
    let Ok(actual) = duthub_core::store::sha256_file(path) else {
        return Ok(None);
    };
    if actual != recorded {
        return Ok(None);
    }
    let line = bundle::probe_version(path)
        .map_err(|error| RegistryError::Version(format!("{}：{error}", path.display())))?;
    ensure_goose_version(&line)?;
    Ok(Some((path.to_path_buf(), actual, line)))
}

/// External path verification (UI "Verify and Save"): Complete SHA-256 hash value, along with the
/// goose version number and unsigned status.
///
/// The non-goose ACP program does not require the `--version` output format. Therefore, it only
/// calculates the hash value. Goose must be running on a version that meets the requirement of >=
/// [`MIN_GOOSE_VERSION`).
pub fn probe_program(path: &Path, goose: bool) -> Result<ProgramProbe, RegistryError> {
    let raw = path.to_string_lossy();
    ensure_absolute_program(&raw)?;
    let metadata = std::fs::metadata(path)
        .map_err(|error| RegistryError::Program(format!("{}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(RegistryError::Program(format!(
            "{} 不是文件",
            path.display()
        )));
    }
    let sha256 = duthub_core::store::sha256_file(path)
        .map_err(|error| RegistryError::Program(format!("读取哈希失败: {error}")))?;
    let (version, normalized_version) = if goose {
        let line = bundle::probe_version(path)
            .map_err(|error| RegistryError::Version(error.to_string()))?;
        ensure_goose_version(&line)?;
        let normalized = parse_version(&line);
        (Some(line), normalized)
    } else {
        (None, None)
    };
    Ok(ProgramProbe {
        path: path.to_path_buf(),
        sha256,
        version,
        signature: "NotSigned".into(),
        normalized_version,
    })
}

/// External/downloaded goose version gate: accepts >= [`MIN_GOOSE_VERSION`]
/// (`1.60.2`), never requires an exact match with any bundled build.
pub fn ensure_goose_version(line: &str) -> Result<(), RegistryError> {
    if version_at_least(line, MIN_GOOSE_VERSION) {
        return Ok(());
    }
    Err(RegistryError::Version(format!(
        "期望 goose >= {MIN_GOOSE_VERSION}，实际 `{line}`"
    )))
}

/// Comparison version: Parses the first `major.minor.patch` value in the text, and compares them
/// based on numerical values.
#[must_use]
pub fn version_at_least(actual: &str, minimum: &str) -> bool {
    match (parse_version(actual), parse_version(minimum)) {
        (Some(actual), Some(minimum)) => actual >= minimum,
        _ => false,
    }
}

/// Analyzing the version numbers in `goose-acp 1.60.2` and `goose-acp v1.61.0-rc.1`.
#[must_use]
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    for token in text.split(|character: char| {
        !(character.is_ascii_alphanumeric() || character == '.' || character == '-')
    }) {
        let token = token.trim_start_matches(['v', 'V']);
        let core = token.split('-').next().unwrap_or(token);
        let mut parts = core.split('.');
        let Some(major) = parts.next().and_then(|part| part.parse::<u64>().ok()) else {
            continue;
        };
        let Some(minor) = parts.next().and_then(|part| part.parse::<u64>().ok()) else {
            continue;
        };
        let Some(patch) = parts.next().and_then(|part| part.parse::<u64>().ok()) else {
            continue;
        };
        if parts.next().is_some() {
            continue;
        }
        return Some((major, minor, patch));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gate_accepts_newer_and_rejects_older_or_garbage() {
        for good in [
            "goose-acp 1.60.2",
            "goose-acp 1.60.3",
            "goose-acp 1.61.0",
            "goose-acp 2.0.0",
            "goose-acp v1.60.2",
            "goose-acp 1.61.0-rc.1",
        ] {
            assert!(version_at_least(good, MIN_GOOSE_VERSION), "{good}");
            assert!(ensure_goose_version(good).is_ok(), "{good}");
        }
        for bad in ["goose-acp 1.59.9", "goose-acp 1.60.1", "unknown", ""] {
            assert!(!version_at_least(bad, MIN_GOOSE_VERSION), "{bad}");
            assert!(ensure_goose_version(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_version("goose-acp 1.60.2"), Some((1, 60, 2)));
        assert_eq!(parse_version("v2.3.4"), Some((2, 3, 4)));
    }

    #[test]
    fn cli_profile_is_explicitly_unsupported() {
        let mut agent = AgentDefinition::goose();
        agent.id = "claude".into();
        agent.kind = AgentKind::Cli;
        let error = ensure_launchable(&agent).unwrap_err();
        assert!(
            error.to_string().contains("CLI profile 本轮未实现"),
            "{error}"
        );
    }

    #[test]
    fn absolute_program_detection_covers_drive_and_rejects_unc_forms() {
        for absolute in ["C:/tools/acme.exe", "c:\\tools\\acme.exe"] {
            assert!(is_absolute_program(absolute), "{absolute}");
        }
        for unc in [r"\\server\share\acme.exe", "//server/share/acme.exe"] {
            assert!(
                !is_absolute_program(unc),
                "UNC paths must be rejected as agent program paths: {unc}"
            );
        }
        #[cfg(not(windows))]
        assert!(is_absolute_program("/usr/bin/acme"));
        #[cfg(windows)]
        assert!(
            !is_absolute_program("/usr/bin/acme"),
            "The root path on Windows that does not contain a drive letter is not a valid absolute program path."
        );
        for relative in [
            "acme.exe",
            "tools/acme.exe",
            "./acme.exe",
            "../acme.exe",
            "C:acme.exe",
            "",
            "   ",
        ] {
            assert!(!is_absolute_program(relative), "{relative}");
        }
    }

    /// Scratch dir for the downloaded-candidate tests.
    fn goose_scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "duthub-registry-goose-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn downloaded_goose_with_changed_bytes_is_unusable_not_fatal() {
        let dir = goose_scratch("swap");
        let exe = dir.join("goose.exe");
        std::fs::write(&exe, b"payload").unwrap();
        // Recorded digest from another build: bytes changed since install ->
        // unusable (fall through), not an error.
        let wrong = duthub_core::store::sha256_hex(b"other-bytes");
        assert!(matches!(verify_downloaded_goose(&exe, &wrong), Ok(None)));
        // Unreadable file counts as unusable too.
        let missing = dir.join("gone.exe");
        assert!(matches!(
            verify_downloaded_goose(&missing, &wrong),
            Ok(None)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downloaded_goose_with_matching_hash_but_wrong_program_fails_closed() {
        let dir = goose_scratch("policy");
        let exe = dir.join("goose.exe");
        std::fs::write(&exe, b"not a goose binary").unwrap();
        // Bytes match the install record exactly, but the binary does not
        // answer like goose: loud policy failure, no silent fallback.
        let recorded = duthub_core::store::sha256_hex(b"not a goose binary");
        assert!(matches!(
            verify_downloaded_goose(&exe, &recorded),
            Err(RegistryError::Version(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The released `goose.exe` needs the `acp` subcommand (bare launch enters the
    /// interactive configure flow); a dedicated `goose-acp.exe` must not get it.
    #[test]
    fn goose_launch_arguments_select_the_acp_subcommand() {
        let args = |program: &str, configured: &[&str]| {
            let configured: Vec<String> = configured
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            goose_acp_args(Path::new(program), &configured)
        };
        assert_eq!(args("C:/tools/goose/goose.exe", &[]), ["acp"]);
        assert_eq!(args("C:/tools/goose/GOOSE.EXE", &[]), ["acp"]);
        assert_eq!(
            args("C:/tools/goose/goose-acp.exe", &[]),
            Vec::<String>::new()
        );
        assert_eq!(
            args("C:/tools/goose/goose.exe", &["acp", "--verbose"]),
            ["acp", "--verbose"],
            "The explicitly configured args must be preserved as they are."
        );
    }

    /// Real-binary check of the launch chain: resolving a downloaded-style/`PATH`
    /// goose must produce the `acp` subcommand, otherwise the process drops into
    /// the interactive configure flow and the session never initializes.
    /// Runs only when `DUTHUB_TEST_GOOSE` names a real goose executable.
    #[test]
    fn builtin_goose_from_path_uses_the_acp_subcommand() {
        let Some(program) = std::env::var_os("DUTHUB_TEST_GOOSE").map(PathBuf::from) else {
            eprintln!("SKIP: DUTHUB_TEST_GOOSE is unset and no goose executable is available");
            return;
        };
        if !program.is_file() {
            eprintln!("SKIP: {} is not a file", program.display());
            return;
        }
        let definition = AgentDefinition::goose();
        let spec = resolve_builtin_from(&definition, Vec::new(), None, Some(program))
            .expect("The real goose must be able to parse it properly.");
        assert_eq!(
            spec.args,
            ["acp"],
            "The goose.exe program must be launched using the acp command."
        );
        assert!(
            spec.version
                .as_deref()
                .is_some_and(|line| !line.trim().is_empty()),
            "The version information must be obtained using the goose method. Please provide the specific version number: {:?}",
            spec.version
        );
    }

    /// The tool-downloads table must keep describing exactly what the resolver
    /// looks for: id `goose`, one executable named `goose.exe`.
    #[test]
    fn tooldown_spec_matches_the_goose_install_layout() {
        let spec = duthub_tooldown::spec_by_id(GOOSE_TOOL_ID)
            .expect("Goose must be listed in the tool download list.");
        assert_eq!(spec.id, GOOSE_TOOL_ID);
        assert_eq!(
            spec.entries
                .iter()
                .filter(|entry| entry.kind == duthub_tooldown::EntryKind::Executable)
                .map(|entry| entry.file_name)
                .collect::<Vec<_>>(),
            ["goose.exe"],
            "The parsing layer starts by executing the executable files reported by the installed kernel."
        );
        assert!(
            spec.url.contains("aaif-goose/goose/releases"),
            "The source from which the file was downloaded must be a valid upstream release: {}.",
            spec.url
        );
    }

    #[test]
    fn install_state_prioritises_the_downloaded_install_over_path() {
        let downloaded = (
            PathBuf::from("C:/config/tools/goose/goose.exe"),
            "0".repeat(64),
        );
        let path_program = PathBuf::from("C:/other/goose.exe");
        assert_eq!(
            goose_install_state_from(Some(downloaded.clone()), Some(path_program.clone())),
            GooseInstallState::Downloaded {
                path: downloaded.0,
                sha256: downloaded.1,
            },
            "The downloaded and installed files must be given priority over the PATH environment variables."
        );
        assert_eq!(
            goose_install_state_from(None, Some(path_program.clone())),
            GooseInstallState::Path { path: path_program }
        );
        let none = goose_install_state_from(None, None);
        assert_eq!(none, GooseInstallState::NotInstalled);
        assert!(none.needs_install());
        assert_eq!(none.program(), None);
        assert!(
            none.to_string().contains("工具下载"),
            "In the uninstalled state, the link must point to the tool download page: {none}"
        );
    }

    #[test]
    fn downloaded_candidate_is_consulted_before_path() {
        // Bytes match the install record but the program is not goose: the
        // failure must come from the downloaded candidate, proving it is tried
        // first (a PATH-first order would report the PATH path instead).
        let dir = goose_scratch("priority");
        let downloaded = dir.join("downloaded-goose.exe");
        std::fs::write(&downloaded, b"not a goose binary").unwrap();
        let recorded = duthub_core::store::sha256_hex(b"not a goose binary");
        let path_candidate = dir.join("path-goose.exe");
        let error = resolve_builtin_from(
            &AgentDefinition::goose(),
            Vec::new(),
            Some(&(downloaded.clone(), recorded)),
            Some(path_candidate.clone()),
        )
        .expect_err("The programs that use the same hash function but are not related to \"goose\" must be terminated with failure.");
        let text = error.to_string();
        assert!(matches!(error, RegistryError::Version(_)), "{text}");
        assert!(
            text.contains(&downloaded.display().to_string()),
            "You must first try the downloaded candidate: {text}."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_download_fails_closed_instead_of_falling_through_to_path() {
        let dir = goose_scratch("fail-closed");
        let downloaded = dir.join("downloaded-goose.exe");
        std::fs::write(&downloaded, b"payload").unwrap();
        let recorded = duthub_core::store::sha256_hex(b"other-bytes");
        let path_candidate = dir.join("path-goose.exe");
        let error = resolve_builtin_from(
            &AgentDefinition::goose(),
            Vec::new(),
            Some(&(downloaded, recorded)),
            Some(path_candidate.clone()),
        )
        .expect_err("hash mismatch on downloaded goose must fail closed");
        assert!(matches!(error, RegistryError::Program(_)), "{error}");
        let text = error.to_string();
        assert!(
            !text.contains(&path_candidate.display().to_string()),
            "must not fall through to unpinned PATH candidate: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_goose_reports_not_installed_instead_of_panicking() {
        let error = resolve_builtin_from(&AgentDefinition::goose(), Vec::new(), None, None)
            .expect_err("When there are no candidates available, the system must return to the displayable state.");
        assert!(matches!(error, RegistryError::NotInstalled(_)), "{error}");
        let text = error.to_string();
        assert!(text.contains("工具下载"), "{text}");
        assert!(text.contains("<config>/tools/goose/"), "{text}");
    }

    #[test]
    fn non_goose_builtin_still_requires_an_external_program() {
        let mut agent = AgentDefinition::goose();
        agent.id = "acme".into();
        let error = resolve_builtin_from(&agent, Vec::new(), None, None).unwrap_err();
        assert!(matches!(error, RegistryError::Program(_)), "{error}");
    }
}
