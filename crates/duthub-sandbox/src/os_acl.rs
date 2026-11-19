//! Private directory ACL hardening and OS isolation capability reporting.
//!
//! All Win32 FFI lives inside `crate::windows::dir_acl`; this facade stays
//! completely free of `unsafe` code (enforced by `tests/source_contract.rs`).

use std::path::{Path, PathBuf};

/// What OS-level isolation the current host can actually provide.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OsIsolationStatus {
    pub private_dir_acl: bool,
    pub restricted_token: bool,
    pub app_container: bool,
    /// Platform without an OS-isolation implementation.
    pub unsupported: bool,
    /// Human-readable summary (UI/transcript text).
    pub detail: String,
}

/// Directory DACL facts read back from the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateDirAcl {
    /// `SE_DACL_PROTECTED`: inherited ACEs were removed.
    pub protected: bool,
    /// Protected and free of broad principals (`Everyone` / `BUILTIN\Users`).
    pub private: bool,
    /// SID strings of the access-allowed ACEs, in DACL order.
    pub entries: Vec<String>,
}

/// ACL/token probe failure.
#[derive(Debug)]
pub enum AclError {
    /// The platform has no OS-isolation implementation.
    Unsupported,
    Io(String),
    /// The path is not a plain directory (missing, file, symlink or junction).
    InvalidPath(PathBuf),
    Win32 {
        call: &'static str,
        code: u32,
    },
    /// The API calls succeeded but the resulting ACL does not match expectations.
    Rejected(String),
}

impl std::fmt::Display for AclError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AclError::Unsupported => write!(formatter, "当前平台不支持 OS 隔离探测"),
            AclError::Io(error) => write!(formatter, "OS 隔离 I/O 失败: {error}"),
            AclError::Rejected(reason) => {
                write!(formatter, "OS 隔离授权未生效，已拒绝继续: {reason}")
            }
            AclError::InvalidPath(path) => {
                write!(formatter, "OS 隔离目标不是普通目录: {}", path.display())
            }
            AclError::Win32 { call, code } => write!(formatter, "{call} 失败（Win32 错误 {code}）"),
        }
    }
}

impl std::error::Error for AclError {}

/// Restrict `path` (a plain directory) to the current user, SYSTEM and
/// Administrators, with a protected DACL and inheritable ACEs.
pub fn harden_private_dir(path: &Path) -> Result<(), AclError> {
    #[cfg(windows)]
    {
        crate::windows::dir_acl::harden(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(AclError::Unsupported)
    }
}

/// Read the directory DACL back (tests/diagnostics).
pub fn inspect_private_dir_acl(path: &Path) -> Result<PrivateDirAcl, AclError> {
    #[cfg(windows)]
    {
        crate::windows::dir_acl::inspect(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(AclError::Unsupported)
    }
}

/// Probe restricted-token creation without spawning any process.
pub fn probe_restricted_token() -> Result<(), AclError> {
    #[cfg(windows)]
    {
        crate::windows::dir_acl::probe_restricted_token()
    }
    #[cfg(not(windows))]
    {
        Err(AclError::Unsupported)
    }
}

/// Describe the OS isolation currently available for `path`.
pub fn probe_os_isolation(path: &Path) -> OsIsolationStatus {
    #[cfg(windows)]
    {
        crate::windows::dir_acl::probe(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        OsIsolationStatus {
            unsupported: true,
            detail: "当前平台未实现 OS 隔离探测（仅 Windows 提供目录 ACL 与受限令牌）".to_owned(),
            ..OsIsolationStatus::default()
        }
    }
}
