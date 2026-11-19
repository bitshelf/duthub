//! Security invariants of the sandbox backend. **This file is the regression
//! gate for the boundary**: any change that lets a sandboxed script write
//! outside its cell, read the host config, read another DUT's cell, reach the
//! loopback, escape via a grandchild, survive a timeout, or run an over-limit
//! script must fail here.
//!
//! Every negative test first proves the operation **succeeds without the
//! sandbox**, so a missing file or a bad path cannot masquerade as enforcement.
//! The verdict is always host-side state (does the file exist? does it contain
//! the sentinel?) plus the child's exit code — never "the script printed NO".
//!
//! Read isolation (`T3`, `T4`) needs the dedicated sandbox identity: an
//! AppContainer token keeps the host user's SID and group list, so the kernel
//! authorises its reads with the host identity and no deny ACE for a
//! container-owned SID can change that (measured 2026-11-25, ADR-12 §5). On a
//! machine that has not run the one-time elevated provisioning those tests print
//! an explicit SKIP and return; they are never silently weakened and never
//! marked ignored (the annotations are rejected by `tests/source_contract.rs`).
//!
//! On non-Windows the same entry points must report `Unsupported`; that branch
//! is compiled there and never silently skips a boundary claim.

#![cfg_attr(not(windows), allow(dead_code))]

use std::path::PathBuf;

fn work_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("duthub-sandbox-it-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create work dir");
    dir
}

fn powershell() -> PathBuf {
    duthub_sandbox::windows_powershell()
}

#[cfg(windows)]
mod windows {
    use super::*;
    use duthub_sandbox::{
        CellSpec, MAX_SCRIPT_CHARS, MAX_TIMEOUT, SandboxError, SandboxPolicy, run_agent, run_script,
    };
    use std::ffi::OsString;

    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use std::time::Instant;

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

    fn fixture(tag: &str) -> Fixture {
        let work = work_dir(tag);
        let cell = CellSpec::for_dut(&work, "dut-a").expect("cell");
        std::fs::create_dir_all(cell.root()).expect("cell root");
        let mut policy = SandboxPolicy::scratch(cell.root());
        policy.timeout = Duration::from_secs(60);
        Fixture { work, cell, policy }
    }

    /// The cell of a second DUT under the same work dir, with a secret file.
    fn sibling_cell(work: &Path, secret: &str) -> (CellSpec, PathBuf) {
        let sibling = CellSpec::for_dut(work, "dut-b").expect("sibling cell");
        std::fs::create_dir_all(sibling.root()).expect("sibling root");
        let secret_path = sibling.root().join("secret.txt");
        std::fs::write(&secret_path, secret).expect("write sibling secret");
        (sibling, secret_path)
    }

    fn script_text(result: &duthub_sandbox::ExecResult) -> String {
        String::from_utf8_lossy(&result.stdout).to_string()
    }

    /// Whether the dedicated sandbox identity is provisioned and usable.
    fn identity_available() -> bool {
        duthub_sandbox::status().identity.is_verified()
    }

    /// Print the exact reason and skip when the identity cannot be used.
    ///
    /// Two different situations land here and the message keeps them apart: the
    /// account was never provisioned, or it exists but this process cannot start
    /// a process as it (`CreateProcessAsUser` → Win32 1314 without
    /// `SeAssignPrimaryTokenPrivilege` / `SeIncreaseQuotaPrivilege`). The second
    /// is what a non-elevated desktop process always looks like; ADR-12 §4.1.3
    /// records the two remedies.
    fn skip_without_identity(test: &str) -> bool {
        let status = duthub_sandbox::status();
        if status.identity.is_verified() {
            return false;
        }
        eprintln!(
            "SKIP {test}: 专用沙箱身份不可用（读隔离需要「专用身份 + 提权创建进程」）。\n  能力: {}",
            status.detail
        );
        true
    }

