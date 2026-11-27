//! Unified device error categories aligned with `docs/API_CONTRACT.md`.

use thiserror::Error;

/// Canonical error categories for device, serial, relay, and tool operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeviceError {
    #[error("not found")]
    NotFound,
    #[error("resource busy")]
    Busy,
    #[error("permission denied")]
    PermissionDenied,
    #[error("device disconnected")]
    Disconnected,
    #[error("operation timed out")]
    Timeout,
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("unsupported action")]
    Unsupported,
    /// Control lease admission denial (GUI priority / exclusive control).
    #[error("lease denied: {0}")]
    Lease(String),
}

/// Unified `Result` alias for device operations.
pub type DeviceResult<T> = Result<T, DeviceError>;
