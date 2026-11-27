//! 进程执行回归（审核发现 P2-2）：用**会派生子进程的假工具**覆盖进程树与输出收尾。
//!
//! 假工具就是本测试二进制自身（libtest harness 可执行文件）：
//! - `fake_tool_spawn_and_exit`：父进程立即退出，孙进程（`fake_tool_hold_output`）
//!   继续持有继承的输出管道 —— 旧行为下读取线程 join 会一直等到孙进程退出；
//! - `fake_tool_hang_parent`：父进程长驻（超过超时），验证超时按**进程树**终止；
//! - `fake_tool_normal_exit`：正常退出并输出（保证常规路径不被破坏）。
//!
//! 假工具模式仅在以 `--exact <名>` 子调用时生效；普通 `cargo test` 直接跑
//! 回归测试（它们会以假工具模式再拉起本二进制）。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use duthub_core::error::DeviceError;
use duthub_core::process::{ProcessRunner, RealProcessRunner};

/// 孙进程持有输出管道的时长（必须显著大于 `DRAIN_TIMEOUT` + 测试断言上限）。
const HOLD_SECS: u64 = 12;
/// 回归测试给执行器的超时。
const RUN_TIMEOUT: Duration = Duration::from_secs(3);
/// 修复后 `run()` 必须有界返回的上限（远小于 `HOLD_SECS`）。
const BOUND: Duration = Duration::from_secs(9);

/// 当前进程是否以"假工具模式"被子调用拉起。
fn is_fake_tool_mode() -> bool {
    std::env::args()
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--exact")
}

/// 以假工具模式拉起本二进制：只运行指定测试（孙进程持有输出管道 `HOLD_SECS`）。
fn spawn_hold_child() -> std::process::Child {
    let exe = std::env::current_exe().expect("测试二进制路径");
    Command::new(exe)
        .args(["fake_tool_hold_output", "--exact", "--test-threads=1"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("拉起假工具子进程")
}

// ---------------------------------------------------------------------------
// 假工具模式（仅子调用时执行实际逻辑）
// ---------------------------------------------------------------------------

/// 假工具模式：持有输出管道 `HOLD_SECS` 后退出（管道写端因此被长期占用）。
#[test]
fn fake_tool_hold_output() {
    if !is_fake_tool_mode() {
        return;
    }
    println!("fake-tool child holding stdout");
    std::thread::sleep(Duration::from_secs(HOLD_SECS));
}

/// 假工具模式：派生孙进程后父进程立即退出（孙进程持有继承的管道写端）。
#[test]
fn fake_tool_spawn_and_exit() {
    if !is_fake_tool_mode() {
        return;
    }
    // Intentionally not waited: simulates a parent exiting while a grandchild holds inherited pipes.
    #[expect(
        clippy::zombie_processes,
        reason = "test intentionally exits parent without waiting on grandchild"
    )]
    let _child = spawn_hold_child();
    println!("fake-tool parent exits immediately");
}

/// Fake tool mode: parent stays alive longer than runner timeout to test tree termination.
#[test]
fn fake_tool_hang_parent() {
    if !is_fake_tool_mode() {
        return;
    }
    // Intentionally not waited: the runner kills the whole process tree on timeout.
    #[expect(
        clippy::zombie_processes,
        reason = "runner terminates the process tree before wait() could ever run"
    )]
    let _child = spawn_hold_child();
    std::thread::sleep(Duration::from_secs(60));
}

/// 假工具模式：正常退出并输出。
#[test]
fn fake_tool_normal_exit() {
    if !is_fake_tool_mode() {
        return;
    }
    println!("fake-tool ok");
}

// ---------------------------------------------------------------------------
// 回归测试
// ---------------------------------------------------------------------------

fn harness_args(test: &str) -> Vec<String> {
    vec![
        test.to_string(),
        "--exact".to_string(),
        "--test-threads=1".to_string(),
    ]
}

/// 主进程正常退出但孙进程持有输出管道：`run()` 必须有界返回（旧行为会阻塞
/// 到孙进程退出，约 `HOLD_SECS`）。
#[test]
fn runner_returns_when_grandchild_holds_pipe() {
    let runner = RealProcessRunner;
    let exe = std::env::current_exe().unwrap();
    let started = Instant::now();
    let result = runner.run(&exe, &harness_args("fake_tool_spawn_and_exit"), RUN_TIMEOUT);
    let elapsed = started.elapsed();
    assert!(
        elapsed < BOUND,
        "run() 必须有界返回（孙进程持有管道 {HOLD_SECS}s），实际 {elapsed:?}"
    );
    match result {
        // 修复后预期：输出收尾超时（孙进程持有管道，输出不完整）
        Err(DeviceError::Timeout) => {}
        // 或树清理成功拿到完整输出
        Ok(res) => assert_eq!(res.exit_code, Some(0)),
        Err(err) => panic!("意外错误: {err}"),
    }
}

/// 父进程长驻超过超时：执行器必须按进程树终止并在远小于 `HOLD_SECS` 的时间内返回。
/// 旧行为只杀主进程，孙进程持有管道会把读取线程拖到约 `HOLD_SECS`。
#[test]
fn runner_timeout_kills_tree() {
    let runner = RealProcessRunner;
    let exe = std::env::current_exe().unwrap();
    let started = Instant::now();
    let result = runner.run(&exe, &harness_args("fake_tool_hang_parent"), RUN_TIMEOUT);
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(DeviceError::Timeout)),
        "超时必须报 Timeout"
    );
    assert!(
        elapsed < BOUND,
        "超时必须终止进程树并尽快返回（父进程长驻 60s / 孙进程持管道 {HOLD_SECS}s），实际 {elapsed:?}"
    );
}

/// 常规路径不被破坏：正常退出的假工具返回 exit 0 且能收到输出。
#[test]
fn runner_normal_exit_still_ok() {
    let runner = RealProcessRunner;
    let exe = std::env::current_exe().unwrap();
    let started = Instant::now();
    let result = runner
        .run(&exe, &harness_args("fake_tool_normal_exit"), RUN_TIMEOUT)
        .expect("正常退出必须成功");
    let elapsed = started.elapsed();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("test result: ok"));
    assert!(
        elapsed < BOUND,
        "正常路径不应触发收尾超时，实际 {elapsed:?}"
    );
}

#[cfg(windows)]
#[test]
fn fake_tool_console_state() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleWindow() -> *mut std::ffi::c_void;
        fn GetConsoleProcessList(processes: *mut u32, capacity: u32) -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn IsWindowVisible(window: *mut std::ffi::c_void) -> i32;
    }
    if !is_fake_tool_mode() {
        return;
    }
    let mut processes = [0; 2];
    // SAFETY: The array is writable for the provided capacity, and IsWindowVisible
    // is called only with the non-null handle returned by GetConsoleWindow.
    unsafe {
        let window = GetConsoleWindow();
        assert!(
            window.is_null() || IsWindowVisible(window) == 0,
            "后台工具不能显示控制台"
        );
        assert!(
            GetConsoleProcessList(
                processes.as_mut_ptr(),
                u32::try_from(processes.len()).unwrap()
            ) <= 1,
            "后台工具不能继承父进程控制台"
        );
    }
}

#[cfg(windows)]
#[test]
fn runner_starts_windows_tools_without_a_console() {
    let result = RealProcessRunner
        .run(
            &std::env::current_exe().unwrap(),
            &harness_args("fake_tool_console_state"),
            RUN_TIMEOUT,
        )
        .expect("应能启动后台工具并收集输出");
    assert_eq!(result.exit_code, Some(0), "{}", result.stdout);
}