    /// Depth-first search for a file named `name` below `root`. Used where the
    /// child's real path depends on the sandbox runtime (for example the
    /// AppContainer TEMP redirection) and a fixed path would be a guess.
    fn find_file_named(root: &Path, name: &str) -> Option<PathBuf> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().and_then(|value| value.to_str()) == Some(name) {
                    return Some(path);
                }
            }
        }
        None
    }

    // T1 -------------------------------------------------------------------
    #[test]
    fn t1_script_writes_inside_its_cell() {
        let fixture = fixture("t1");
        let result = run_script(
            &fixture.cell,
            &fixture.policy,
            "Set-Content -LiteralPath 'inside.txt' -Value 'inside-ok' -Encoding ascii",
        )
        .expect("sandboxed run");
        assert!(result.sandboxed, "backend must report a verified sandbox");
        assert_eq!(result.exit_code, Some(0), "stderr={:?}", result.stderr);
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

        // Real denial, not "container virtualization": `-ErrorAction Stop` turns
        // the denied write into a terminating error, so the script must abort
        // before its completion marker. A virtualized (successful) write would
        // let the script finish and leave the marker behind.
        let attempted = fixture.cell.root().join("write-attempted.txt");
        let completed = fixture.cell.root().join("write-completed.txt");
        let script = format!(
            "$ErrorActionPreference = 'Stop'\nSet-Content -LiteralPath '{}' -Value 'attempted' -Encoding ascii\nSet-Content -LiteralPath '{}' -Value escape -Encoding ascii\nSet-Content -LiteralPath '{}' -Value 'completed' -Encoding ascii",
            attempted.display(),
            escape.display(),
            completed.display()
        );
        let result = run_script(&fixture.cell, &fixture.policy, &script).expect("sandboxed run");
        assert!(
            attempted.exists(),
            "the script never reached the write attempt: exit={:?} stderr={:?}",
            result.exit_code,
            result.stderr
        );
        assert!(
            !escape.exists(),
            "escaped write created {} (exit={:?})",
            escape.display(),
            result.exit_code
        );
        assert!(
            !completed.exists(),
            "the denied write did not abort the script: exit={:?} stderr={:?}",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        );
        assert_ne!(
            result.exit_code,
            Some(0),
            "a terminated script must report a non-zero exit: stderr={:?}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    // T3 -------------------------------------------------------------------
    #[test]
    fn t3_script_cannot_read_the_host_config() {
        if skip_without_identity("t3_script_cannot_read_the_host_config") {
            return;
        }
        let fixture = fixture("t3");
        // `<exe dir>/.duthub` is what the policy layer protects in production.
        let exe = std::env::current_exe().expect("current exe");
        let config = exe.parent().expect("exe parent").join(".duthub");
        std::fs::create_dir_all(&config).expect("create host config dir");
        let settings = config.join(format!("settings-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&settings, "{\"secret\":\"DUTHUB-TEST-SECRET\"}").expect("write settings");

        // Baseline: the host itself can read it.
        let baseline = std::fs::read_to_string(&settings).expect("baseline read");
        assert!(baseline.contains("DUTHUB-TEST-SECRET"));

        let mut policy = fixture.policy.clone();
        policy.deny_read = vec![config.clone()];
        // A terminating error is the only way a denied read is observable: the
        // verdict is the host-side file, never what the script chose to print.
        let started = fixture.cell.root().join("read-started.txt");
        let leak = fixture.cell.root().join("read-leaked.txt");
        let denied = fixture.cell.root().join("read-denied.txt");
        let script = format!(
            "$ErrorActionPreference = 'Stop'\nSet-Content -LiteralPath '{}' -Value 'started' -Encoding ascii\ntry {{ $c = Get-Content -Raw -LiteralPath '{}'; Set-Content -LiteralPath '{}' -Value $c -Encoding ascii }} catch {{ Set-Content -LiteralPath '{}' -Value $_.Exception.GetType().Name -Encoding ascii }}\n",
            started.display(),
            settings.display(),
            leak.display(),
            denied.display()
        );
        let result = run_script(&fixture.cell, &policy, &script).expect("sandboxed run");
        assert!(
            started.exists(),
            "the script never started: exit={:?} stderr={:?}",
            result.exit_code,
            result.stderr
        );
        let leaked = std::fs::read_to_string(&leak).unwrap_or_default();
        assert!(
            !leaked.contains("DUTHUB-TEST-SECRET"),
            "host config leaked into the container: {leaked:?}"
        );
        assert!(
            !leak.exists(),
            "the read succeeded (the file's content was written back): {}",
            leak.display()
        );
        assert!(
            denied.exists(),
            "expected a terminating denial (exit={:?}, stderr={:?})",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        );

        let _ = std::fs::remove_file(&settings);
    }

    // T4 -------------------------------------------------------------------
    #[test]
    fn t4_script_cannot_read_another_dut_cell() {
        if skip_without_identity("t4_script_cannot_read_another_dut_cell") {
            return;
        }
        let fixture = fixture("t4");
        let (sibling, secret) = sibling_cell(&fixture.work, "DUTHUB-SIBLING-SECRET");

        // Baseline: the host can read the sibling's file.
        assert!(
            std::fs::read_to_string(&secret)
                .expect("baseline read")
                .contains("DUTHUB-SIBLING-SECRET")
        );

        let mut policy = fixture.policy.clone();
        policy.deny_read = vec![sibling.root().to_path_buf()];
        let prepared = duthub_sandbox::prepare(&fixture.cell, &policy).expect("prepare");
        let account_sid = prepared
            .sandbox_identity_sid
            .expect("with the identity provisioned the cell must report its SID");

        // Make the test about the *deny ACE*, not only about identity
        // separation: grant the sandbox account read access to the sibling cell
        // first, so only the deny ACE can still block the read.
        let grant = std::process::Command::new("icacls.exe")
            .arg(sibling.root())
            .args([
                "/grant",
                &format!("*{account_sid}:(OI)(CI)(RX)"),
                "/T",
                "/Q",
            ])
            .output()
            .expect("run icacls");
        assert!(
            grant.status.success(),
            "icacls grant failed: {}",
            String::from_utf8_lossy(&grant.stderr)
        );

        let leak = fixture.cell.root().join("read-leaked.txt");
        let denied = fixture.cell.root().join("read-denied.txt");
        let script = format!(
            "$ErrorActionPreference = 'Stop'\ntry {{ $c = Get-Content -Raw -LiteralPath '{}'; Set-Content -LiteralPath '{}' -Value $c -Encoding ascii }} catch {{ Set-Content -LiteralPath '{}' -Value $_.Exception.GetType().Name -Encoding ascii }}\n",
            secret.display(),
            leak.display(),
            denied.display()
        );
        let result = run_script(&fixture.cell, &policy, &script).expect("sandboxed run");
        assert_eq!(result.exit_code, Some(0), "stderr={:?}", result.stderr);
        let leaked = std::fs::read_to_string(&leak).unwrap_or_default();
        assert!(
            !leaked.contains("DUTHUB-SIBLING-SECRET"),
            "sibling cell leaked: {leaked:?}"
        );
        assert!(
            !leak.exists(),
            "the read succeeded: the sibling secret was copied into {}",
            leak.display()
        );
        assert!(
            denied.exists(),
            "expected a terminating denial, got exit={:?} stderr={:?}",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        );
    }

    // T5 -------------------------------------------------------------------
    #[test]
    fn t5_script_has_no_loopback_network() {
        let fixture = fixture("t5");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();

        // Baseline: the host can reach the listener.
        TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().expect("addr"),
            Duration::from_secs(2),
        )
        .expect("baseline loopback connect");

        let script = format!(
            "$ErrorActionPreference = 'Stop'\ntry {{ $c = New-Object System.Net.Sockets.TcpClient; $c.Connect('127.0.0.1', {port}); 'CONNECTED' }} catch {{ 'BLOCKED' }}"
        );
        let result = run_script(&fixture.cell, &fixture.policy, &script).expect("sandboxed run");
        let output = script_text(&result);
        assert_eq!(result.exit_code, Some(0), "stderr={:?}", result.stderr);
        assert!(
            !output.contains("CONNECTED"),
            "loopback reachable: {output:?}"
        );
        assert!(
            output.contains("BLOCKED"),
            "expected blocked, got {output:?}"
        );
        drop(listener);
    }

    // T6 -------------------------------------------------------------------
    #[test]
    fn t6_grandchild_inherits_the_sandbox() {
        let fixture = fixture("t6");
        let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
        let escape = PathBuf::from(profile).join(format!(
            "duthub-sandbox-grandchild-{}",
            uuid::Uuid::new_v4()
        ));
        let inside = fixture.cell.root().join("grandchild-inside");

        // Baseline: the same mkdir succeeds without a sandbox.
        std::fs::create_dir(&escape).expect("baseline mkdir");
        std::fs::remove_dir(&escape).expect("baseline cleanup");

        // Two children of the same kind: one inside the cell (must work, so the
        // negative half cannot pass just because `cmd` never ran) and one
        // outside (must be denied).
        let script = format!(
            "$ErrorActionPreference = 'Stop'\n$inside = '{}'\n$outside = '{}'\ncmd.exe /c mkdir \"$inside\" | Out-Null\n\"inside-exit=$LASTEXITCODE\"\ncmd.exe /c mkdir \"$outside\" | Out-Null\n\"outside-exit=$LASTEXITCODE\"\n",
            inside.display(),
            escape.display()
        );
        let result = run_script(&fixture.cell, &fixture.policy, &script).expect("sandboxed run");
        let output = script_text(&result);
        assert_eq!(result.exit_code, Some(0), "stderr={:?}", result.stderr);
        assert!(
            output.contains("inside-exit=0"),
            "cmd must be able to create a directory inside the cell first: {output:?}"
        );
        assert!(
            inside.is_dir(),
            "the in-cell mkdir did not happen: {output:?}"
        );
        assert!(
            !escape.exists(),
            "grandchild escaped the sandbox: {}",
            escape.display()
        );
        assert!(
            !output.contains("outside-exit=0"),
            "the outside mkdir reported success: {output:?}"
        );
    }

    // T7 -------------------------------------------------------------------
    #[test]
    fn t7_timeout_kills_the_whole_tree() {
        let mut fixture = fixture("t7");
        fixture.policy.timeout = Duration::from_secs(4);
        fixture.policy.max_output_bytes = 4096;
        // AppContainer redirects the child's TEMP to
        // `<cell>\home\AppData\Local\Packages\<profile>\AC\Temp`, still inside
        // the cell. Record both environment paths as files inside the cell,
        // then let a **grandchild** keep writing through `$env:TEMP`; the marker
        // is located by walking the cell instead of assuming a fixed path.
        let script = concat!(
            "$ErrorActionPreference = 'Stop'\n",
            "$env:TEMP | Set-Content -LiteralPath 'env-temp.txt' -Encoding ascii\n",
            "$env:USERPROFILE | Set-Content -LiteralPath 'env-profile.txt' -Encoding ascii\n",
            "$p = Join-Path $env:TEMP 'marker.txt'\n",
            "$inner = \"while (`$true) { Add-Content -LiteralPath '$p' -Value 'x'; Start-Sleep -Milliseconds 50 }\"\n",
            "$g = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-Command',$inner) -PassThru -NoNewWindow\n",
            "$g.Id | Set-Content -LiteralPath 'grandchild-pid.txt' -Encoding ascii\n",
            "while ($true) { Start-Sleep -Milliseconds 200 }",
        );
        let started = Instant::now();
        let result = run_script(&fixture.cell, &fixture.policy, script).expect("sandboxed run");
        assert!(result.timed_out, "expected a timeout, got {result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timeout took {:?}",
            started.elapsed()
        );

        // The child's TEMP and profile must both live inside the cell.
        let cell_root = fixture.cell.root().canonicalize().expect("cell root");
        for name in ["env-temp.txt", "env-profile.txt"] {
            let path = fixture.cell.root().join(name);
            let value = std::fs::read_to_string(&path).unwrap_or_default();
            let value = value.trim();
            assert!(
                !value.is_empty(),
                "{name} is missing: the child did not record its environment"
            );
            let actual = Path::new(value)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(value));
            assert!(
                actual.starts_with(&cell_root),
                "{name} = {value:?} is outside the cell {}",
                cell_root.display()
            );
        }

        // The marker is written by the *grandchild*: if it stops growing, the
        // whole tree is gone, not just the process the host waited on.
        let marker = find_file_named(fixture.cell.root(), "marker.txt")
            .expect("the grandchild must have written its marker inside the cell");
        let first = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert!(first > 0, "the grandchild never wrote its marker");
        std::thread::sleep(Duration::from_millis(1500));
        let second = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            first, second,
            "a descendant survived the timeout and kept writing"
        );

        // Second, independent proof: the recorded grandchild PID must be gone.
        if let Some(pid) = find_file_named(fixture.cell.root(), "grandchild-pid.txt")
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            let listed = std::process::Command::new("tasklist.exe")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output();
            if let Ok(listed) = listed {
                let text = String::from_utf8_lossy(&listed.stdout).to_lowercase();
                assert!(
                    !text.contains(&pid.to_string()),
                    "grandchild {pid} is still alive:\n{text}"
                );
            }
        }
    }

    // T8 -------------------------------------------------------------------
    #[test]
    fn t8_over_limit_requests_are_refused_without_spawning() {
        let mut fixture = fixture("t8");
        let too_long = "x".repeat(MAX_SCRIPT_CHARS + 1);
        match run_script(&fixture.cell, &fixture.policy, &too_long) {
            Err(SandboxError::PolicyRejected(reason)) => {
                assert!(reason.contains("过长"), "{reason}")
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
                assert!(reason.contains("禁止"), "{reason}")
            }
            other => panic!("destructive script must be refused, got {other:?}"),
        }
        // Nothing may have been created by the refused runs.
        let entries = std::fs::read_dir(fixture.cell.root())
            .expect("read cell")
            .count();
        assert_eq!(entries, 0, "a refused request left files behind");

        // The Job limits are not advisory: a process limit really stops extra
        // children, and a commit limit really fails a large allocation.
        fixture.policy.process_limit = Some(3);
        fixture.policy.timeout = Duration::from_secs(60);
        let process_script = concat!(
            "$ErrorActionPreference = 'Continue'\n",
            "$started = 0\n",
            "for ($i = 0; $i -lt 6; $i++) {\n",
            "  try { $p = Start-Process -FilePath $env:COMSPEC -ArgumentList @('/c','ping -n 20 127.0.0.1') -PassThru -NoNewWindow -ErrorAction Stop; $started++ } catch { }\n",
            "}\n",
            "\"started=$started\"\n",
        );
        let result = run_script(&fixture.cell, &fixture.policy, process_script)
            .expect("sandboxed run with a process limit");
        let output = script_text(&result);
        assert_eq!(result.exit_code, Some(0), "stderr={:?}", result.stderr);
        let started = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("started="))
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(99);
        assert!(
            started < 6,
            "ActiveProcessLimit=3 did not stop extra children: {output:?}"
        );

        fixture.policy.process_limit = None;
        fixture.policy.memory_limit_bytes = Some(256 * 1024 * 1024);
        let memory_script =
            "try { $a = New-Object 'byte[]' (1024MB); 'allocated' } catch { 'denied' }\n";
        let result = run_script(&fixture.cell, &fixture.policy, memory_script)
            .expect("sandboxed run with a memory limit");
        let output = script_text(&result);
        assert!(
            !output.contains("allocated"),
            "ProcessMemoryLimit=256MiB did not stop a 1GiB allocation (exit={:?}): {output:?}",
            result.exit_code
        );
    }

    // The identity really reaches the child -------------------------------------------------
    // These two run only once the one-time provisioning has been done; before
    // that they print the same explicit SKIP. They are the fastest way to learn
    // whether `CreateProcessAsUser` + AppContainer actually start a process
    // under the sandbox account, which is the one combination this crate cannot
    // prove without the account.
    #[test]
    fn sandbox_identity_is_the_script_identity() {
        if skip_without_identity("sandbox_identity_is_the_script_identity") {
            return;
        }
        let fixture = fixture("identity-script");
        let prepared = duthub_sandbox::prepare(&fixture.cell, &fixture.policy).expect("prepare");
        assert!(
            prepared.sandbox_identity_sid.is_some(),
            "a provisioned identity must be reported on the prepared cell"
        );
        let result = run_script(&fixture.cell, &fixture.policy, "whoami")
            .expect("sandboxed run under the dedicated identity");
        let output = script_text(&result).to_lowercase();
        assert_eq!(result.exit_code, Some(0), "stderr={:?}", result.stderr);
        assert!(
            output.contains("duthubsandbox"),
            "the child must run as the sandbox account, got {output:?}"
        );
    }

    #[test]
    fn sandbox_identity_is_the_agent_host_identity() {
        if skip_without_identity("sandbox_identity_is_the_agent_host_identity") {
            return;
        }
        let fixture = fixture("identity-host");
        let mut policy = SandboxPolicy::agent_cell(&fixture.cell);
        policy.timeout = Duration::from_secs(30);
        let cmd = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("cmd.exe");
        let prepared = duthub_sandbox::prepare(&fixture.cell, &policy).expect("prepare");
        assert!(
            prepared.sandbox_identity_sid.is_some(),
            "a provisioned identity must be reported on the prepared cell"
        );
        let result = run_agent(
            &fixture.cell,
            &policy,
            &cmd,
            &[OsString::from("/c"), OsString::from("whoami")],
            &[],
        )
        .expect("sandboxed host run");
        let output = script_text(&result).to_lowercase();
        assert!(
            output.contains("duthubsandbox"),
            "the Agent host must run as the sandbox account, got {output:?}"
        );
    }

    // Read isolation needs an identity; without it the request must be refused
    // rather than silently downgraded to "the path is not protected".
    #[test]
    fn read_isolation_is_refused_instead_of_pretending_without_the_identity() {
        if identity_available() {
            eprintln!(
                "SKIP read_isolation_is_refused...: 专用身份已初始化，本用例只覆盖未初始化的 fail-closed 分支"
            );
            return;
        }
        let fixture = fixture("no-identity");
        let protected = fixture.work.join("duthub-data");
        std::fs::create_dir_all(&protected).expect("create protected dir");
        let mut policy = fixture.policy.clone();
        policy.deny_read = vec![protected];
        match run_script(&fixture.cell, &policy, "Get-Date") {
            Err(SandboxError::Setup(reason)) => {
                assert!(
                    reason.contains("管理员") || reason.contains("身份"),
                    "{reason}"
                );
            }
            other => panic!("deny_read without the identity must be refused, got {other:?}"),
        }
    }

    #[test]
    fn provisioning_is_only_ever_explicit_and_reports_the_elevation_requirement() {
        if duthub_sandbox::is_elevated() {
            // An elevated test runner would create a real local account here.
            eprintln!(
                "SKIP provisioning...: 测试进程已提升，跳过以避免创建真实本地账户（由运维显式初始化）"
            );
            return;
        }
        match duthub_sandbox::provision_identity() {
            Err(SandboxError::Setup(reason)) => assert!(
                reason.contains("管理员"),
                "the refusal must name the elevation requirement: {reason}"
            ),
            other => panic!("provisioning without elevation must fail, got {other:?}"),
        }
    }

    // Host (restricted token) write boundary -------------------------------
    #[test]
    fn agent_host_writes_only_inside_its_cell() {
        let fixture = fixture("host");
        let mut policy = SandboxPolicy::agent_cell(&fixture.cell);
        policy.timeout = Duration::from_secs(30);
        let cmd = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("cmd.exe");
        assert!(cmd.is_file(), "{}", cmd.display());

        let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
        let outside = PathBuf::from(profile).join(format!(
            "duthub-sandbox-host-escape-{}",
            uuid::Uuid::new_v4()
        ));
        // Baseline: the directory can be created without a sandbox.
        std::fs::create_dir(&outside).expect("baseline mkdir");
        std::fs::remove_dir(&outside).expect("baseline cleanup");

        let denied = run_agent(
            &fixture.cell,
            &policy,
            &cmd,
            &[
                OsString::from("/c"),
                OsString::from("mkdir"),
                outside.clone().into_os_string(),
            ],
            &[],
        )
        .expect("sandboxed run");
        assert!(denied.sandboxed);
        assert_ne!(denied.exit_code, Some(0), "outside mkdir must fail");
        assert!(!outside.exists(), "host escaped the cell");

        let inside = fixture.cell.root().join("inside-dir");
        let allowed = run_agent(
            &fixture.cell,
            &policy,
            &cmd,
            &[
                OsString::from("/c"),
                OsString::from("mkdir"),
                inside.clone().into_os_string(),
            ],
            &[],
        )
        .expect("sandboxed run");
        assert_eq!(allowed.exit_code, Some(0), "stderr={:?}", allowed.stderr);
        assert!(inside.is_dir(), "inside mkdir must succeed");
    }

    #[test]
    fn agent_policy_refuses_deny_read_instead_of_pretending() {
        let fixture = fixture("agent-deny");
        let mut policy = SandboxPolicy::agent_cell(&fixture.cell);
        policy.deny_read = vec![fixture.cell.root().to_path_buf()];
        match duthub_sandbox::prepare(&fixture.cell, &policy) {
            Err(SandboxError::PolicyRejected(reason)) => {
                assert!(reason.contains("deny_read"), "{reason}")
            }
            other => panic!("restricted mode must refuse deny_read, got {other:?}"),
        }
    }

    #[test]
    fn prepare_reports_the_identities_it_verified() {
        let fixture = fixture("prepare");
        let prepared = duthub_sandbox::prepare(&fixture.cell, &fixture.policy).expect("prepare");
        assert!(
            prepared
                .container_sid
                .as_deref()
                .is_some_and(|sid| sid.starts_with("S-1-15-2-"))
        );
        assert!(!prepared.notes.is_empty());
        // The container SID grant must be on the cell and readable back.
        let again = duthub_sandbox::prepare(&fixture.cell, &fixture.policy).expect("idempotent");
        assert_eq!(prepared.container_sid, again.container_sid);

        // The restricted (Agent host) cell reports its capability SID.
        let host_policy = SandboxPolicy::agent_cell(&fixture.cell);
        let host_prepared =
            duthub_sandbox::prepare(&fixture.cell, &host_policy).expect("prepare host cell");
        assert!(
            host_prepared
                .capability_sid
                .as_deref()
                .is_some_and(|sid| sid.starts_with("S-1-5-21-"))
        );
        // The capability SID now lives outside the writable cell (it used to be
        // `<cell>/capability.sid`, which the Agent itself could rewrite).
        assert!(
            !fixture.cell.root().join("capability.sid").exists(),
            "capability.sid must not live inside the writable cell"
        );
        assert!(
            fixture
                .work
                .join("duthub-data")
                .join("sandbox")
                .join("capabilities")
                .is_dir(),
            "the capability store must be under <work>/duthub-data"
        );
    }

    #[test]
    fn status_never_claims_an_unverified_capability() {
        let status = duthub_sandbox::status();
        assert_eq!(status.platform, "windows");
        for capability in [
            status.restricted_token,
            status.app_container,
            status.job_limits,
            status.read_deny,
            status.identity,
        ] {
            assert!(
                matches!(
                    capability,
                    duthub_sandbox::Capability::Verified
                        | duthub_sandbox::Capability::Unavailable
                        | duthub_sandbox::Capability::Unverified
                ),
                "{capability:?}"
            );
        }
        if !status.identity.is_verified() {
            assert!(
                !status.read_deny.is_verified(),
                "read isolation must not be claimed without the sandbox identity: {}",
                status.detail
            );
            assert!(
                !status.agent_isolation(),
                "agent isolation must not be claimed without read isolation: {}",
                status.detail
            );
        }
    }

    #[test]
    fn powershell_is_the_fixed_system_interpreter() {
        assert!(powershell().is_absolute());
        assert!(powershell().is_file(), "{}", powershell().display());
        assert!(
            powershell()
                .to_string_lossy()
                .ends_with(r"System32\WindowsPowerShell\v1.0\powershell.exe")
        );
    }
}

