//! Per-DUT Agent directory layout and unified MXC sandbox preparation.
//!
//! Directory tree: `<work_dir>/agent/dut/<hex(dut_id)>/`
//! - `goose/`          -> `GOOSE_PATH_ROOT` (`config/`, `data/sessions/`)
//! - `sandbox/`        -> container working directory and primary writable cell
//! - `sandbox/tmp/`    -> child `TEMP` / `TMP` (never inherits host `%TEMP%`)
//! - `sandbox/home/`   -> child private `LOCALAPPDATA` / `APPDATA` root
//! - `transcript.jsonl`
//! - `session-id` (legacy resume marker)
//! - `resume.json` (`agentId` + `programSha256` + `sessionId`)
//!
//! All OS containment (filesystem grants/denials, directional network policy,
//! UI isolation, and tree termination) is delegated to `duthub-sandbox`
//! (`mxc-sdk`).

use std::path::{Path, PathBuf};

use duthub_core::store;
use duthub_sandbox::{CellSpec, NetworkPolicy, SandboxPolicy, SandboxStatus, prepare_cell};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    pub dut_dir: PathBuf,
    pub sandbox: PathBuf,
    /// Per-session `TEMP`/`TMP`: `<sandbox>/tmp`.
    pub temp: PathBuf,
    pub goose_root: PathBuf,
    pub transcript: PathBuf,
    pub session_id: PathBuf,
    /// Structured resume marker (`agentId` + `programSha256` + `sessionId`).
    pub resume: PathBuf,
}

/// Prepared MXC sandbox state shared by both ACP and CLI workers.
#[derive(Debug, Clone)]
pub struct PreparedAgentSandbox {
    pub cell: CellSpec,
    pub policy: SandboxPolicy,
    pub status: SandboxStatus,
}

#[derive(Debug)]
pub enum SandboxError {
    InvalidDutId,
    Io(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::InvalidDutId => write!(formatter, "DUT id is empty"),
            SandboxError::Io(error) => write!(formatter, "Agent sandbox directory error: {error}"),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Create and validate the per-DUT directory layout via `cap-std`.
pub fn prepare(work_dir: &Path, dut_id: &str) -> Result<AgentPaths, SandboxError> {
    if dut_id.trim().is_empty() {
        return Err(SandboxError::InvalidDutId);
    }
    let dut_dir = store::agent_dut_dir(work_dir, dut_id)
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let sandbox = store::ensure_agent_dir(&dut_dir, &["sandbox"])
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let temp = temp_dir(&sandbox)?;
    let goose_root = store::ensure_agent_dir(&dut_dir, &["goose"])
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    Ok(AgentPaths {
        transcript: dut_dir.join("transcript.jsonl"),
        session_id: dut_dir.join("session-id"),
        resume: dut_dir.join("resume.json"),
        dut_dir,
        sandbox,
        temp,
        goose_root,
    })
}

/// Create and validate `<sandbox>/tmp` as a plain (non-reparse) directory.
pub fn temp_dir(sandbox: &Path) -> Result<PathBuf, SandboxError> {
    let dir = store::ensure_agent_dir(sandbox, &["tmp"])
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let metadata =
        std::fs::symlink_metadata(&dir).map_err(|error| SandboxError::Io(error.to_string()))?;
    if !metadata.is_dir() || is_redirect(&metadata) {
        return Err(SandboxError::Io(format!(
            "TEMP path is not a plain directory: {}",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Verify host MXC capability, establish the cell directories, and build the
/// per-DUT [`SandboxPolicy`] (fail-closed if MXC containment is unavailable).
pub fn prepare_agent_sandbox(
    work_dir: &Path,
    dut_id: &str,
    paths: &AgentPaths,
    allowed_exe_dir: Option<&Path>,
    extra_writable: &[&Path],
    extra_readable: &[&Path],
    network: NetworkPolicy,
) -> Result<PreparedAgentSandbox, String> {
    let status = duthub_sandbox::status();
    if !status.supports_agent_isolation() {
        return Err(format!(
            "OS sandbox backend is unavailable or missing required isolation capabilities ({})",
            status.summary()
        ));
    }
    let cell = CellSpec::for_dut(work_dir, dut_id)
        .map_err(|error| format!("invalid Agent sandbox cell: {error}"))?;
    if cell.root() != paths.sandbox {
        return Err(format!(
            "Agent sandbox cell mismatch: {} != {}",
            cell.root().display(),
            paths.sandbox.display()
        ));
    }
    prepare_cell(&cell)
        .map_err(|error| format!("failed to prepare Agent sandbox cell: {error}"))?;

    let mut policy = SandboxPolicy::agent_cell(&cell, allowed_exe_dir);
    policy.network = network;
    for dir in extra_writable {
        if dir.is_dir() && *dir != cell.root() {
            policy.writable.push(dir.to_path_buf());
        }
    }
    for dir in extra_readable {
        if dir.is_dir() && *dir != cell.root() {
            policy.readable.push(dir.to_path_buf());
        }
    }
    // Remove any denied path that is an ancestor of an explicitly granted root.
    policy.deny_read.retain(|denied| {
        !policy
            .writable
            .iter()
            .chain(&policy.readable)
            .any(|allowed| allowed == denied || allowed.starts_with(denied))
    });
    duthub_sandbox::validate_policy(&policy, cell.root())
        .map_err(|error| format!("Agent sandbox policy rejected: {error}"))?;

    Ok(PreparedAgentSandbox {
        cell,
        policy,
        status,
    })
}

#[cfg(windows)]
fn is_redirect(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_redirect(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_work_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("duthub-sandbox-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prepares_versioned_per_dut_layout() {
        let work = temp_work_dir("layout");
        let paths = prepare(&work, "dut-A").unwrap();
        assert!(paths.sandbox.is_dir());
        assert!(paths.goose_root.is_dir());
        assert!(paths.temp.is_dir());
        assert_eq!(paths.temp, paths.sandbox.join("tmp"));
        let key = store::agent_dut_key("dut-A").unwrap();
        assert!(paths.dut_dir.ends_with(format!("agent/dut/{key}")));
        assert!(paths.transcript.ends_with("transcript.jsonl"));
        assert!(paths.session_id.ends_with("session-id"));
        assert!(paths.resume.ends_with("resume.json"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn temp_dir_rejects_non_directory() {
        let work = temp_work_dir("temp-file");
        let paths = prepare(&work, "dut-temp").unwrap();
        std::fs::remove_dir(&paths.temp).unwrap();
        std::fs::write(&paths.temp, b"not a directory").unwrap();
        assert!(matches!(temp_dir(&paths.sandbox), Err(SandboxError::Io(_))));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn rejects_empty_dut_id() {
        let work = temp_work_dir("empty");
        assert!(matches!(
            prepare(&work, "   "),
            Err(SandboxError::InvalidDutId)
        ));
        let _ = std::fs::remove_dir_all(&work);
    }
}
