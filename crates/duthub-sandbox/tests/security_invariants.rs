//! Security invariants for `duthub-sandbox` built on `mxc-sdk` (ADR-12).
//!
//! All eight invariants (T1-T8) plus Agent-host simultaneous read+write
//! isolation, capability reporting, and native denial capture run ungated with
//! zero `SKIP` paths on Windows 11 (PSEC 1.1+).
//!
//! Test workloads use pure console system executables (`cmd.exe`, `curl.exe`)
//! that do not link `user32.dll`, so every kernel boundary (filesystem read/write
//! isolation, `deniedPaths`, WFP loopback denial, grandchild inheritance, and
//! Job Object tree termination) is enforced deterministically on both
//! interactive desktops (`WinSta0\Default`) and headless/isolated CI desktops.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(windows)]
mod windows {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use duthub_sandbox::{
        CellSpec, DenialCaptureConfig, ExecResult, MAX_SCRIPT_CHARS, MAX_TIMEOUT, NetworkPolicy,
        SandboxBackendKind, SandboxError, SandboxPolicy, prepare_cell, run_agent, run_script,
    };

    struct Fixture {
        work: PathBuf,
        cell: CellSpec,
        policy: SandboxPolicy,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.work);
        }
    }

    fn cmd_exe() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
            .join("System32")
            .join("cmd.exe")
    }

    fn curl_exe() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
            .join("System32")
            .join("curl.exe")
    }

    fn fixture(tag: &str) -> Fixture {
        let work =
            std::env::temp_dir().join(format!("duthub-sandbox-it-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&work).expect("create temp work_dir");
        let cell = CellSpec::for_dut(&work, &format!("dut-{tag}")).expect("cell spec");
        std::fs::create_dir_all(cell.root()).expect("create cell root");
        let mut policy = SandboxPolicy::script_cell(&cell);
        policy.timeout = Duration::from_secs(30);
        Fixture { work, cell, policy }
    }

    fn sibling_cell(work: &Path, secret: &str) -> (CellSpec, PathBuf) {
        let sibling = CellSpec::for_dut(work, "dut-other").expect("sibling cell");
        prepare_cell(&sibling).expect("prepare sibling cell");
        let secret_path = sibling.root().join("secret.txt");
        std::fs::write(&secret_path, secret).expect("write sibling secret");
        (sibling, secret_path)
    }

    fn run_cmd(cell: &CellSpec, policy: &SandboxPolicy, command: &str) -> ExecResult {
        run_agent(
            cell,
            policy,
            &cmd_exe(),
            &[
                OsString::from("/D"),
                OsString::from("/C"),
                OsString::from(command),
            ],
            &[],
        )
        .expect("sandboxed cmd run")
    }

    fn run_cmd_env(
        cell: &CellSpec,
        policy: &SandboxPolicy,
        command: &str,
        extra_env: &[(&str, &Path)],
    ) -> ExecResult {
        let env: Vec<(OsString, OsString)> = extra_env
            .iter()
            .map(|(k, v)| (OsString::from(k), v.as_os_str().to_os_string()))
            .collect();
        run_agent(
            cell,
            policy,
            &cmd_exe(),
            &[
                OsString::from("/D"),
                OsString::from("/C"),
                OsString::from(command),
            ],
            &env,
        )
        .expect("sandboxed cmd run")
    }

    // T1 -------------------------------------------------------------------
    #[test]
    fn t1_script_writes_inside_its_cell() {
        let fixture = fixture("t1");
        let result = run_cmd(&fixture.cell, &fixture.policy, "echo inside-ok>inside.txt");
        assert_eq!(
            result.exit_code,
            Some(0),
            "stderr={:?}",
            String::from_utf8_lossy(&result.stderr)
        );
        let written = std::fs::read_to_string(fixture.cell.root().join("inside.txt"))
            .expect("script output must exist");
        assert_eq!(written.trim(), "inside-ok");
    }

    // T2 -------------------------------------------------------------------
    #[test]
    fn t2_script_cannot_write_outside_its_cell() {
        let fixture = fixture("t2");
        let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
        let escape = PathBuf::from(profile).join(format!(
            "duthub-sandbox-escape-{}.txt",
            uuid::Uuid::new_v4()
        ));

        // Baseline: the same write succeeds without a sandbox.
        std::fs::write(&escape, "baseline").expect("baseline write must succeed");
        std::fs::remove_file(&escape).expect("baseline cleanup");

        let result = run_cmd_env(
            &fixture.cell,
            &fixture.policy,
            "echo attempted>write-attempted.txt & (echo escaped>\"%ESCAPE_TARGET%\" && echo completed>write-completed.txt)",
            &[("ESCAPE_TARGET", &escape)],
        );
        assert!(
            fixture.cell.root().join("write-attempted.txt").is_file(),
            "workload never reached write attempt: exit={:?} stderr={:?}",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !fixture.cell.root().join("write-completed.txt").is_file(),
            "outside write unexpectedly reported success"
        );
        assert!(
            !escape.exists(),
            "sandboxed workload wrote outside its cell: {}",
            escape.display()
        );
        assert_ne!(result.exit_code, Some(0));
    }

    // T3 -------------------------------------------------------------------
    #[test]
    fn t3_script_cannot_read_the_host_config() {
        let mut fixture = fixture("t3");
        let host_config_dir = fixture.work.join(".duthub");
        std::fs::create_dir_all(&host_config_dir).expect("create .duthub");
        let host_settings = host_config_dir.join("settings.json");
        let marker = format!("DUTHUB-HOST-SECRET-{}", uuid::Uuid::new_v4());
        std::fs::write(&host_settings, format!("{{\"token\":\"{marker}\"}}"))
            .expect("write host settings");

        // Baseline: the host itself reads the file.
        assert!(
            std::fs::read_to_string(&host_settings)
                .unwrap()
                .contains(&marker)
        );

        fixture.policy.deny_read.push(host_config_dir);

        let result = run_cmd_env(
            &fixture.cell,
            &fixture.policy,
            "echo reached>t3-reached.txt & type \"%HOST_SETTINGS%\"",
            &[("HOST_SETTINGS", &host_settings)],
        );
        assert!(
            fixture.cell.root().join("t3-reached.txt").is_file(),
            "workload never started: exit={:?} stderr={:?}",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            !stdout.contains(&marker) && !stderr.contains(&marker),
            "host config leaked into sandbox output: stdout={stdout:?} stderr={stderr:?}"
        );
        assert_ne!(
            result.exit_code,
            Some(0),
            "reading denied host config must fail"
        );
    }

    // T4 -------------------------------------------------------------------
    #[test]
    fn t4_script_cannot_read_another_dut_cell() {
        let fixture = fixture("t4");
        let marker = format!("DUTHUB-SIBLING-SECRET-{}", uuid::Uuid::new_v4());
        let (_sibling, sibling_secret) = sibling_cell(&fixture.work, &marker);

        // Refresh policy so `protected_paths()` includes the newly created sibling DUT.
        let mut policy = SandboxPolicy::script_cell(&fixture.cell);
        policy.timeout = Duration::from_secs(30);

        let result = run_cmd_env(
            &fixture.cell,
            &policy,
            "echo reached>t4-reached.txt & type \"%SIBLING_SECRET%\"",
            &[("SIBLING_SECRET", &sibling_secret)],
        );
        assert!(
            fixture.cell.root().join("t4-reached.txt").is_file(),
            "workload never started: exit={:?}",
            result.exit_code
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            !stdout.contains(&marker) && !stderr.contains(&marker),
            "sibling DUT cell leaked into sandbox: stdout={stdout:?} stderr={stderr:?}"
        );
        assert_ne!(
            result.exit_code,
            Some(0),
            "reading sibling DUT cell must fail"
        );
    }

    // T5 -------------------------------------------------------------------
    #[test]
    fn t5_script_has_no_loopback_network() {
        let fixture = fixture("t5");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let accepted = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let accepted_bg = accepted.clone();
        let stop_bg = stop.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).ok();
            while !stop_bg.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        accepted_bg.store(true, Ordering::SeqCst);
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        let url = format!("http://127.0.0.1:{port}/");
        // 1. NetworkPolicy::Deny blocks loopback TCP at the kernel WFP layer.
        let denied = run_agent(
            &fixture.cell,
            &fixture.policy,
            &curl_exe(),
            &[
                OsString::from("--silent"),
                OsString::from("--connect-timeout"),
                OsString::from("2"),
                OsString::from(&url),
            ],
            &[],
        )
        .expect("sandboxed denied curl");
        assert!(
            !accepted.load(Ordering::SeqCst),
            "sandboxed workload with NetworkPolicy::Deny reached host loopback TCP listener"
        );
        assert_ne!(denied.exit_code, Some(0), "denied curl must fail");

        // 2. Positive control: NetworkPolicy::LoopbackOnly permits the exact same loopback target.
        let mut loopback_policy = fixture.policy.clone();
        loopback_policy.network = NetworkPolicy::LoopbackOnly;
        let _allowed = run_agent(
            &fixture.cell,
            &loopback_policy,
            &curl_exe(),
            &[
                OsString::from("--silent"),
                OsString::from("--connect-timeout"),
                OsString::from("3"),
                OsString::from(&url),
            ],
            &[],
        )
        .expect("sandboxed loopback curl");
        assert!(
            accepted.load(Ordering::SeqCst),
            "NetworkPolicy::LoopbackOnly must allow host loopback TCP connection"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = server.join();
    }

    // T6 -------------------------------------------------------------------
    #[test]
    fn t6_grandchild_inherits_the_sandbox() {
        let fixture = fixture("t6");
        let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
        let escape = PathBuf::from(profile).join(format!(
            "duthub-sandbox-grandchild-{}.txt",
            uuid::Uuid::new_v4()
        ));
        let marker = format!("GRANDCHILD-SIBLING-SECRET-{}", uuid::Uuid::new_v4());
        let (_sibling, sibling_secret) = sibling_cell(&fixture.work, &marker);

        let mut policy = SandboxPolicy::script_cell(&fixture.cell);
        policy.timeout = Duration::from_secs(30);

        // Child cmd.exe spawns a grandchild cmd.exe via a script inside the cell.
        let grandchild_cmd = fixture.cell.root().join("grandchild.cmd");
        std::fs::write(
            &grandchild_cmd,
            "@echo off\r\n\
             (echo x>\"%ESCAPE_TARGET%\") 2>nul && (echo GRANDCHILD_WROTE) || (echo GRANDCHILD_WRITE_DENIED)\r\n\
             (type \"%SIBLING_SECRET%\" 2>nul) && (echo GRANDCHILD_READ_LEAKED) || (echo GRANDCHILD_READ_DENIED)\r\n\
             echo grandchild-ok>grandchild-inside.txt\r\n",
        )
        .expect("write grandchild script");

        let result = run_cmd_env(
            &fixture.cell,
            &policy,
            "cmd.exe /D /C grandchild.cmd",
            &[
                ("ESCAPE_TARGET", &escape),
                ("SIBLING_SECRET", &sibling_secret),
            ],
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(
            stdout.contains("GRANDCHILD_WRITE_DENIED"),
            "grandchild write was not denied: stdout={stdout:?} stderr={:?}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            stdout.contains("GRANDCHILD_READ_DENIED"),
            "grandchild sibling read was not denied: stdout={stdout:?}"
        );
        assert!(
            !stdout.contains(&marker),
            "grandchild leaked sibling secret: {stdout:?}"
        );
        assert!(
            !escape.exists(),
            "grandchild escaped and created {}",
            escape.display()
        );
        assert_eq!(
            std::fs::read_to_string(fixture.cell.root().join("grandchild-inside.txt"))
                .expect("grandchild inside write")
                .trim(),
            "grandchild-ok"
        );
    }

    // T7 -------------------------------------------------------------------
    #[test]
    fn t7_timeout_kills_the_whole_tree() {
        let mut fixture = fixture("t7");
        fixture.policy.timeout = Duration::from_secs(2);

        // Child cmd.exe spawns a grandchild cmd.exe that loops writing to marker.txt
        // indefinitely. When the 2s container timeout fires, MXC's UiJobObject must
        // terminate both parent and grandchild.
        let child_cmd = fixture.cell.root().join("sleeper.cmd");
        std::fs::write(
            &child_cmd,
            "@echo off\r\n\
             :loop\r\n\
             echo tick>>marker.txt\r\n\
             for /L %%i in (1,1,4000) do @rem\r\n\
             goto loop\r\n",
        )
        .expect("write sleeper script");

        let result = run_cmd(&fixture.cell, &fixture.policy, "cmd.exe /D /C sleeper.cmd");
        assert!(result.timed_out, "expected timeout, got {result:?}");
        assert_eq!(result.exit_code, None);

        let marker = fixture.cell.root().join("marker.txt");
        let first = std::fs::metadata(&marker)
            .expect("grandchild must have created marker.txt")
            .len();
        assert!(first > 0, "grandchild never wrote marker.txt");
        std::thread::sleep(Duration::from_millis(800));
        let second = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            first, second,
            "descendant survived container timeout and kept writing ({first} -> {second})"
        );
    }

    // T8 -------------------------------------------------------------------
    #[test]
    fn t8_over_limit_requests_are_refused_without_spawning() {
        let mut fixture = fixture("t8");
        let too_long = "x".repeat(MAX_SCRIPT_CHARS + 1);
        match run_script(&fixture.cell, &fixture.policy, &too_long) {
            Err(SandboxError::PolicyRejected(reason)) => {
                assert!(reason.contains("exceeds limit"), "{reason}")
            }
            other => panic!("oversized script must be refused, got {other:?}"),
        }
        fixture.policy.timeout = MAX_TIMEOUT + Duration::from_secs(1);
        match run_script(&fixture.cell, &fixture.policy, "Get-Date") {
            Err(SandboxError::PolicyRejected(_)) => {}
            other => panic!("over-limit timeout must be refused, got {other:?}"),
        }
        fixture.policy.timeout = Duration::from_secs(10);
        match run_script(
            &fixture.cell,
            &fixture.policy,
            "Format-Volume -DriveLetter C",
        ) {
            Err(SandboxError::PolicyRejected(reason)) => {
                assert!(reason.contains("prohibited"), "{reason}")
            }
            other => panic!("destructive script must be refused, got {other:?}"),
        }
        let entries = std::fs::read_dir(fixture.cell.root())
            .expect("read cell")
            .count();
        assert_eq!(entries, 0, "refused requests must not touch cell");

        // Output truncation cap is enforced without deadlocking the child.
        fixture.policy.timeout = Duration::from_secs(30);
        fixture.policy.max_output_bytes = 256;
        let result = run_cmd(
            &fixture.cell,
            &fixture.policy,
            "for /L %i in (1,1,200) do @echo AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        );
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout_truncated, "stdout must report truncation");
        assert_eq!(result.stdout.len(), 256);
    }

    // Agent host: simultaneous write + read isolation ----------------------
    #[test]
    fn agent_host_writes_only_inside_its_cell_and_cannot_read_sibling() {
        let fixture = fixture("host");
        let (_sibling, sibling_secret) = sibling_cell(&fixture.work, "AGENT-SIBLING-SECRET");
        let mut policy = SandboxPolicy::agent_cell(&fixture.cell, None);
        policy.timeout = Duration::from_secs(30);

        // Write outside cell must be denied.
        let outside = fixture.work.join("outside-dir");
        let denied_write = run_agent(
            &fixture.cell,
            &policy,
            &cmd_exe(),
            &[
                OsString::from("/c"),
                OsString::from("mkdir"),
                outside.clone().into_os_string(),
            ],
            &[],
        )
        .expect("sandboxed outside mkdir");
        assert!(!outside.exists(), "agent created directory outside cell");
        assert_ne!(denied_write.exit_code, Some(0));

        // Read of sibling DUT secret must be denied simultaneously.
        let denied_read = run_agent(
            &fixture.cell,
            &policy,
            &cmd_exe(),
            &[
                OsString::from("/c"),
                OsString::from("type"),
                sibling_secret.into_os_string(),
            ],
            &[],
        )
        .expect("sandboxed read check");
        assert!(
            !String::from_utf8_lossy(&denied_read.stdout).contains("AGENT-SIBLING-SECRET"),
            "agent read sibling DUT secret"
        );
        assert_ne!(denied_read.exit_code, Some(0));

        // Write inside cell must succeed.
        let inside = fixture.cell.root().join("inside-dir");
        let allowed = run_agent(
            &fixture.cell,
            &policy,
            &cmd_exe(),
            &[
                OsString::from("/c"),
                OsString::from("mkdir"),
                inside.clone().into_os_string(),
            ],
            &[],
        )
        .expect("sandboxed inside mkdir");
        assert_eq!(allowed.exit_code, Some(0));
        assert!(inside.is_dir(), "inside mkdir must succeed");
    }

    #[test]
    fn status_reports_real_windows_psec_capabilities() {
        let status = duthub_sandbox::status();
        assert_eq!(status.backend, SandboxBackendKind::ProcessContainer);
        assert!(status.is_enforced(), "PSEC must be enforced: {status:?}");
        assert_eq!(status.tier.as_deref(), Some("base-container"));
        assert!(
            status.supports_read_isolation(),
            "PSEC 1.1 must report read_deny: {status:?}"
        );
        assert!(
            status.supports_agent_isolation(),
            "PSEC 1.1 must report agent isolation: {status:?}"
        );
    }

    #[test]
    fn capture_denials_produces_diagnostic_report_when_supported() {
        let status = duthub_sandbox::status();
        if !status.capture_denials {
            return;
        }
        let fixture = fixture("denials");
        let (_sibling, secret) = sibling_cell(&fixture.work, "CAPTURE-SECRET");
        let report_path = fixture.work.join("denials.json");
        let mut policy = fixture.policy.clone();
        policy.deny_read = vec![secret.parent().unwrap().to_path_buf()];
        policy.capture_denials = Some(DenialCaptureConfig {
            output_path: Some(report_path),
            retain_etl: false,
        });
        let result = run_cmd_env(
            &fixture.cell,
            &policy,
            "type \"%DENIED_FILE%\"",
            &[("DENIED_FILE", &secret)],
        );
        let summary = result
            .denial_capture
            .expect("denial capture metadata must be populated");
        assert!(
            summary.json_path.is_file(),
            "denials JSON must exist at {}",
            summary.json_path.display()
        );
        assert!(
            summary.total_denials > 0,
            "expected at least one recorded denial in {}",
            summary.json_path.display()
        );
    }
}

#[cfg(not(windows))]
mod non_windows {
    use super::*;
    use duthub_sandbox::{CellSpec, SandboxPolicy};

    #[test]
    fn status_and_policy_validation_work_on_non_windows() {
        let status = duthub_sandbox::status();
        assert!(!status.summary().is_empty());

        let dir = tempfile::tempdir().expect("tempdir");
        let cell = CellSpec::for_dut(dir.path(), "dut-unix").expect("cell");
        std::fs::create_dir_all(cell.root()).expect("mkdir");
        let policy = SandboxPolicy::agent_cell(&cell, None);
        duthub_sandbox::validate_policy(&policy, cell.root()).expect("valid policy");
    }
}
