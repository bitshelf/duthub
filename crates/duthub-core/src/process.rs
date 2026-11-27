//! Safe local process execution (`docs/SECURITY.md` #1).
//!
//! - Uses `Command::new(resolved).args(argv)` only; never concatenates shell command strings.
//! - Resolves bare executable names on PATH to absolute paths (on Windows, only extensionless or
//!   `.exe` binaries are accepted; `.bat`/`.cmd` implicit shell execution is forbidden).
//! - Caps captured stdout/stderr and terminates the full **process tree** on timeout (children
//!   may spawn grandchildren).
//! - Bounds output draining: if a lingering grandchild still holds an inherited pipe, reading does
//!   not block indefinitely and returns `DeviceError::Timeout` after `DRAIN_TIMEOUT`.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::backend::ProcessResult;
use crate::error::{DeviceError, DeviceResult};

const MAX_OUTPUT: usize = 65_536;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Upper bound on waiting for output pipes to hit EOF after the main process exits, preventing
/// reader threads from blocking indefinitely when a grandchild inherits the pipes.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Process execution abstraction: injected with a mock in tests and [`RealProcessRunner`] in
/// production.
pub trait ProcessRunner: Send + Sync {
    fn run(&self, exe: &Path, args: &[String], timeout: Duration) -> DeviceResult<ProcessResult>;
}

/// Real process runner: argv-only without a shell, using the executable's parent directory as cwd.
#[derive(Debug)]
pub struct RealProcessRunner;

impl ProcessRunner for RealProcessRunner {
    #[tracing::instrument(name = "process.run", skip(self, args), fields(exe = %exe.display(), timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)))]
    fn run(&self, exe: &Path, args: &[String], timeout: Duration) -> DeviceResult<ProcessResult> {
        let output = run_capped(exe, args, &[], timeout, MAX_OUTPUT)?;
        Ok(ProcessResult {
            exit_code: Some(output.exit_code),
            stdout: display_output(&output.stdout),
            stderr: display_output(&output.stderr),
            elapsed_ms: output.elapsed_ms,
        })
    }
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

struct CommandOutput {
    stdout: Captured,
    stderr: Captured,
    exit_code: i32,
    elapsed_ms: u64,
}

/// Read a command's stdout without text conversion. Stderr stays separately bounded.
pub fn run_binary(
    exe: &Path,
    args: &[String],
    timeout: Duration,
    limit: usize,
) -> DeviceResult<Vec<u8>> {
    run_binary_with_env(exe, args, &[], timeout, limit)
}

/// [`run_binary`] with an explicit environment overlay.
///
/// The child still inherits the parent environment; `env` entries override it.
/// Used by trusted probes that must keep the probed program from writing into
/// the user profile (for example goose logs every CLI invocation under
/// `GOOSE_PATH_ROOT`, and the version probe is our invocation, not the agent's).
pub fn run_binary_with_env(
    exe: &Path,
    args: &[String],
    env: &[(OsString, OsString)],
    timeout: Duration,
    limit: usize,
) -> DeviceResult<Vec<u8>> {
    let output = run_capped(exe, args, env, timeout, limit)?;
    if output.exit_code != 0 {
        return Err(DeviceError::Invalid(format!(
            "exit {}: {}",
            output.exit_code,
            display_output(&output.stderr).trim()
        )));
    }
    if output.stdout.truncated {
        return Err(DeviceError::Invalid(
            "binary output exceeded its size limit".into(),
        ));
    }
    Ok(output.stdout.bytes)
}

fn run_capped(
    exe: &Path,
    args: &[String],
    env: &[(OsString, OsString)],
    timeout: Duration,
    stdout_limit: usize,
) -> DeviceResult<CommandOutput> {
    let mut child = background_command(exe)
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        .spawn()
        .map_err(|e| DeviceError::Invalid(format!("启动失败: {e}")))?;

    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    // Reader threads return results over a channel so the main thread can drain with
    // `recv_timeout` rather than `join` (a lingering grandchild holding inherited pipes would
    // cause `join` to hang forever).
    let (tx, rx) = std::sync::mpsc::channel::<(Stream, Captured)>();
    let tx_err = tx.clone();
    let stdout_reader = std::thread::spawn(move || {
        let _ = tx.send((Stream::Stdout, read_capped(stdout_pipe, stdout_limit)));
    });
    let stderr_reader = std::thread::spawn(move || {
        let _ = tx_err.send((Stream::Stderr, read_capped(stderr_pipe, MAX_OUTPUT)));
    });

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| DeviceError::Invalid(e.to_string()))?
        {
            break status;
        }
        if started.elapsed() > timeout {
            // Cancel on timeout by terminating the entire process tree (killing only the direct
            // child leaves spawned grandchildren holding the output pipes open).
            kill_process_tree(&mut child);
            timed_out = true;
            break child
                .wait()
                .map_err(|e| DeviceError::Invalid(e.to_string()))?;
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    drop(stdout_reader);
    drop(stderr_reader);
    if timed_out {
        return Err(DeviceError::Timeout);
    }
    // Bounded output drain: after a normal exit, pipe write ends may still be held by lingering
    // descendants; return `Timeout` rather than blocking the caller if both EOFs do not arrive.
    let (stdout, stderr) = collect_outputs(&rx);
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        return Err(DeviceError::Timeout);
    };

