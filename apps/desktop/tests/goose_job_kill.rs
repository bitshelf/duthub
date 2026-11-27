//! Process-tree termination regressions for `AcpClient` backed by `mxc-sdk`.
//!
//! - [`AcpClient::kill`] terminates both the root process and any descendants
//!   spawned inside the MXC container (`UiJobObject::terminate`).
//! - Dropping [`AcpClient`] without calling `kill()` terminates the entire tree
//!   via `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use duthub_desktop::agent_host::acp::AcpClient;
use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duthub-job-kill-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cmd_exe() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("cmd.exe")
}

/// Parent `cmd.exe` spawns a grandchild `cmd.exe` that continuously appends to
/// `marker.txt`, proving both parent and descendant are alive before teardown
/// and both cease immediately after `kill()` or `Drop`.
fn write_tree_scripts(work: &Path) -> (PathBuf, PathBuf) {
    let marker = work.join("grandchild-marker.txt");
    let child = work.join("grandchild-loop.cmd");
    std::fs::write(
        &child,
        "@echo off\r\n\
         :loop\r\n\
         echo tick>>grandchild-marker.txt\r\n\
         for /L %%i in (1,1,4000) do @rem\r\n\
         goto loop\r\n",
    )
    .unwrap();
    let parent = work.join("spawn-tree.cmd");
    std::fs::write(
        &parent,
        "@echo off\r\n\
         cmd.exe /D /Q /C grandchild-loop.cmd\r\n",
    )
    .unwrap();
    (parent, marker)
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

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: `OpenProcess` requests `PROCESS_QUERY_LIMITED_INFORMATION` only
    // to read the exit code, and closes the handle immediately afterwards.
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

#[test]
fn acp_kill_terminates_job_process_tree() {
    let work = temp_dir("acp-kill");
    let (script, marker) = write_tree_scripts(&work);
    let acp = AcpClient::spawn(
        &cmd_exe(),
        &[
            "/D".into(),
            "/Q".into(),
            "/C".into(),
            script.to_string_lossy().into_owned(),
        ],
        &work,
        &[],
        64 * 1024,
    )
    .unwrap();
    assert!(
        acp.has_job(),
        "AcpClient must attach a sandbox teardown handle"
    );

    assert!(
        wait_until(Duration::from_secs(15), || file_len(&marker) > 0),
        "grandchild never wrote marker: {}",
        marker.display()
    );
    let root = acp.pid();

    acp.kill();
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(root)),
        "root process still alive after AcpClient::kill(): {root}"
    );
    std::thread::sleep(Duration::from_millis(250));
    let after_kill = file_len(&marker);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        after_kill,
        file_len(&marker),
        "grandchild kept writing after AcpClient::kill()"
    );
    drop(acp);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn dropping_acp_client_kills_job_tree() {
    let work = temp_dir("drop");
    let (script, marker) = write_tree_scripts(&work);
    let acp = AcpClient::spawn(
        &cmd_exe(),
        &[
            "/D".into(),
            "/Q".into(),
            "/C".into(),
            script.to_string_lossy().into_owned(),
        ],
        &work,
        &[],
        64 * 1024,
    )
    .unwrap();
    assert!(
        acp.has_job(),
        "AcpClient must attach a sandbox teardown handle"
    );

    assert!(
        wait_until(Duration::from_secs(15), || file_len(&marker) > 0),
        "grandchild never wrote marker: {}",
        marker.display()
    );
    let root = acp.pid();

    drop(acp);
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(root)),
        "root process still alive after AcpClient::drop(): {root}"
    );
    std::thread::sleep(Duration::from_millis(250));
    let after_drop = file_len(&marker);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        after_drop,
        file_len(&marker),
        "grandchild kept writing after AcpClient::drop()"
    );
    let _ = std::fs::remove_dir_all(&work);
}
