//! Long-lived sandboxed processes (the Agent host's ACP session).
//!
//! One-shot commands use [`crate::run`] / [`crate::run_agent`]: the stdin
//! payload is fixed, the process must finish inside the policy timeout, and
//! the output is returned after exit. An ACP session cannot work that way — it
//! needs a writable stdin and readable stdout/stderr for as long as the
//! session lives.
//!
//! [`SandboxedProcess`] is that second lifecycle. The token, Job limits and
//! token read-back are exactly the ones a one-shot launch uses; only the pipe
//! ownership moves to the caller. There is no timeout: the host's stop path or
//! `Drop` ends the session, and `KILL_ON_JOB_CLOSE` is the last-resort tree
//! kill when the handle is closed without an explicit terminate.
//!
//! The constructor is crate-private — callers reach it through
//! [`crate::spawn_streaming`], so the policy is validated before any OS state
//! exists.

use std::fs::File;
use std::io;
use std::time::Duration;

#[cfg(not(windows))]
use crate::unsupported::ProcessHandle;
#[cfg(windows)]
use crate::windows::process::ProcessHandle;

/// How long `Drop` waits for the tree to disappear after `TerminateJobObject`.
const DROP_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Limits that were queried back from the Job Object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLimits {
    pub active_process_limit: u32,
    pub process_memory_limit: u64,
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is set.
    pub kill_on_close: bool,
}

/// A running sandboxed process whose pipes are owned by the caller.
///
/// Drop terminates the whole Job tree; call [`SandboxedProcess::terminate`]
/// first when the exit status still has to be observed.
pub struct SandboxedProcess {
    pid: u32,
    stdin: Option<File>,
    stdout: Option<File>,
    stderr: Option<File>,
    handle: ProcessHandle,
}

impl SandboxedProcess {
    pub(crate) fn new(
        pid: u32,
        stdin: File,
        stdout: File,
        stderr: File,
        handle: ProcessHandle,
    ) -> Self {
        Self {
            pid,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
            handle,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Take the child's stdin. Dropping it closes the pipe (the child sees EOF).
    pub fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }

    /// Take the child's stdout. Dropping it discards a copy of the handle.
    pub fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }

    /// Take the child's stderr. Dropping it discards a copy of the handle.
    pub fn take_stderr(&mut self) -> Option<File> {
        self.stderr.take()
    }

    /// Exit code, or `None` while the process is still running.
    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        self.handle.try_wait()
    }

    /// Wait up to `timeout` for the process to exit.
    pub fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<i32>> {
        self.handle.wait_timeout(timeout)
    }

    /// Terminate the whole process tree (`TerminateJobObject`). Does not close
    /// the Job handle, so a later `try_wait` can still observe the exit code.
    pub fn terminate(&self) -> io::Result<()> {
        self.handle.terminate()
    }

    /// Limits read back from the kernel, for status/diagnostics.
    pub fn limits(&self) -> io::Result<SandboxLimits> {
        self.handle.limits()
    }
}

impl std::fmt::Debug for SandboxedProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SandboxedProcess")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl Drop for SandboxedProcess {
    fn drop(&mut self) {
        // Closing the last Job handle would kill the tree asynchronously; an
        // explicit terminate makes "Drop = tree gone" deterministic, then the
        // bounded wait keeps the dropping thread from lingering.
        let _ = self.handle.terminate();
        let _ = self.handle.wait_timeout(DROP_REAP_TIMEOUT);
    }
}
