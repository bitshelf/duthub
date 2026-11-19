//! Codex-style workspace scope and path access policy for Agent execution.
//!
//! All verdicts compose under a strictest-wins lattice (`Forbidden > Prompt > Allow`):
//! a pipeline or compound script is only as permissive as its most dangerous
//! command or path target.

use std::path::{Component, Path, PathBuf};

use duthub_core::path_guard;

/// Filesystem/process isolation level requested for an Agent execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// No filesystem writes anywhere (even inside the active DUT directory).
    ReadOnly,
    /// Writes allowed strictly inside the active DUT directory; sibling DUT
    /// directories remain read-only; sensitive host/config paths are forbidden.
    DutWorkspaceWrite,
    /// Explicit operator escape hatch (still enforces protected-path blocks
    /// unless overridden at a higher layer).
    DangerFullAccess,
}

/// Approval posture paired with [`SandboxMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Prompt on every non-allowlisted or dynamic operation.
    Untrusted,
    /// Auto-allow policy-approved operations; prompt on ambiguous/dynamic ones.
    OnRequest,
    /// Never prompt interactively (non-interactive runs fail closed on Prompt).
    Never,
}

/// Whether a path access intends to read or mutate the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
}

/// Three-way execution decision following OpenAI Codex's policy lattice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Prompt { reason: String },
    Forbidden { reason: String },
}

impl Verdict {
    /// Strictest-wins composition: `Forbidden > Prompt > Allow`.
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (first @ Self::Forbidden { .. }, _) => first,
            (_, second @ Self::Forbidden { .. }) => second,
            (first @ Self::Prompt { .. }, _) => first,
            (_, second @ Self::Prompt { .. }) => second,
            (Self::Allow, Self::Allow) => Self::Allow,
        }
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    pub fn is_forbidden(&self) -> bool {
        matches!(self, Self::Forbidden { .. })
    }
}

/// Canonical filesystem scope for one DUT Agent session.
#[derive(Debug, Clone)]
pub struct WorkspaceScope {
    /// Active DUT directory: read-write in [`SandboxMode::DutWorkspaceWrite`].
    pub active_dut_dir: PathBuf,
    /// Sibling DUT directories: read-only (cross-DUT analysis), NEVER writable.
    pub sibling_dut_dirs: Vec<PathBuf>,
    /// Additional explicitly granted read-only roots (e.g., shared firmware docs).
    pub extra_read_dirs: Vec<PathBuf>,
    /// Protected roots denied for BOTH reads and writes (`.duthub/`, credentials, etc.).
    pub protected_denied_dirs: Vec<PathBuf>,
}

impl WorkspaceScope {
    /// Build a scope anchored at `active_dut_dir` with default protected host
    /// directories (`.duthub`, user credential folders, and system roots).
    pub fn new(active_dut_dir: impl Into<PathBuf>) -> Self {
        let active = canonical_or_lexical(&active_dut_dir.into());
        Self {
            active_dut_dir: active,
            sibling_dut_dirs: Vec::new(),
            extra_read_dirs: Vec::new(),
            protected_denied_dirs: default_protected_paths(),
        }
    }

    /// Add a read-only sibling DUT directory (never writable).
    pub fn with_sibling_dut(mut self, dir: impl Into<PathBuf>) -> Self {
        self.sibling_dut_dirs
            .push(canonical_or_lexical(&dir.into()));
        self
    }

    /// Add a protected directory or file denied for both read and write.
    pub fn with_protected_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.protected_denied_dirs
            .push(canonical_or_lexical(&path.into()));
        self
    }
}

/// Default protected directories denied to Agent scripts (config, secrets, OS).
pub fn default_protected_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        paths.push(canonical_or_lexical(&dir.join(".duthub")));
    }
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        let home = PathBuf::from(home);
        for rel in [".ssh", ".aws", ".kube", ".docker", ".gitconfig", ".gnupg"] {
            paths.push(canonical_or_lexical(&home.join(rel)));
        }
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        paths.push(canonical_or_lexical(&PathBuf::from(appdata).join("duthub")));
    }
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        paths.push(canonical_or_lexical(
            &PathBuf::from(local_appdata).join("duthub"),
        ));
    }
    paths
}

