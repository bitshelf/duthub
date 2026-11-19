//! Non-Windows backend: every entry point refuses, with an explicit reason.
//!
//! There is deliberately no `cfg`-free path that silently succeeds: the same
//! assertions that prove the Windows boundary also run on other platforms and
//! must observe [`SandboxError::Unsupported`] there (ADR-12 §7).

use std::time::Duration;

use crate::backend::{PreparedCell, SandboxBackend};
use crate::policy::{CellSpec, ExecRequest, ExecResult, SandboxError, SandboxPolicy, SpawnRequest};
use crate::status::SandboxStatus;
use crate::stream::{SandboxLimits, SandboxedProcess};

static UNSUPPORTED: UnsupportedBackend = UnsupportedBackend;

pub(crate) fn backend() -> &'static dyn SandboxBackend {
    &UNSUPPORTED
}

struct UnsupportedBackend;

const REASON: &str = "当前平台没有 OS 沙箱后端（仅 Windows 提供 WRITE_RESTRICTED 令牌与 AppContainer），命令一律拒绝执行";

impl SandboxBackend for UnsupportedBackend {
    fn platform(&self) -> &'static str {
        "unsupported"
    }

    fn status(&self) -> SandboxStatus {
        SandboxStatus::unsupported()
    }

    fn prepare(
        &self,
        _cell: &CellSpec,
        _policy: &SandboxPolicy,
    ) -> Result<PreparedCell, SandboxError> {
        Err(SandboxError::Unsupported(REASON))
    }

    fn launch(
        &self,
        _cell: &CellSpec,
        _policy: &SandboxPolicy,
        _request: &ExecRequest,
    ) -> Result<ExecResult, SandboxError> {
        Err(SandboxError::Unsupported(REASON))
    }

    fn spawn(
        &self,
        _cell: &CellSpec,
        _policy: &SandboxPolicy,
        _request: &SpawnRequest,
    ) -> Result<SandboxedProcess, SandboxError> {
        Err(SandboxError::Unsupported(REASON))
    }
}

/// Non-Windows process handle: no constructor exists, and every method
/// refuses. It exists so [`SandboxedProcess`] compiles without a `cfg` in its
/// public surface.
pub(crate) struct ProcessHandle;

impl ProcessHandle {
    pub(crate) fn try_wait(&self) -> std::io::Result<Option<i32>> {
        Err(unsupported_io())
    }

    pub(crate) fn wait_timeout(&self, _timeout: Duration) -> std::io::Result<Option<i32>> {
        Err(unsupported_io())
    }

    pub(crate) fn terminate(&self) -> std::io::Result<()> {
        Err(unsupported_io())
    }

    pub(crate) fn limits(&self) -> std::io::Result<SandboxLimits> {
        Err(unsupported_io())
    }
}

fn unsupported_io() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, REASON)
}
