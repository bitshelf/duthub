//! W2b Agent restart regressions.
//!
//! - A session that never reached `Ready` is not restarted (startup failure).
//! - A session that reached `Ready` and exited on its own is restarted at most
//!   three times per ten-minute window (1s/2s/4s backoff), then `Failed`.
//! - `HostManager::stop` cancels a pending restart without spawning a new
//!   process.
//!
//! The native Rust fixture answers `initialize` and
//! `session/new` over stdin/stdout and then exits 0 (or exits immediately in
//! `fail-early` mode). Every spawn appends one line to a counter file, so the
//! tests can prove exactly how many processes were started.

#![cfg(windows)]

#[path = "support/agent_fixture.rs"]
mod agent_fixture;

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

/// Build an external ACP definition backed by the native fixture.
fn fake_agent(mode: &str, counter: &Path) -> AgentDefinition {
    let program = agent_fixture::executable();
    AgentDefinition {
        id: format!("fake-{mode}"),
        name: format!("Fake ACP {mode}"),
        kind: AgentKind::Acp,
        program: Some(program.to_string_lossy().into_owned()),
        args: vec![mode.to_owned(), counter.to_string_lossy().into_owned()],
        env_keys: Vec::new(),
        cli_profile: None,
        enabled: true,
        auth_mode: "inherit".into(),
        trust_level: "mcp_only".into(),
        expected_sha256: Some(duthub_core::store::sha256_file(program).unwrap()),
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
/// sandbox cell. Since ADR-12 the Agent runs under a `WRITE_RESTRICTED` token, so
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
        fake_agent("fail-early", &early_counter),
    )
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(60), || matches!(
            status_of(&host, "dut-no-ready"),
            Some(SessionStatus::Failed(_))
        )),
        "initialize failure must enter Failed: {:?}",
        status_of(&host, "dut-no-ready")
    );
    // Longer than the first restart delay: a failed startup must not retry.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        spawn_count(&early_counter),
        1,
        "startup failure must not auto-restart"
    );
    assert!(
        events
            .failed
            .lock()
            .unwrap()
            .iter()
            .any(|(dut, _)| dut == "dut-no-ready"),
        "startup failure must invoke on_failed"
    );

    // Ready then self-exit: initial start + three restarts, then Failed.
    let crash_counter = sandbox_counter(&work, "dut-crash", "crash-after-ready");
    host.start_with_agent(
        "dut-crash",
        None,
        fake_agent("crash-after-ready", &crash_counter),
    )
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(120), || matches!(
            status_of(&host, "dut-crash"),
            Some(SessionStatus::Failed(_))
        )),
        "must enter Failed after restart budget is exhausted: {:?} stderr={}",
        status_of(&host, "dut-crash"),
        host.view("dut-crash")
            .map_or_default(|view| view.stderr_tail())
    );
    assert_eq!(
        spawn_count(&crash_counter),
        MAX_RESTARTS_PER_WINDOW + 1,
        "initial start + 3 automatic restarts"
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
    assert_eq!(
        failed, 1,
        "only budget exhaustion triggers final Failed callback"
    );

    let view = host.view("dut-crash").unwrap();
    let transcript = view.transcript_snapshot();
    assert_eq!(
        transcript
            .iter()
            .filter(|entry| entry.text.contains("秒后自动重启"))
            .count(),
        MAX_RESTARTS_PER_WINDOW,
        "each restart must write one system note"
    );
    assert!(
        transcript
            .iter()
            .any(|entry| entry.text.contains("已停止自动重启")),
        "budget exhaustion must leave a system note"
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
    host.start_with_agent("dut-stop", None, fake_agent("crash-after-ready", &counter))
        .unwrap();
    wait_until(Duration::from_secs(60), || {
        matches!(
            status_of(&host, "dut-stop"),
            Some(SessionStatus::Ready | SessionStatus::Failed(_))
        )
    });
    assert!(
        matches!(status_of(&host, "dut-stop"), Some(SessionStatus::Ready)),
        "session did not become ready: {:?} stderr={}",
        status_of(&host, "dut-stop"),
        host.view("dut-stop")
            .map_or_default(|view| view.stderr_tail())
    );
    // After the fake exits, the worker settles in Stopped and then resets to
    // Starting while it waits out the 1s backoff.
    assert!(
        wait_until(Duration::from_secs(30), || matches!(
            status_of(&host, "dut-stop"),
            Some(SessionStatus::Starting)
        )),
        "did not enter restart backoff: {:?}",
        status_of(&host, "dut-stop")
    );

    let stop_started = Instant::now();
    host.stop("dut-stop");
    assert!(
        stop_started.elapsed() < Duration::from_secs(2),
        "stop must not block UI"
    );
    assert!(
        wait_until(Duration::from_secs(30), || matches!(
            status_of(&host, "dut-stop"),
            Some(SessionStatus::Stopped)
        )),
        "must stay in Stopped after stop: {:?}",
        status_of(&host, "dut-stop")
    );
    // Longer than the first backoff delay: no new process may appear.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(spawn_count(&counter), 1, "stop must cancel pending restart");
    assert_eq!(
        events
            .started
            .lock()
            .unwrap()
            .iter()
            .filter(|dut| *dut == "dut-stop")
            .count(),
        1,
        "cancelling restart must not trigger another on_started"
    );

    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}