/// Evaluate whether accessing `raw_path` with `access` is permitted under
/// `scope` and `mode`.
pub fn evaluate_path_access(
    scope: &WorkspaceScope,
    raw_path: &str,
    access: AccessKind,
    mode: SandboxMode,
) -> Verdict {
    let trimmed = raw_path.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() {
        return Verdict::Prompt {
            reason: "empty path argument requires confirmation".to_owned(),
        };
    }
    if trimmed.chars().any(char::is_control) {
        return Verdict::Forbidden {
            reason: format!("path contains control characters: {trimmed:?}"),
        };
    }
    if path_guard::is_unc_or_device_path(trimmed) {
        return Verdict::Forbidden {
            reason: format!("UNC or Windows device path is forbidden: {trimmed:?}"),
        };
    }
    if has_disallowed_provider_prefix(trimmed) {
        return Verdict::Forbidden {
            reason: format!("PowerShell provider path is forbidden: {trimmed:?}"),
        };
    }
    // Check NTFS Alternate Data Streams (`:` anywhere except a leading drive letter like `C:`).
    let after_drive = strip_windows_drive_prefix(trimmed);
    if path_guard::has_windows_ads(after_drive) {
        return Verdict::Forbidden {
            reason: format!("NTFS Alternate Data Stream path is forbidden: {trimmed:?}"),
        };
    }
    // Reject Windows reserved device names in any path component.
    for segment in after_drive.split(['/', '\\']) {
        if !segment.is_empty() && path_guard::is_windows_reserved_name(segment) {
            return Verdict::Forbidden {
                reason: format!("Windows reserved device name is forbidden: {trimmed:?}"),
            };
        }
    }

    // Resolve relative paths against active_dut_dir, then canonicalize any
    // existing ancestor so symlinks/junctions cannot bypass containment.
    let candidate_raw = Path::new(trimmed);
    let anchored = if candidate_raw.is_absolute() {
        candidate_raw.to_path_buf()
    } else if has_drive_relative_prefix(trimmed) {
        return Verdict::Forbidden {
            reason: format!("drive-relative Windows path is forbidden: {trimmed:?}"),
        };
    } else {
        scope.active_dut_dir.join(candidate_raw)
    };

    if has_reparse_ancestor(&anchored) {
        return Verdict::Forbidden {
            reason: format!("path traverses a symlink or NTFS reparse point: {trimmed:?}"),
        };
    }

    let resolved = resolve_deepest_existing(&anchored);

    // Protected paths are ALWAYS forbidden for both reads and writes.
    if is_protected_path(scope, &resolved) {
        return Verdict::Forbidden {
            reason: format!(
                "access to protected path is forbidden: {}",
                resolved.display()
            ),
        };
    }

    match access {
        AccessKind::Write => match mode {
            SandboxMode::ReadOnly => Verdict::Forbidden {
                reason: format!(
                    "write access to {} is forbidden in ReadOnly mode",
                    resolved.display()
                ),
            },
            SandboxMode::DutWorkspaceWrite => {
                if path_within(&resolved, &scope.active_dut_dir) {
                    Verdict::Allow
                } else if scope
                    .sibling_dut_dirs
                    .iter()
                    .any(|dir| path_within(&resolved, dir))
                {
                    Verdict::Forbidden {
                        reason: format!(
                            "writes to sibling DUT directory are forbidden: {}",
                            resolved.display()
                        ),
                    }
                } else {
                    Verdict::Forbidden {
                        reason: format!(
                            "writes outside active DUT workspace are forbidden: {}",
                            resolved.display()
                        ),
                    }
                }
            }
            SandboxMode::DangerFullAccess => Verdict::Allow,
        },
        AccessKind::Read => {
            if path_within(&resolved, &scope.active_dut_dir)
                || scope
                    .sibling_dut_dirs
                    .iter()
                    .any(|dir| path_within(&resolved, dir))
                || scope
                    .extra_read_dirs
                    .iter()
                    .any(|dir| path_within(&resolved, dir))
            {
                Verdict::Allow
            } else {
                Verdict::Prompt {
                    reason: format!(
                        "read outside active/sibling DUT workspace requires approval: {}",
                        resolved.display()
                    ),
                }
            }
        }
    }
}

fn is_protected_path(scope: &WorkspaceScope, resolved: &Path) -> bool {
    if scope
        .protected_denied_dirs
        .iter()
        .any(|denied| path_within(resolved, denied))
    {
        return true;
    }
    // Always block any `.duthub` directory or `agents.json` / `settings.json`
    // config store regardless of install location.
    for component in resolved.components() {
        if let Component::Normal(part) = component {
            let text = part.to_string_lossy();
            if text.eq_ignore_ascii_case(".duthub") {
                return true;
            }
        }
    }
    if let Some(name) = resolved.file_name().and_then(|n| n.to_str())
        && (name.eq_ignore_ascii_case("agents.json")
            || name.eq_ignore_ascii_case("agent-bindings.json"))
    {
        return true;
    }
    false
}

/// Detect PowerShell provider prefixes such as `HKLM:\`, `HKCU:\`, `Registry::`,
/// `Cert:\`, `WSMan:\`, `Env:\`, `Alias:\`, `Function:\`.
pub fn has_disallowed_provider_prefix(raw: &str) -> bool {
    let lower = raw.trim().to_ascii_lowercase();
    const PROVIDERS: &[&str] = &[
        "hklm:",
        "hkcu:",
        "hkcr:",
        "hku:",
        "hkcc:",
        "registry::",
        "cert:",
        "wsman:",
        "env:",
        "alias:",
        "function:",
        "variable:",
        "microsoft.powershell.core\\",
    ];
    PROVIDERS.iter().any(|prefix| lower.starts_with(prefix))
}

fn strip_windows_drive_prefix(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        &raw[2..]
    } else {
        raw
    }
}

fn has_drive_relative_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !matches!(bytes.get(2), Some(b'\\' | b'/'))
}

