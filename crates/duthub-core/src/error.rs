//! 统一设备错误类别（与 `docs/API_CONTRACT.md` 的失败语义对齐）。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    NotFound,
    Busy,
    PermissionDenied,
    Disconnected,
    Timeout,
    Invalid(String),
    Unsupported,
    /// 控制租约复核拒绝（GUI 优先 / 独占控制，docs/SECURITY.md）；
    /// 携带面向用户的原因文案。
    Lease(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::NotFound => write!(f, "not found"),
            DeviceError::Busy => write!(f, "resource busy"),
            DeviceError::PermissionDenied => write!(f, "permission denied"),
            DeviceError::Disconnected => write!(f, "device disconnected"),
            DeviceError::Timeout => write!(f, "operation timed out"),
            DeviceError::Invalid(what) => write!(f, "invalid config: {what}"),
            DeviceError::Unsupported => write!(f, "unsupported action"),
            DeviceError::Lease(what) => write!(f, "lease denied: {what}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// 应用内统一 `Result` 别名。
pub type DeviceResult<T> = Result<T, DeviceError>;
