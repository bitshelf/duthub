//! F3/F4/F5/F8：goose 会话生命周期回归。
//!
//! - start→stop→立即 start 不得出现两个 goose（同 DUT 共享 `GOOSE_PATH_ROOT`）；
//! - 失败/终止后可以重启；
//! - stop 不在 UI 线程做 stdin I/O；
//! - MCP 不可达时扩展校验拒绝（F1 负向）；
//! - 停止时孙进程随进程树消失（F5）。

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings};
use duthub_core::agent_config::AgentDefinition;
use duthub_core::lease::{ControlLeaseManager, WriteIdentity};
use duthub_core::{ApprovalError, AuditSink};
use duthub_desktop::agent_host::acp::{AcpClient, McpServerRef};
use duthub_desktop::agent_host::host::{
    HostError, HostEvents, HostManager, validate_extension_results,
};
use duthub_desktop::agent_host::model_env::{self, GooseBinding};
use duthub_desktop::agent_host::registry;
use duthub_desktop::agent_host::sandbox;
use duthub_desktop::agent_host::session::SessionStatus;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, DutInfo, DutSummary, LogReadResult, McpState,
    ProcessResultDto,
};

struct StubState;

impl McpState for StubState {
    fn list_duts(&self) -> Vec<DutSummary> {
        Vec::new()
    }

    fn dut_info(&self, _dut_id: &str) -> Option<DutInfo> {
        None
    }

    fn serial_read(&self, _dut_id: &str, _since_seq: u64, _limit: usize) -> Option<LogReadResult> {
        None
    }

    fn serial_write(
        &self,
        _dut_id: &str,
        _text: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub".into())
    }

    fn adb_list(&self) -> Vec<AdbDeviceDto> {
        Vec::new()
    }

    fn adb_shell(
        &self,
        _dut_id: &str,
        _argv: &[String],
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("stub".into())
    }

    fn control_power(
        &self,
        _dut_id: &str,
        _on: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub".into())
    }

    fn control_reset(
        &self,
        _dut_id: &str,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub".into())
    }

    fn control_power_key(
        &self,
        _dut_id: &str,
        _long_press: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub".into())
    }

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("stub".into())
    }

    fn approval_issue(&self, dut_id: &str, action: &str, _args_json: &str) -> ApprovalIssueResult {
        ApprovalIssueResult {
            token: "stub".into(),
            dut_id: dut_id.into(),
            action: action.into(),
            expires_in_secs: 0,
        }
    }

    fn approval_validate(
        &self,
        _token: &str,
        _dut_id: &str,
        _action: &str,
        _args_json: &str,
    ) -> Result<(), ApprovalErr> {
        Err(ApprovalErr {
            kind: ApprovalError::Missing,
        })
    }
}

#[derive(Default)]
struct Events {
    exited: Mutex<Vec<(String, Option<i32>)>>,
    failed: Mutex<Vec<(String, String)>>,
}

impl HostEvents for Events {
    fn on_started(&self, _dut: &str, _session: &str) {}

    fn on_exited(&self, dut: &str, code: Option<i32>) {
        self.exited.lock().unwrap().push((dut.to_owned(), code));
    }