fn has_reparse_ancestor(path: &Path) -> bool {
    let mut current = Some(path);
    while let Some(node) = current {
        if node.exists() && path_guard::is_reparse_point(node).unwrap_or(true) {
            return true;
        }
        current = node.parent();
    }
    false
}

/// Canonicalize the deepest existing ancestor and append the remaining
/// lexically normalized tail so non-existent write targets still resolve
/// reliably without skipping symlink checks on parent directories.
fn resolve_deepest_existing(path: &Path) -> PathBuf {
    let lexical = normalize_lexical(path);
    let mut existing = lexical.as_path();
    let mut tail = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return lexical;
        };
        tail.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return lexical;
        };
        existing = parent;
    }
    let mut base = std::fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    for part in tail.into_iter().rev() {
        base.push(part);
    }
    strip_verbatim_prefix(&base)
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        strip_verbatim_prefix(&canon)
    } else {
        normalize_lexical(path)
    }
}

fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(stripped) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                out.pop();
            }
        }
    }
    out
}

/// Case-insensitive component-wise containment check on Windows, exact on Unix.
pub fn path_within(candidate: &Path, root: &Path) -> bool {
    let candidate_clean = strip_verbatim_prefix(candidate);
    let root_clean = strip_verbatim_prefix(root);
    let mut cand = candidate_clean.components();
    for expected in root_clean.components() {
        let Some(actual) = cand.next() else {
            return false;
        };
        #[cfg(windows)]
        let matched = actual
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy());
        #[cfg(not(windows))]
        let matched = actual == expected;
        if !matched {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_composition_is_strictest_wins() {
        assert_eq!(Verdict::Allow.combine(Verdict::Allow), Verdict::Allow);
        let prompt = Verdict::Prompt {
            reason: "p".to_owned(),
        };
        let forbidden = Verdict::Forbidden {
            reason: "f".to_owned(),
        };
        assert_eq!(Verdict::Allow.combine(prompt.clone()), prompt);
        assert_eq!(prompt.clone().combine(Verdict::Allow), prompt);
        assert_eq!(prompt.combine(forbidden.clone()), forbidden);
        assert_eq!(forbidden.clone().combine(Verdict::Allow), forbidden);
    }

    #[test]
    fn workspace_scope_enforces_active_rw_sibling_ro_and_protected_deny() {
        let tmp = tempfile::tempdir().unwrap();
        let dut_a = tmp.path().join("dut-a");
        let dut_b = tmp.path().join("dut-b");
        let duthub_cfg = tmp.path().join(".duthub");
        std::fs::create_dir_all(&dut_a).unwrap();
        std::fs::create_dir_all(&dut_b).unwrap();
        std::fs::create_dir_all(&duthub_cfg).unwrap();

        let scope = WorkspaceScope::new(&dut_a)
            .with_sibling_dut(&dut_b)
            .with_protected_path(&duthub_cfg);

        // Active DUT: read & write allowed in DutWorkspaceWrite; write forbidden in ReadOnly.
        assert_eq!(
            evaluate_path_access(
                &scope,
                "boot.log",
                AccessKind::Read,
                SandboxMode::DutWorkspaceWrite
            ),
            Verdict::Allow
        );
        assert_eq!(
            evaluate_path_access(
                &scope,
                "report.md",
                AccessKind::Write,
                SandboxMode::DutWorkspaceWrite
            ),
            Verdict::Allow
        );
        assert!(
            evaluate_path_access(
                &scope,
                "report.md",
                AccessKind::Write,
                SandboxMode::ReadOnly
            )
            .is_forbidden()
        );

        // Sibling DUT: read allowed, write FORBIDDEN.
        let sibling_file = dut_b.join("serial.log");
        assert_eq!(
            evaluate_path_access(
                &scope,
                &sibling_file.display().to_string(),
                AccessKind::Read,
                SandboxMode::DutWorkspaceWrite
            ),
            Verdict::Allow
        );
        assert!(
            evaluate_path_access(
                &scope,
                &sibling_file.display().to_string(),
                AccessKind::Write,
                SandboxMode::DutWorkspaceWrite
            )
            .is_forbidden()
        );

        // `.duthub/settings.json`: both read & write FORBIDDEN.
        let secret = duthub_cfg.join("settings.json");
        assert!(
            evaluate_path_access(
                &scope,
                &secret.display().to_string(),
                AccessKind::Read,
                SandboxMode::DutWorkspaceWrite
            )
            .is_forbidden()
        );
        assert!(
            evaluate_path_access(
                &scope,
                r"..\.duthub\settings.json",
                AccessKind::Read,
                SandboxMode::DutWorkspaceWrite
            )
            .is_forbidden()
        );

        // UNC, ADS, reserved device name, registry provider: all FORBIDDEN.
        for bad in [
            r"\\server\share\file.txt",
            "boot.log:hidden",
            "CON.txt",
            r"HKCU:\Software\Test",
        ] {
            assert!(
                evaluate_path_access(
                    &scope,
                    bad,
                    AccessKind::Read,
                    SandboxMode::DutWorkspaceWrite
                )
                .is_forbidden(),
                "expected forbidden for {bad}"
            );
        }
    }
}
