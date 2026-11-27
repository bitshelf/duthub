//! MXC OS sandbox integration regressions for `duthub-desktop`.
//!
//! - `duthub_sandbox::status()` probes Windows 11 PSEC 1.1+ capabilities without
//!   spawning any child process or mutating user directories.
//! - `agent_host::sandbox::prepare_agent_sandbox()` validates the per-DUT cell,
//!   configures simultaneous write + read isolation, and allows `.duthub/tools/`
//!   executables to coexist with `.duthub/settings.json` denial.
//! - `duthub_sandbox::probe_request()` confirms request serviceability without
//!   launching the target executable.

#![cfg(windows)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use duthub_desktop::agent_host::sandbox;
use duthub_sandbox::{NetworkPolicy, SandboxBackendKind, SpawnRequest, probe_request};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-agent-isolation-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn mxc_status_reports_enforced_windows_psec_without_spawning() {
    let dir = temp_dir("status-probe");
    let canary = dir.join("spawn-canary.cmd");
    std::fs::write(
        &canary,
        "@echo off\r\necho spawned > \"%~dp0spawned.txt\"\r\n",
    )
    .unwrap();
    let before = std::fs::read_dir(&dir).unwrap().count();

    let status = duthub_sandbox::status();
    assert_eq!(status.backend, SandboxBackendKind::ProcessContainer);
    assert!(status.is_enforced(), "expected enforced PSEC: {status:?}");
    assert!(
        status.supports_agent_isolation(),
        "expected full agent isolation on Windows 11 PSEC 1.1+: {status:?}"
    );

    std::thread::sleep(Duration::from_millis(200));
    assert!(!dir.join("spawned.txt").exists(), "probe must never spawn");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        before,
        "probe must not mutate directories"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prepare_agent_sandbox_isolates_sibling_duts_and_probes_cleanly() {
    let work = temp_dir("prepare-cell");
    let paths_a = sandbox::prepare(&work, "dut-alpha").expect("prepare dut-alpha");
    let paths_b = sandbox::prepare(&work, "dut-beta").expect("prepare dut-beta");

    let prepared = sandbox::prepare_agent_sandbox(
        &work,
        "dut-alpha",
        &paths_a,
        None,
        &[paths_a.goose_root.as_path()],
        &[],
        NetworkPolicy::LoopbackAndOutbound { proxy_url: None },
    )
    .expect("prepare_agent_sandbox");

    assert!(prepared.status.supports_agent_isolation());
    assert!(
        prepared
            .policy
            .writable
            .iter()
            .any(|p| p == &paths_a.sandbox)
    );
    assert!(
        prepared
            .policy
            .writable
            .iter()
            .any(|p| p == &paths_a.goose_root)
    );
    assert!(
        prepared
            .policy
            .deny_read
            .iter()
            .any(|p| p == &paths_b.dut_dir),
        "sibling DUT directory must be denied for reads: {:?}",
        prepared.policy.deny_read
    );

    // Probe a concrete SpawnRequest without launching the binary.
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let cmd = PathBuf::from(&system_root).join("System32").join("cmd.exe");
    duthub_sandbox::prepare_cell(&prepared.cell).expect("prepare_cell");
    let local_app_data = prepared.cell.layout().home().join("AppData").join("Local");
    let req = SpawnRequest {
        program: cmd,
        args: vec![OsString::from("/c"), OsString::from("echo ok")],
        cwd: paths_a.sandbox.clone(),
        environment: vec![
            (OsString::from("SYSTEMROOT"), system_root),
            (
                OsString::from("LOCALAPPDATA"),
                local_app_data.into_os_string(),
            ),
        ],
    };
    probe_request(&prepared.cell, &prepared.policy, &req).expect("probe_request must pass");

    let _ = std::fs::remove_dir_all(&work);
}
