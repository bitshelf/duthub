//! W2b Agent restart regressions.
//!
//! - A session that never reached `Ready` is not restarted (startup failure).
//! - A session that reached `Ready` and exited on its own is restarted at most
//!   three times per ten-minute window (1s/2s/4s backoff), then `Failed`.
//! - `HostManager::stop` cancels a pending restart without spawning a new
//!   process.
//!
//! The fake ACP program is a PowerShell script that answers `initialize` and
//! `session/new` over stdin/stdout and then exits 0 (or exits immediately in
//! `fail-early` mode). Every spawn appends one line to a counter file, so the
//! tests can prove exactly how many processes were started.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings};
use duthub_core::agent_config::{AgentDefinition, AgentKind};
use duthub_core::lease::{ControlLeaseManager, WriteIdentity};
use duthub_core::restart::MAX_RESTARTS_PER_WINDOW;
use duthub_core::{ApprovalError, AuditSink};
use duthub_desktop::agent_host::host::{HostEvents, HostManager};
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
    started: Mutex<Vec<String>>,
    exited: Mutex<Vec<(String, Option<i32>)>>,
    failed: Mutex<Vec<(String, String)>>,
}

impl HostEvents for Events {
    fn on_started(&self, dut: &str, _session: &str) {
        self.started.lock().unwrap().push(dut.to_owned());
    }

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

/// Fake ACP agent written as a pure `cmd.exe` batch script so it runs inside
/// MXC PSEC 1.1 on both interactive and headless/isolated CI desktops without
/// linking `user32.dll`.
///
/// - `fail-early`: records a spawn line and exits 3 before answering `initialize`.
/// - other modes: records a spawn line, answers `initialize` (id=1) and
///   `session/new` (id=2), briefly stays alive so callers observe `Ready`, then
///   exits 0.
const FAKE_ACP_FAIL_EARLY_CMD: &str = "@echo off\r\n\
echo spawn>>\"%~1\"\r\n\
exit /b 3\r\n";

const FAKE_ACP_CRASH_AFTER_READY_CMD: &str = "@echo off\r\n\
echo spawn>>\"%~1\"\r\n\
set \"line1=\"\r\n\
set /p \"line1=\" || exit /b 1\r\n\
echo {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentInfo\":{\"name\":\"fake-acp\",\"version\":\"0.0.1\"},\"agentCapabilities\":{\"loadSession\":false,\"mcpCapabilities\":{\"http\":true}}}}\r\n\
set \"line2=\"\r\n\
set /p \"line2=\" || exit /b 1\r\n\
echo {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"fake-session-1\",\"_meta\":{\"extensionResults\":[{\"name\":\"duthub\",\"success\":true}]}}}\r\n\
ping -n 2 127.0.0.1 >nul\r\n\
exit /b 0\r\n";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-agent-restart-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn access_context() -> Arc<AccessContext> {
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let lease = Arc::new(ControlLeaseManager::new(None));
    Arc::new(AccessContext::new(auth, lease, AuditSink::memory()))
}

fn cmd_exe() -> PathBuf {
    let root =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from("C:/Windows"), PathBuf::from);
    root.join("System32").join("cmd.exe")
}

