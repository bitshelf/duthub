//! Long-lived sandboxed processes (the ACP session host lifecycle).
//!
//! `run`/`run_agent` are one-shot: the stdin payload is fixed and the output is
//! collected after exit. An ACP session needs a writable stdin and readable
//! stdout for as long as it lives, so it is spawned through
//! [`duthub_sandbox::spawn_streaming`] instead. The security properties are the
//! same ones `security_invariants.rs` proves for one-shot commands: the child
//! runs under the cell's restricted token, writes outside the cell are denied,
//! and the Job bounds the tree.
//!
//! The `Drop`/`terminate` tests never touch real hardware and never spawn
//! anything outside the cell.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use duthub_sandbox::{
    CellSpec, SandboxPolicy, SpawnRequest, encode_script, spawn_streaming, windows_powershell,
};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-sandbox-stream-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn comspec() -> PathBuf {
    std::env::var_os("COMSPEC").map_or_else(|| PathBuf::from("cmd.exe"), PathBuf::from)
}

fn agent_policy(cell: &CellSpec) -> SandboxPolicy {
    let mut policy = SandboxPolicy::agent_cell(cell);
    policy.process_limit = Some(16);
    policy.memory_limit_bytes = Some(4 * 1024 * 1024 * 1024);
    policy
}

/// Environment for a long-lived Agent host: the cell's TEMP plus the system
/// facts PowerShell needs. `USERPROFILE` is deliberately the real host profile:
/// the test proves the restricted token refuses the write (host write boundary),
/// not that the variable is merely absent.
fn agent_environment(cell: &CellSpec) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let temp = cell.root().join("tmp");
    vec![
        ("SystemRoot".into(), system_root.clone()),
        ("windir".into(), system_root.clone()),
        (
            "PATH".into(),
            format!(
                "{}\\System32;{}\\",
                system_root.to_string_lossy(),
                system_root.to_string_lossy()
            )
            .into(),
        ),
        ("COMSPEC".into(), comspec().into_os_string()),
        ("PATHEXT".into(), ".COM;.EXE;.BAT;.CMD".into()),
        ("TEMP".into(), temp.clone().into_os_string()),
        ("TMP".into(), temp.into_os_string()),
        (
            "USERPROFILE".into(),
            std::env::var_os("USERPROFILE").unwrap_or_default(),
        ),
    ]
}

fn prepare_cell(work: &Path, dut: &str) -> (CellSpec, SandboxPolicy) {
    let cell = CellSpec::for_dut(work, dut).expect("cell");
    // `prepare` validates that the writable root exists, so the host creates it
    // before establishing the kernel state (the desktop does the same).
    std::fs::create_dir_all(cell.root()).expect("cell root");
    let policy = agent_policy(&cell);
    duthub_sandbox::prepare(&cell, &policy).expect("prepare");
    (cell, policy)
}

/// Read stdout lines on a helper thread so a hung child fails the test instead
/// of blocking the suite forever.
fn line_channel(stdout: std::fs::File) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    receiver
}

