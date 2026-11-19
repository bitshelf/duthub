//! W5：Windows Job Object 进程树收尾回归。
//!
//! - [`AcpClient::kill`]（`TerminateJobObject`）后孙进程消失；
//! - 只关闭 Job 句柄（`KILL_ON_JOB_CLOSE`）也能杀树；
//! - `QueryInformationJobObject` 实测 `ActiveProcessLimit` /
//!   `ProcessMemoryLimit` / `KILL_ON_JOB_CLOSE` 已在核内生效。
//!
//! 用 `cmd /c start /b powershell` 派生孙进程，不依赖真实 goose 的 10 分钟
//! 等待，也不访问真实硬件。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use duthub_desktop::agent_host::acp::AcpClient;
use duthub_desktop::agent_host::job::{
    self, JOB_ACTIVE_PROCESS_LIMIT, JOB_PROCESS_MEMORY_LIMIT, Job,
};
use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duthub-job-kill-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn comspec() -> PathBuf {
    std::env::var_os("COMSPEC").map_or_else(|| PathBuf::from("cmd.exe"), PathBuf::from)
}

/// cmd 脚本：`start /b` 派生 PowerShell 孙进程（写 PID 后睡 60 秒），cmd 自身
/// 再用 ping 挂住，保证测试期间主进程仍然存活。
fn write_tree_script(work: &Path) -> (PathBuf, PathBuf) {
    let pid_file = work.join("grandchild.pid");
    let script = work.join("spawn-tree.cmd");
    let content = format!(
        "@echo off\r\n\
         start \"\" /b powershell -NoProfile -Command \"$PID | Set-Content -Path '{pid}'; Start-Sleep -Seconds 60\"\r\n\
         ping -n 60 127.0.0.1 >nul\r\n",
        pid = pid_file.display()
    );
    std::fs::write(&script, content).unwrap();
    (script, pid_file)
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    predicate()
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// 用 `OpenProcess + GetExitCodeProcess` 判断存活（避免 tasklist 子进程开销）。
fn process_alive(pid: u32) -> bool {
    // SAFETY: 只申请 QUERY_LIMITED_INFORMATION 读取退出码，句柄立即关闭。
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let queried = GetExitCodeProcess(handle, &mut code).is_ok();
        let _ = CloseHandle(handle);
        queried && code == STILL_ACTIVE.0 as u32
    }
}

/// 停止会话（`TerminateJobObject`）后，主进程与孙进程都必须消失。
#[test]
fn acp_kill_terminates_job_process_tree() {
    let work = temp_dir("acp-kill");
    let (script, pid_file) = write_tree_script(&work);
    let acp = AcpClient::spawn(
        &comspec(),
        &["/c".into(), script.to_string_lossy().into_owned()],
        &work,
        &[],
        64 * 1024,
    )
    .unwrap();
    assert!(acp.has_job(), "Windows 上 AcpClient 必须创建并持有 Job");

    assert!(
        wait_until(Duration::from_secs(30), || read_pid(&pid_file).is_some()),
        "孙进程未写出 PID: {}",
        pid_file.display()
    );
    let grandchild = read_pid(&pid_file).unwrap();
    assert_ne!(grandchild, acp.pid(), "孙进程 PID 不应等于父进程");
    assert!(process_alive(grandchild), "孙进程未启动: {grandchild}");
    let root = acp.pid();

    acp.kill();
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(grandchild)),
        "TerminateJobObject 后孙进程仍在运行: {grandchild}"
    );
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(root)),
        "TerminateJobObject 后主进程仍在运行: {root}"
    );
    drop(acp);
    let _ = std::fs::remove_dir_all(&work);
}

/// 不调用 TerminateJobObject，只关闭 Job 句柄：KILL_ON_JOB_CLOSE 兜底杀树。
#[test]
fn closing_job_handle_alone_kills_process_tree() {
    let work = temp_dir("close-handle");
    let (script, pid_file) = write_tree_script(&work);
    let script_arg = script.to_string_lossy().into_owned();
    let mut command = Command::new(comspec());
    command
        .args(["/c", script_arg.as_str()])
        .current_dir(&work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (mut child, job) = job::spawn(&mut command).unwrap();
    let job: Job = job.expect("Windows 上 job::spawn 必须返回 Job");

    assert!(
        wait_until(Duration::from_secs(30), || read_pid(&pid_file).is_some()),
        "孙进程未写出 PID: {}",
        pid_file.display()
    );
    let grandchild = read_pid(&pid_file).unwrap();
    let root = child.id();
    assert!(process_alive(grandchild), "孙进程未启动: {grandchild}");

    drop(job);
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(grandchild)),
        "关闭 Job 句柄后孙进程仍在运行: {grandchild}"
    );
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(root)),
        "关闭 Job 句柄后主进程仍在运行: {root}"
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&work);
}

/// 不走 `kill()`，直接 Drop `AcpClient`：Drop 必须关闭 Job 句柄，
/// `KILL_ON_JOB_CLOSE` 兜底杀树（硬崩溃/Drop 顺序异常的最后防线）。
#[test]
fn dropping_acp_client_kills_job_tree() {
    let work = temp_dir("drop");
    let (script, pid_file) = write_tree_script(&work);
    let acp = AcpClient::spawn(
        &comspec(),
        &["/c".into(), script.to_string_lossy().into_owned()],
        &work,
        &[],
        64 * 1024,
    )
    .unwrap();
    assert!(acp.has_job(), "Windows 上 AcpClient 必须创建并持有 Job");

    assert!(
        wait_until(Duration::from_secs(30), || read_pid(&pid_file).is_some()),
        "孙进程未写出 PID: {}",
        pid_file.display()
    );
    let grandchild = read_pid(&pid_file).unwrap();
    let root = acp.pid();
    assert!(process_alive(grandchild), "孙进程未启动: {grandchild}");

    drop(acp);
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(grandchild)),
        "Drop 后孙进程仍在运行: {grandchild}"
    );
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(root)),
        "Drop 后主进程仍在运行: {root}"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// 内核回读的限制值：ActiveProcessLimit / ProcessMemoryLimit /
/// KILL_ON_JOB_CLOSE 必须与实际配置一致（不是只写在代码里的常量）。
#[test]
fn job_limits_are_applied_in_kernel() {
    let job = Job::create().unwrap();
    let limits = job.query_limits().unwrap();
    assert_eq!(limits.active_process_limit, JOB_ACTIVE_PROCESS_LIMIT);
    assert_eq!(limits.process_memory_limit, JOB_PROCESS_MEMORY_LIMIT);
    // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000
    // JOB_OBJECT_LIMIT_ACTIVE_PROCESS   = 0x0008
    // JOB_OBJECT_LIMIT_PROCESS_MEMORY   = 0x0100
    assert_ne!(
        limits.limit_flags & 0x2000,
        0,
        "KILL_ON_JOB_CLOSE 未生效: {limits:?}"
    );
    assert_ne!(
        limits.limit_flags & 0x0008,
        0,
        "ActiveProcessLimit 未生效: {limits:?}"
    );
    assert_ne!(
        limits.limit_flags & 0x0100,
        0,
        "ProcessMemoryLimit 未生效: {limits:?}"
    );
}