/// Build an external ACP definition running the fake batch script inside the
/// DUT's sandbox cell.
fn fake_agent(work: &Path, dut: &str, mode: &str, counter: &Path) -> AgentDefinition {
    let program = cmd_exe();
    assert!(program.is_file(), "缺少 cmd.exe: {}", program.display());
    let key = duthub_core::store::agent_dut_key(dut).expect("DUT id must map to a key");
    let cell_dir = work.join("agent").join("dut").join(key).join("sandbox");
    std::fs::create_dir_all(&cell_dir).unwrap();
    let script = cell_dir.join(format!("fake-acp-{mode}.cmd"));
    let body = if mode == "fail-early" {
        FAKE_ACP_FAIL_EARLY_CMD
    } else {
        FAKE_ACP_CRASH_AFTER_READY_CMD
    };
    std::fs::write(&script, body).unwrap();
    AgentDefinition {
        id: format!("fake-{mode}"),
        name: format!("Fake ACP {mode}"),
        kind: AgentKind::Acp,
        program: Some(program.to_string_lossy().into_owned()),
        args: vec![
            "/D".into(),
            "/Q".into(),
            "/C".into(),
            script.to_string_lossy().into_owned(),
            counter.to_string_lossy().into_owned(),
        ],
        env_keys: Vec::new(),
        cli_profile: None,
        enabled: true,
        auth_mode: "inherit".into(),
        trust_level: "mcp_only".into(),
        expected_sha256: Some(duthub_core::store::sha256_file(&program).unwrap()),
    }
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

fn status_of(host: &HostManager, dut: &str) -> Option<SessionStatus> {
    host.view(dut).map(|view| view.status())
}

fn spawn_count(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

/// Where the fake agent's spawn marker must live: inside the DUT's writable
/// sandbox cell. Since ADR-12 the Agent runs under a WRITE_RESTRICTED token, so
/// a counter next to the script (outside the cell) is denied by the kernel —
/// the fixture has to write where the real Agent may write.
fn sandbox_counter(work: &Path, dut: &str, mode: &str) -> PathBuf {
    let key = duthub_core::store::agent_dut_key(dut).expect("DUT id must map to a key");
    work.join("agent")
        .join("dut")
        .join(key)
        .join("sandbox")
        .join(format!("spawn-{mode}.txt"))
}

#[test]
fn restart_only_after_ready() {
    let work = temp_dir("ready");
    let events = Arc::new(Events::default());
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(StubState),
        access_context(),
        Some(events.clone()),
    );

    // The Agent starts but never answers initialize: startup failure, no restart.
    let early_counter = sandbox_counter(&work, "dut-no-ready", "fail-early");
    host.start_with_agent(
        "dut-no-ready",
        None,
        fake_agent(&work, "dut-no-ready", "fail-early", &early_counter),
    )
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(60), || matches!(
            status_of(&host, "dut-no-ready"),
            Some(SessionStatus::Failed(_))
        )),
        "initialize 失败必须进入 Failed：{:?}",
        status_of(&host, "dut-no-ready")
    );
    // Longer than the first restart delay: a failed startup must not retry.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(spawn_count(&early_counter), 1, "启动失败不得自动重启");
    assert!(
        events
            .failed
            .lock()
            .unwrap()
            .iter()
            .any(|(dut, _)| dut == "dut-no-ready"),
        "启动失败必须回调 on_failed"
    );

    // Ready then self-exit: initial start + three restarts, then Failed.
    let crash_counter = sandbox_counter(&work, "dut-crash", "crash-after-ready");
    host.start_with_agent(
        "dut-crash",
        None,
        fake_agent(&work, "dut-crash", "crash-after-ready", &crash_counter),
    )
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(120), || matches!(
            status_of(&host, "dut-crash"),
            Some(SessionStatus::Failed(_))
        )),
        "重启用尽后必须 Failed：{:?} stderr={}",
        status_of(&host, "dut-crash"),
        host.view("dut-crash")
            .map(|view| view.stderr_tail())
            .unwrap_or_default()
    );
    assert_eq!(
        spawn_count(&crash_counter),
        MAX_RESTARTS_PER_WINDOW + 1,
        "首次启动 + 3 次自动重启"
    );
    let started = events
        .started
        .lock()
        .unwrap()
        .iter()
        .filter(|dut| *dut == "dut-crash")
        .count();
    assert_eq!(started, MAX_RESTARTS_PER_WINDOW + 1);
    let failed = events
        .failed
        .lock()
        .unwrap()
        .iter()
        .filter(|(dut, _)| dut == "dut-crash")
        .count();
    assert_eq!(failed, 1, "只有预算耗尽才是最终 Failed");

    let view = host.view("dut-crash").unwrap();
    let transcript = view.transcript_snapshot();
    assert_eq!(
        transcript
            .iter()
            .filter(|entry| entry.text.contains("秒后自动重启"))
            .count(),
        MAX_RESTARTS_PER_WINDOW,
        "每次重启前写一条系统注记"
    );
    assert!(
        transcript
            .iter()
            .any(|entry| entry.text.contains("已停止自动重启")),
        "预算耗尽必须留下注记"
    );

    // The restart path must not leave a restartable view behind.
    assert!(matches!(
        view.status(),
        SessionStatus::Failed(message) if message.contains("已停止自动重启")
    ));

    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn stop_cancels_pending_restart_without_new_spawn() {
    let work = temp_dir("stop");
    let events = Arc::new(Events::default());
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(StubState),
        access_context(),
        Some(events.clone()),
    );

    let counter = sandbox_counter(&work, "dut-stop", "crash-after-ready");
    host.start_with_agent(
        "dut-stop",
        None,
        fake_agent(&work, "dut-stop", "crash-after-ready", &counter),
    )
    .unwrap();
    wait_until(Duration::from_secs(60), || {
        matches!(
            status_of(&host, "dut-stop"),
            Some(SessionStatus::Ready | SessionStatus::Failed(_))
        )
    });
    assert!(
        matches!(status_of(&host, "dut-stop"), Some(SessionStatus::Ready)),
        "会话未就绪：{:?} stderr={}",
        status_of(&host, "dut-stop"),
        host.view("dut-stop")
            .map(|view| view.stderr_tail())
            .unwrap_or_default()
    );
    // After the fake exits, the worker settles in Stopped and then resets to
    // Starting while it waits out the 1s backoff.
    assert!(
        wait_until(Duration::from_secs(30), || matches!(
            status_of(&host, "dut-stop"),
            Some(SessionStatus::Starting)
        )),
        "未进入重启退避：{:?}",
        status_of(&host, "dut-stop")
    );

    let stop_started = Instant::now();
    host.stop("dut-stop");
    assert!(
        stop_started.elapsed() < Duration::from_secs(2),
        "stop 不得阻塞 UI"
    );
    assert!(
        wait_until(Duration::from_secs(30), || matches!(
            status_of(&host, "dut-stop"),
            Some(SessionStatus::Stopped)
        )),
        "stop 后必须停在 Stopped：{:?}",
        status_of(&host, "dut-stop")
    );
    // Longer than the first backoff delay: no new process may appear.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(spawn_count(&counter), 1, "stop 必须取消待执行的重启");
    assert_eq!(
        events
            .started
            .lock()
            .unwrap()
            .iter()
            .filter(|dut| *dut == "dut-stop")
            .count(),
        1,
        "取消失败会重新 on_started"
    );

    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}