#[test]
fn streaming_child_round_trips_and_cannot_write_the_host_profile() {
    let work = temp_dir("round-trip");
    let (cell, policy) = prepare_cell(&work, "dut-stream");
    let user_profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
    let marker = PathBuf::from(&user_profile)
        .join(format!("duthub-stream-escape-{}.txt", uuid::Uuid::new_v4()));
    let marker_for_script = marker.to_string_lossy().replace('\'', "''");
    let script = format!(
        r#"
try {{
    Set-Content -LiteralPath '{marker_for_script}' -Value 'escaped' -ErrorAction Stop
    Write-Output 'WROTE'
}} catch {{
    Write-Output 'DENIED'
}}
while ($true) {{
    $line = [Console]::In.ReadLine()
    if ($null -eq $line) {{ break }}
    Write-Output ('echo:' + $line)
}}
"#
    );
    let request = SpawnRequest {
        program: windows_powershell(),
        args: [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &encode_script(&script),
        ]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect(),
        cwd: cell.root().to_path_buf(),
        environment: agent_environment(&cell),
    };
    let mut process = spawn_streaming(&cell, &policy, &request).expect("spawn_streaming");
    let limits = process.limits().expect("job limits");
    assert_eq!(limits.active_process_limit, 16, "{limits:?}");
    assert_eq!(
        limits.process_memory_limit,
        4 * 1024 * 1024 * 1024,
        "{limits:?}"
    );
    assert!(
        limits.kill_on_close,
        "KILL_ON_JOB_CLOSE 必须已生效: {limits:?}"
    );

    let mut stdin = process.take_stdin().expect("stdin");
    let lines = line_channel(process.take_stdout().expect("stdout"));

    let first = lines
        .recv_timeout(Duration::from_secs(60))
        .expect("受限令牌下的 PowerShell 未写出首行");
    assert_eq!(
        first.trim(),
        "DENIED",
        "受限令牌必须拒绝写 %USERPROFILE%（实际: {first:?}）"
    );
    assert!(
        !marker.exists(),
        "宿主目录出现了沙箱子进程写入的文件: {}",
        marker.display()
    );

    stdin.write_all(b"ping\n").expect("write stdin");
    stdin.flush().expect("flush stdin");
    let echo = lines
        .recv_timeout(Duration::from_secs(30))
        .expect("长驻子进程未回应 stdin 行");
    assert_eq!(echo.trim(), "echo:ping", "stdin→stdout 往返失败: {echo:?}");

    process.terminate().expect("terminate");
    let exit = process.wait_timeout(Duration::from_secs(15)).expect("wait");
    assert!(exit.is_some(), "TerminateJobObject 后进程未退出");
    drop(stdin);
    let _ = std::fs::remove_dir_all(&work);
}

/// `cmd /c start /b powershell` derives a grandchild; dropping the handle must
/// terminate the whole tree, not only the direct child.
#[test]
fn drop_terminates_the_whole_tree() {
    let work = temp_dir("tree");
    let (cell, policy) = prepare_cell(&work, "dut-tree");
    let pid_file = cell.root().join("grandchild.pid");
    let script = cell.root().join("spawn-tree.cmd");
    std::fs::write(
        &script,
        format!(
            "@echo off\r\n\
             start \"\" /b powershell -NoProfile -Command \"$PID | Set-Content -Path '{pid}'; Start-Sleep -Seconds 60\"\r\n\
             ping -n 60 127.0.0.1 >nul\r\n",
            pid = pid_file.display()
        ),
    )
    .unwrap();
    let request = SpawnRequest {
        program: comspec(),
        args: vec!["/c".into(), script.to_string_lossy().into_owned().into()],
        cwd: cell.root().to_path_buf(),
        environment: agent_environment(&cell),
    };
    let process = spawn_streaming(&cell, &policy, &request).expect("spawn_streaming");
    let root = process.pid();
    assert!(
        wait_until(Duration::from_secs(30), || read_pid(&pid_file).is_some()),
        "孙进程未写出 PID: {}",
        pid_file.display()
    );
    let grandchild = read_pid(&pid_file).unwrap();
    assert!(process_alive(grandchild), "孙进程未启动: {grandchild}");

    drop(process);
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

fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: only QUERY_LIMITED_INFORMATION is requested and the handle is
    // closed immediately.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let queried = GetExitCodeProcess(handle, &mut code) != 0;
        let _ = CloseHandle(handle);
        queried && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(windows))]
mod unsupported {
    use std::path::PathBuf;

    use duthub_sandbox::{CellSpec, SandboxError, SandboxPolicy, SpawnRequest, spawn_streaming};

    #[test]
    fn streaming_spawn_refuses_without_a_backend() {
        let cell = CellSpec::at(std::env::temp_dir(), "dut-unsupported").unwrap();
        let policy = SandboxPolicy::agent_cell(&cell);
        let request = SpawnRequest {
            program: PathBuf::from("/bin/sh"),
            args: vec![],
            cwd: cell.root().to_path_buf(),
            environment: Vec::new(),
        };
        assert!(matches!(
            spawn_streaming(&cell, &policy, &request),
            Err(SandboxError::Unsupported(_))
        ));
    }
}
