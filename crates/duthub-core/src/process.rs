//! 安全的本地进程执行（`docs/SECURITY.md` #1）。
//!
//! - 仅 `Command::new(resolved).args(argv)`，绝不拼接 shell 字符串。
//! - 可执行名先在 PATH 解析为绝对路径（Windows 仅接受 `.exe`/无扩展名，杜绝 `.bat` 隐式 cmd）。
//! - stdout/stderr 有上限截断；超时按**进程树**终止（子进程可能派生孙进程）。
//! - 输出收尾有界：若残留孙进程持有继承的输出管道，读取不会无限等待，
//!   超过 `DRAIN_TIMEOUT` 返回 `DeviceError::Timeout`。

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::backend::ProcessResult;
use crate::error::{DeviceError, DeviceResult};

const MAX_OUTPUT: usize = 65_536;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 主进程结束后等待输出管道关闭（EOF）的上限：孙进程继承管道时防止读取线程无限等待。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 进程执行抽象：测试用 Mock 注入，生产用 `RealProcessRunner`。
pub trait ProcessRunner: Send + Sync {
    fn run(&self, exe: &Path, args: &[String], timeout: Duration) -> DeviceResult<ProcessResult>;
}

/// 真实执行器：argv-only，无 shell；工作目录为可执行文件所在目录。
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
    // 读取线程经 channel 回传结果：主线程可以用 recv_timeout 限时收尾，
    // 不能 join —— 残留孙进程持有继承管道时 join 会无限等待（审核发现 P2）。
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
            // 超时即取消：终止整个进程树（仅杀主进程时，
            // 派生的孙进程会继续持有输出管道）
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
    // 输出收尾（有界）：正常退出后管道写端可能仍被残留子进程持有，
    // 等不到双路 EOF 就按超时返回，不再阻塞调用方。
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

/// 在 `DRAIN_TIMEOUT` 内收集双路输出；任一路未在期限内到达返回 `None`。
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

/// 终止进程树：Windows 用 `taskkill /T /F`（argv-only，非 shell）；
/// 主进程已退出时枚举不到树，由调用方的输出收尾超时兜底。Unix 仅终止主进程。
/// 保留 `wait()` 收尸，避免残留僵尸进程。
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

/// 把 `adb` / `upgrade_tool` 这类名字或绝对路径解析为可执行文件绝对路径。
///
/// 裸名字只搜 PATH；Windows 仅接受无扩展名与 `.exe`（`.bat/.cmd` 需要 shell 解释，禁止）。
pub fn resolve_executable(spec: &str) -> DeviceResult<PathBuf> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(DeviceError::Invalid("可执行文件为空".into()));
    }
    if trimmed.len() > 2048 || trimmed.contains(['\r', '\n', '\0']) {
        return Err(DeviceError::Invalid("可执行文件路径非法".into()));
    }
    // .bat/.cmd/.com 需要 shell 解释器，隐式 shell 违反 argv-only 边界
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
        // 绝对路径形式的 .bat/.cmd 也必须拒绝（Windows 会隐式经 cmd 解释）
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