    fn on_failed(&self, dut: &str, message: &str) {
        self.failed
            .lock()
            .unwrap()
            .push((dut.to_owned(), message.to_owned()));
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("duthub-goose-life-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn access_context() -> Arc<AccessContext> {
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let lease = Arc::new(ControlLeaseManager::new(None));
    let audit = AuditSink::memory();
    Arc::new(AccessContext::new(auth, lease, audit))
}

fn binding() -> GooseBinding {
    GooseBinding {
        provider: duthub_core::Provider::Openai,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "fake-model".into(),
        api_key: "sk-lifecycle-secret".into(),
    }
}

// ---------------------------------------------------------------------------
// Test goose fixture (2026-11-24: goose is downloaded, no longer embedded)
// ---------------------------------------------------------------------------

/// `DUTHUB_TEST_GOOSE` (absolute path to a goose executable) wins, otherwise the
/// product resolver decides (downloaded install > PATH). `None` = not installed.
fn test_goose_program() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("DUTHUB_TEST_GOOSE") {
        let path = PathBuf::from(raw);
        assert!(
            path.is_absolute() && path.is_file(),
            "DUTHUB_TEST_GOOSE 必须是存在的 goose 可执行文件绝对路径: {}",
            path.display()
        );
        return Some(path);
    }
    registry::goose_install_state()
        .program()
        .map(std::path::Path::to_path_buf)
}

/// External-program definition that runs the test goose through the same
/// fail-closed path as production (absolute path + pinned SHA-256).
fn test_goose_agent() -> Option<AgentDefinition> {
    let program = test_goose_program()?;
    let sha256 = duthub_core::store::sha256_file(&program).ok()?;
    let mut agent = AgentDefinition::goose();
    agent.program = Some(program.display().to_string());
    agent.expected_sha256 = Some(sha256);
    Some(agent)
}

/// Loud skip: goose is no longer embedded, so ACP tests need an installed one.
fn skip_without_goose() {
    eprintln!(
        "SKIP: 未安装 goose（内嵌资产已于 2026-11-24 移除）——先在「工具下载」页下载 goose，\
         或设置 DUTHUB_TEST_GOOSE 指向 goose 可执行文件"
    );
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

fn wait_ready(host: &mut HostManager, dut: &str) -> bool {
    wait_until(Duration::from_secs(120), || {
        host.tick();
        host.view(dut)
            .is_some_and(|view| matches!(view.status(), SessionStatus::Ready))
    })
}

fn wait_terminal(host: &mut HostManager, dut: &str, timeout: Duration) -> bool {
    wait_until(timeout, || {
        host.tick();
        host.view(dut)
            .is_some_and(|view| view.status().is_terminal())
    })
}

#[test]
fn start_stop_restart_serializes_sessions() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("restart");
    let events = Arc::new(Events::default());
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(StubState),
        access_context(),
        Some(events.clone()),
    );

