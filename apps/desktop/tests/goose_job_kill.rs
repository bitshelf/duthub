//! Process-tree termination regressions for `AcpClient` backed by `mxc-sdk`.
//!
//! - [`AcpClient::kill`] terminates both the root process and any descendants
//!   spawned inside the MXC container (`UiJobObject::terminate`).
//! - Dropping [`AcpClient`] without calling `kill()` terminates the entire tree
//!   via `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.

#![cfg(windows)]

#[path = "support/agent_fixture.rs"]
mod agent_fixture;

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
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: `OpenProcess` requests `PROCESS_QUERY_LIMITED_INFORMATION` only
    // to read the exit code, and closes the handle immediately afterwards.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let queried = GetExitCodeProcess(handle, &raw mut code).is_ok();
        let _ = CloseHandle(handle);
        queried && code == STILL_ACTIVE.0 as u32
    }
}

#[test]
fn acp_kill_terminates_job_process_tree() {
    let work = temp_dir("acp-kill");
    let marker = work.join("grandchild-marker.txt");
    let acp = AcpClient::spawn(
        agent_fixture::executable(),
        &["tree".into(), marker.to_string_lossy().into_owned()],
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
        "grandchild never wrote marker {}: {}",
        marker.display(),
        acp.stderr_tail()
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
    let marker = work.join("grandchild-marker.txt");
    let acp = AcpClient::spawn(
        agent_fixture::executable(),
        &["tree".into(), marker.to_string_lossy().into_owned()],
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
        "grandchild never wrote marker {}: {}",
        marker.display(),
        acp.stderr_tail()
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
