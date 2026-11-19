//! The OS boundary: one trait, one implementation per platform.
//!
//! Callers reach the OS only through [`SandboxBackend`]; nothing above this
//! module knows about tokens, ACLs, AppContainers or Job Objects.

use std::path::PathBuf;

use crate::policy::{
    CellSpec, ExecRequest, ExecResult, IsolationMode, SandboxError, SandboxPolicy, SpawnRequest,
};
use crate::status::SandboxStatus;
use crate::stream::SandboxedProcess;

/// Kernel state established for a cell. Returned so callers (UI, audit) can
/// report the identities that were actually created and verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCell {
    pub scope: String,
    pub root: PathBuf,
    pub mode: IsolationMode,
    /// Random per-DUT capability SID (restricted-token mode), as `S-1-5-21-…`.
    pub capability_sid: Option<String>,
    /// AppContainer package SID (script mode), as `S-1-15-2-…`.
    pub container_sid: Option<String>,
    /// Paths whose read/write ACEs and labels were applied and read back.
    pub protected_paths: Vec<PathBuf>,
    /// `S-1-5-21-…` of the dedicated sandbox account the child runs as, when the
    /// one-time provisioning has been done. `None` means the child keeps the host
    /// user's identity, so reads are **not** separated.
    pub sandbox_identity_sid: Option<String>,
    /// Human-readable facts about what was verified (backend notes).
    pub notes: Vec<String>,
}

/// Runs one command under an OS-enforced sandbox.
///
/// `launch` blocks for up to the policy timeout and must be called off any
/// async executor thread. Implementations must be fail-closed: if the sandbox
/// cannot be established, return an error instead of running the command.
pub trait SandboxBackend: Send + Sync {
    /// Short platform label for status/UI (`windows`, `unsupported`).
    fn platform(&self) -> &'static str;

    /// What this host can actually enforce (probing may create and delete a
    /// throwaway profile/job; it never spawns a sandboxed command).
    fn status(&self) -> SandboxStatus;

    /// Establish (or re-verify) the cell's kernel state. Idempotent.
    fn prepare(
        &self,
        cell: &CellSpec,
        policy: &SandboxPolicy,
    ) -> Result<PreparedCell, SandboxError>;

    /// Prepare the cell (if needed) and run `request` inside it.
    fn launch(
        &self,
        cell: &CellSpec,
        policy: &SandboxPolicy,
        request: &ExecRequest,
    ) -> Result<ExecResult, SandboxError>;

    /// Prepare the cell (if needed) and start `request` as a long-lived
    /// process whose pipes stay owned by the caller.
    ///
    /// The token read-back is the same one [`Self::launch`] performs; a
    /// backend that cannot prove the child's token must terminate it and
    /// return [`SandboxError::NotSandboxed`] instead of a live handle.
    fn spawn(
        &self,
        cell: &CellSpec,
        policy: &SandboxPolicy,
        request: &SpawnRequest,
    ) -> Result<SandboxedProcess, SandboxError>;
}

/// Create the cell directories and refuse any path that resolves through a
/// reparse point.
///
/// The check runs *after* creation so a pre-planted junction cannot be used to
/// redirect the cell's ACLs onto an unrelated tree: `create_dir_all` succeeds
/// through a junction, and the metadata check then sees the reparse bit.
pub(crate) fn ensure_cell_dirs(cell: &CellSpec) -> Result<(), SandboxError> {
    std::fs::create_dir_all(cell.root()).map_err(|error| {
        SandboxError::Setup(format!(
            "无法创建沙箱单元 {}: {error}",
            cell.root().display()
        ))
    })?;
    for dir in cell.required_dirs() {
        std::fs::create_dir_all(&dir).map_err(|error| {
            SandboxError::Setup(format!("无法创建沙箱目录 {}: {error}", dir.display()))
        })?;
    }
    for dir in std::iter::once(cell.root().to_path_buf()).chain(cell.required_dirs()) {
        ensure_plain_directory(&dir)?;
    }
    Ok(())
}

/// The path exists, is a directory, and is not a reparse point.
pub(crate) fn ensure_plain_directory(path: &std::path::Path) -> Result<(), SandboxError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        SandboxError::Setup(format!("沙箱目录不可用 {}: {error}", path.display()))
    })?;
    if !metadata.is_dir() || is_reparse_point(&metadata) {
        return Err(SandboxError::Setup(format!(
            "沙箱路径不是普通目录（拒绝符号链接/联接点）: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_directory_is_rejected() {
        let dir = std::env::temp_dir().join(format!("duthub-sbx-{}", uuid::Uuid::new_v4()));
        assert!(ensure_plain_directory(&dir).is_err());
        std::fs::create_dir_all(&dir).unwrap();
        assert!(ensure_plain_directory(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cell_directories_are_created_once_and_idempotently() {
        let work = std::env::temp_dir().join(format!("duthub-sbx-{}", uuid::Uuid::new_v4()));
        let cell = CellSpec::for_dut(&work, "dut-cell").unwrap();
        ensure_cell_dirs(&cell).unwrap();
        ensure_cell_dirs(&cell).unwrap();
        assert!(cell.root().is_dir());
        for dir in cell.required_dirs() {
            assert!(dir.is_dir(), "{}", dir.display());
        }
        let _ = std::fs::remove_dir_all(&work);
    }
}