    host.start_with_agent("dut-life", Some(binding()), agent.clone())
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-life"),
        "会话未就绪：{:?} {}",
        host.view("dut-life").map(|view| view.status()),
        host.view("dut-life")
            .map_or_default(|view| view.stderr_tail())
    );
    let first_session = host.view("dut-life").unwrap().session_id().unwrap();

    // stop 必须在 UI 线程立即返回（不做 stdin 写/等待）。
    let started = Instant::now();
    host.stop("dut-life");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "stop 不应阻塞 UI：{elapsed:?}"
    );

    // 同一 DUT 在旧 worker 收尾前不允许再启动（否则两个 goose 共享 GOOSE_PATH_ROOT）。
    match host.start_with_agent("dut-life", Some(binding()), agent.clone()) {
        Err(HostError::AlreadyRunning(dut)) => assert_eq!(dut, "dut-life"),
        Ok(()) => panic!("旧会话收尾前不得启动第二个 goose"),
        Err(error) => panic!("unexpected start error: {error}"),
    }

    assert!(
        wait_terminal(&mut host, "dut-life", Duration::from_secs(30)),
        "stop 后未进入终止态"
    );

    // 等 join 完成后允许重启；新的 session id 证明是全新会话。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        host.tick();
        match host.start_with_agent("dut-life", Some(binding()), agent.clone()) {
            Ok(()) => break,
            Err(HostError::AlreadyRunning(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("failed 后无法重启: {error}"),
        }
    }
    assert!(
        wait_ready(&mut host, "dut-life"),
        "重启后未就绪：{:?}",
        host.view("dut-life").map(|view| view.status())
    );
    let second_session = host.view("dut-life").unwrap().session_id().unwrap();
    assert_ne!(first_session, second_session);

    host.stop("dut-life");
    assert!(wait_terminal(
        &mut host,
        "dut-life",
        Duration::from_secs(30)
    ));
    assert!(
        !events.exited.lock().unwrap().is_empty(),
        "on_exited 必须回调"
    );
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn failed_session_can_restart_after_work_dir_is_fixed() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("failed-restart");
    let blocked = work.join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let events = Arc::new(Events::default());
    let mut host = HostManager::new(
        blocked.clone(),
        Arc::new(StubState),
        access_context(),
        Some(events.clone()),
    );

    host.start_with_agent("dut-failed", Some(binding()), agent.clone())
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(60), || {
            host.tick();
            matches!(
                host.view("dut-failed").map(|view| view.status()),
                Some(SessionStatus::Failed(_))
            )
        }),
        "启动失败必须进入 Failed"
    );
    assert!(
        !events.failed.lock().unwrap().is_empty(),
        "启动失败必须回调 on_failed"
    );

    // 修复工作目录后立即重启（entry 可能仍在 join，start 会返回 AlreadyRunning）。
    std::fs::remove_file(&blocked).unwrap();
    std::fs::create_dir_all(&blocked).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        host.tick();
        match host.start_with_agent("dut-failed", Some(binding()), agent.clone()) {
            Ok(()) => break,
            Err(HostError::AlreadyRunning(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("Failed 后无法重启: {error}"),
        }
    }
    assert!(
        wait_ready(&mut host, "dut-failed"),
        "修复后未就绪：{:?}",
        host.view("dut-failed").map(|view| view.status())
    );
    host.stop("dut-failed");
    assert!(wait_terminal(
        &mut host,
        "dut-failed",
        Duration::from_secs(30)
    ));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn mcp_unreachable_is_rejected_by_extension_validation() {
    let work = temp_dir("unreachable");
    let paths = sandbox::prepare(&work, "dut-unreachable").unwrap();
    model_env::write_config(&paths.goose_root).unwrap();
    let env = model_env::goose_env(&binding(), &paths.goose_root).unwrap();
    let Some(exe) = test_goose_program() else {
        skip_without_goose();
        return;
    };
    let acp = AcpClient::spawn(&exe, &[], &paths.sandbox, &env, 64 * 1024).unwrap();
    let initialize = acp.initialize(Duration::from_secs(20)).unwrap();
    assert_eq!(initialize.protocol_version, 1);

    let outcome = acp.new_session(
        &paths.sandbox,
        &McpServerRef {
            name: "duthub",
            url: "http://127.0.0.1:1/mcp",
            token: "dth-unreachable-token",
        },
        Duration::from_secs(30),
    );
    match outcome {
        Ok(session) => {
            let entries = session.extension_results.as_array().unwrap();
            assert!(
                entries
                    .iter()
                    .any(|entry| entry["name"] == "duthub" && entry["success"] == false),
                "不可达 MCP 的扩展结果必须失败：{}",
                session.extension_results
            );
            assert!(
                validate_extension_results(&session.extension_results).is_err(),
                "不可达 MCP 必须被启动强校验拒绝"
            );
        }
        Err(error) => {
            // goose 也可能直接在 session/new 返回错误；两种路径都必须是失败。
            let text = error.to_string();
            assert!(
                text.contains("MCP") || text.contains("mcp") || text.contains("session/new"),
                "unexpected error: {text}"
            );
        }
    }

    acp.close_stdin();
    let _ = acp.wait_timeout(Duration::from_secs(10));
    acp.kill();
    let _ = std::fs::remove_dir_all(&work);
}

/// F5: `AcpClient::kill` must terminate descendant processes via the MXC
/// sandbox Job tree, not just the direct child.
#[test]
fn kill_terminates_grandchild_process() {
    let work = temp_dir("tree");
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
    let script = work.join("spawn-tree.cmd");
    std::fs::write(
        &script,
        "@echo off\r\ncmd.exe /D /Q /C grandchild-loop.cmd\r\n",
    )
    .unwrap();
    let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("cmd.exe");
    let acp = AcpClient::spawn(
        &cmd,
        &[
            "/D".into(),
            "/Q".into(),
            "/C".into(),
            script.to_string_lossy().into_owned(),
        ],
        &work,
        &[],
        4096,
    )
    .unwrap();

    assert!(
        wait_until(Duration::from_secs(15), || {
            std::fs::metadata(&marker).map_or(0, |m| m.len()) > 0
        }),
        "grandchild never wrote marker: {}",
        marker.display()
    );
    let root = acp.pid();

    acp.kill();
    assert!(
        wait_until(Duration::from_secs(15), || !process_alive(root)),
        "root process still alive after kill: {root}"
    );
    std::thread::sleep(Duration::from_millis(250));
    let after_kill = std::fs::metadata(&marker).map_or(0, |m| m.len());
    std::thread::sleep(Duration::from_millis(600));
    let later = std::fs::metadata(&marker).map_or(0, |m| m.len());
    assert_eq!(
        after_kill, later,
        "grandchild kept writing after AcpClient::kill() ({after_kill} -> {later})"
    );
    drop(acp);
    let _ = std::fs::remove_dir_all(&work);
}

fn process_alive(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/NH"])
        .output()
    else {
        return false;
    };
    output.status.success() && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}