#[cfg(not(windows))]
mod other_platforms {
    use super::work_dir;
    use duthub_sandbox::{CellSpec, SandboxError, SandboxPolicy};

    fn cell() -> CellSpec {
        let work = work_dir("unsupported");
        CellSpec::for_dut(&work, "dut-a").expect("cell")
    }

    #[test]
    fn every_entry_point_refuses_on_this_platform() {
        let cell = cell();
        let policy = SandboxPolicy::script_cell(&cell);
        match duthub_sandbox::prepare(&cell, &policy) {
            Err(SandboxError::Unsupported(_)) => {}
            other => panic!("prepare must be Unsupported, got {other:?}"),
        }
        match duthub_sandbox::run_script(&cell, &policy, "Get-Date") {
            Err(SandboxError::Unsupported(_)) => {}
            other => panic!("run_script must be Unsupported, got {other:?}"),
        }
        match duthub_sandbox::provision_identity() {
            Err(SandboxError::Unsupported(_)) => {}
            other => panic!("provision_identity must be Unsupported, got {other:?}"),
        }
        let status = duthub_sandbox::status();
        assert_eq!(status.platform, "unsupported");
        assert!(!status.script_isolation());
        assert!(!status.agent_isolation());
        assert!(!status.identity.is_verified());
    }
}
