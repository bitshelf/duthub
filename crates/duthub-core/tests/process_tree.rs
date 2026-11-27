//! Process execution regression tests: uses a **fake tool that spawns child processes** to cover
//! process-tree termination and bounded output draining.
//!
//! The fake tool is this test binary itself (the `libtest` harness executable):
//! - `fake_tool_spawn_and_exit`: parent exits immediately while grandchild
//!   (`fake_tool_hold_output`) keeps holding the inherited output pipes; under the old behavior,
//!   joining reader threads blocked until the grandchild exited.
//! - `fake_tool_hang_parent`: parent stays alive past the timeout to verify **process-tree**
//!   termination on timeout.
//! - `fake_tool_normal_exit`: exits normally with output to verify the normal path is unaffected.
//!
//! Fake-tool mode activates only when re-invoked with `--exact <name>`; normal `cargo test` runs
//! the regression tests, which re-spawn this binary in fake-tool mode.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use duthub_core::error::DeviceError;
use duthub_core::process::{ProcessRunner, RealProcessRunner};

/// Duration the grandchild holds the output pipes (must significantly exceed `DRAIN_TIMEOUT` plus
/// the test assertion bound).
const HOLD_SECS: u64 = 12;
/// Timeout passed to the runner by the regression tests.
const RUN_TIMEOUT: Duration = Duration::from_secs(3);
/// Upper bound on bounded return time for `run()` (well below `HOLD_SECS`).
const BOUND: Duration = Duration::from_secs(9);

/// Returns whether the current process was re-invoked in fake-tool mode.
fn is_fake_tool_mode() -> bool {
    std::env::args()
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--exact")
}

/// Re-spawn this binary in fake-tool mode to run only the hold test (`HOLD_SECS` pipe hold).
fn spawn_hold_child() -> std::process::Child {
    let exe = std::env::current_exe().expect("test binary path");
    Command::new(exe)
        .args(["fake_tool_hold_output", "--exact", "--test-threads=1"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn fake-tool child process")
}

// ---------------------------------------------------------------------------
// Fake-tool mode (executes only when re-invoked as a child process)
// ---------------------------------------------------------------------------

/// Fake-tool mode: holds output pipes for `HOLD_SECS` before exiting so pipe write ends remain
/// open.
#[test]
fn fake_tool_hold_output() {
    if !is_fake_tool_mode() {
        return;
    }
    println!("fake-tool child holding stdout");
    std::thread::sleep(Duration::from_secs(HOLD_SECS));
}

/// Fake-tool mode: spawns a grandchild and exits immediately while the grandchild holds inherited
/// pipe write ends.
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

/// Fake-tool mode: exits normally with output.
#[test]
fn fake_tool_normal_exit() {
    if !is_fake_tool_mode() {
        return;
    }
    println!("fake-tool ok");
}

// ---------------------------------------------------------------------------
// Regression tests
// ---------------------------------------------------------------------------

fn harness_args(test: &str) -> Vec<String> {
    vec![
        test.to_string(),
        "--exact".to_string(),
        "--test-threads=1".to_string(),
    ]
}

/// Parent exits normally while a grandchild holds the output pipes: `run()` must return within a
/// bounded time (the old behavior blocked for ~`HOLD_SECS` until the grandchild exited).
#[test]
fn runner_returns_when_grandchild_holds_pipe() {
    let runner = RealProcessRunner;
    let exe = std::env::current_exe().unwrap();
    let started = Instant::now();
    let result = runner.run(&exe, &harness_args("fake_tool_spawn_and_exit"), RUN_TIMEOUT);
    let elapsed = started.elapsed();
    assert!(
        elapsed < BOUND,
        "run() must return within bound (grandchild holds pipe for {HOLD_SECS}s), got {elapsed:?}"
    );
    match result {
        // Expected after fix: output drain times out while the grandchild holds the pipe.
        Err(DeviceError::Timeout) => {}
        // Or full output collected if tree cleanup succeeded.
        Ok(res) => assert_eq!(res.exit_code, Some(0)),
        Err(err) => panic!("Unexpected error: {err}"),
    }
}

/// Parent hangs past timeout: the runner must kill the entire process tree and return well below
/// `HOLD_SECS` (killing only the parent left the grandchild holding the pipes for ~`HOLD_SECS`).
#[test]
fn runner_timeout_kills_tree() {
    let runner = RealProcessRunner;
    let exe = std::env::current_exe().unwrap();
    let started = Instant::now();
    let result = runner.run(&exe, &harness_args("fake_tool_hang_parent"), RUN_TIMEOUT);
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(DeviceError::Timeout)),
        "timeout must return DeviceError::Timeout"
    );
    assert!(
        elapsed < BOUND,
        "timeout must kill the process tree and return promptly (parent hangs 60s / grandchild holds pipe {HOLD_SECS}s), got {elapsed:?}"
    );
}

/// Normal path remains intact: a cleanly exiting fake tool returns exit 0 with captured output.
#[test]
fn runner_normal_exit_still_ok() {
    let runner = RealProcessRunner;
    let exe = std::env::current_exe().unwrap();
    let started = Instant::now();
    let result = runner
        .run(&exe, &harness_args("fake_tool_normal_exit"), RUN_TIMEOUT)
        .expect("normal exit must succeed");
    let elapsed = started.elapsed();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("test result: ok"));
    assert!(
        elapsed < BOUND,
        "normal path must not hit drain timeout, got {elapsed:?}"
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
            "background tool must not display a visible console window"
        );
        assert!(
            GetConsoleProcessList(
                processes.as_mut_ptr(),
                u32::try_from(processes.len()).unwrap()
            ) <= 1,
            "background tool must not inherit the parent console"
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
        .expect("should launch background tool and collect output");
    assert_eq!(result.exit_code, Some(0), "{}", result.stdout);
}