    Ok(CommandOutput {
        exit_code: status.code().unwrap_or(-1),
        stdout,
        stderr,
        elapsed_ms,
    })
}

fn background_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

#[derive(Debug, Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Collect both output streams within `DRAIN_TIMEOUT`; returns `None` for any stream that does not
/// finish before the deadline.
fn collect_outputs(
    rx: &std::sync::mpsc::Receiver<(Stream, Captured)>,
) -> (Option<Captured>, Option<Captured>) {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    let mut stdout = None;
    let mut stderr = None;
    while stdout.is_none() || stderr.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((Stream::Stdout, text)) => stdout = Some(text),
            Ok((Stream::Stderr, text)) => stderr = Some(text),
            Err(
                std::sync::mpsc::RecvTimeoutError::Timeout
                | std::sync::mpsc::RecvTimeoutError::Disconnected,
            ) => break,
        }
    }
    (stdout, stderr)
}

/// Terminate the process tree: on Windows uses `taskkill /T /F` (argv-only, no shell); if the main
/// process has already exited and the tree can no longer be enumerated, the caller's drain timeout
/// acts as a fallback. On Unix kills the main child process. Always calls `wait()` to reap zombies.
pub fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let pid = child.id();
        let mut cmd = background_command("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.status();
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn read_capped<R: Read>(mut reader: R, cap: usize) -> Captured {
    let mut output = Captured::default();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let copied = n.min(cap - output.bytes.len());
                output.bytes.extend_from_slice(&chunk[..copied]);
                output.truncated |= copied != n;
            }
        }
    }
    output
}

fn display_output(output: &Captured) -> String {
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    if output.truncated {
        text.push_str("\n[输出已截断]");
    }
    text
}

/// Resolve a bare executable name (such as `adb` or `upgrade_tool`) or an explicit path into a
/// concrete executable path.
///
/// Bare names search PATH only. On Windows, only extensionless binaries and `.exe` are accepted;
/// `.bat`/`.cmd` scripts require a shell interpreter and are rejected.
pub fn resolve_executable(spec: &str) -> DeviceResult<PathBuf> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(DeviceError::Invalid("可执行文件为空".into()));
    }
    if trimmed.len() > 2048 || trimmed.contains(['\r', '\n', '\0']) {
        return Err(DeviceError::Invalid("可执行文件路径非法".into()));
    }
    // `.bat`/`.cmd`/`.com` require a shell interpreter, which violates the argv-only boundary.
    let ext = PathBuf::from(trimmed)
        .extension()
        .map(|e| e.to_ascii_lowercase().to_string_lossy().to_string());
    if matches!(ext.as_deref(), Some("bat" | "cmd" | "com")) {
        return Err(DeviceError::Invalid(format!(
            "不允许 .{} 脚本（需要 shell 解释器）",
            ext.unwrap_or_default()
        )));
    }
    let candidate = PathBuf::from(trimmed);
    let has_dir_part = candidate
        .parent()
        .is_some_and(|p| !p.as_os_str().is_empty());
    if has_dir_part {
        return if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(DeviceError::NotFound)
        };
    }
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let exts: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
    for dir in std::env::split_paths(&path_env) {
        for ext in exts {
            let joined = dir.join(format!("{trimmed}{ext}"));
            if joined.is_file() {
                return Ok(joined);
            }
        }
    }
    Err(DeviceError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_missing_and_empty() {
        assert!(resolve_executable("").is_err());
        assert!(resolve_executable("definitely-not-a-real-tool-xyz").is_err());
        assert!(matches!(
            resolve_executable("definitely-not-a-real-tool-xyz"),
            Err(DeviceError::NotFound)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_rejects_shell_interpreted_scripts() {
        // Absolute `.bat`/`.cmd` paths must also be rejected (Windows runs them via cmd implicitly).
        let dir = std::env::temp_dir().join(format!("duthub-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bat = dir.join("tool.bat");
        std::fs::write(&bat, "@echo off\r\n").unwrap();
        let err = resolve_executable(bat.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, DeviceError::Invalid(_)));
        let cmd_file = dir.join("tool.cmd");
        std::fs::write(&cmd_file, "@echo off\r\n").unwrap();
        assert!(resolve_executable(cmd_file.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_capped_truncates() {
        let data = vec![b'x'; 70_000];
        let text = display_output(&read_capped(&data[..], MAX_OUTPUT));
        assert!(text.contains("[输出已截断]"));
        assert!(text.len() < 70_000);
    }
}
