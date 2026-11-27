//! W2 acceptance: `GOOSE_PATH_ROOT` takes effect — real goose config/sessions land inside
//! duthub's per-DUT directory rather than the user's home directory.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings};
use duthub_core::agent_config::AgentDefinition;
use duthub_core::lease::{ControlLeaseManager, WriteIdentity};
use duthub_core::{ApprovalError, AuditSink};
use duthub_desktop::agent_host::host::{HostEvents, HostManager};
use duthub_desktop::agent_host::model_env::GooseBinding;
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
    started: Mutex<Vec<(String, String)>>,
    exited: Mutex<Vec<(String, Option<i32>)>>,
    failed: Mutex<Vec<(String, String)>>,
}

impl HostEvents for Events {
    fn on_started(&self, dut: &str, session: &str) {
        self.started
            .lock()
            .unwrap()
            .push((dut.to_owned(), session.to_owned()));
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
    let dir =
        std::env::temp_dir().join(format!("duthub-goose-root-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn access_context() -> Arc<AccessContext> {
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let lease = Arc::new(ControlLeaseManager::new(None));
    let audit = AuditSink::memory();
    Arc::new(AccessContext::new(auth, lease, audit))
}

fn fake_binding() -> GooseBinding {
    GooseBinding {
        provider: duthub_core::Provider::Openai,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "fake-model".into(),
        api_key: "sk-path-root-secret".into(),
    }
}

// ---------------------------------------------------------------------------
// Test goose fixture (goose is downloaded, not embedded)
// ---------------------------------------------------------------------------

/// External-program definition that runs an installed goose through the same
/// fail-closed path as production (absolute path + pinned SHA-256).
///
/// `DUTHUB_TEST_GOOSE` (absolute path to a goose executable) wins, otherwise the
/// product resolver decides (downloaded install > PATH).
fn test_goose_agent() -> Option<AgentDefinition> {
    let program = match std::env::var_os("DUTHUB_TEST_GOOSE") {
        Some(raw) => {
            let path = PathBuf::from(raw);
            assert!(
                path.is_absolute() && path.is_file(),
                "DUTHUB_TEST_GOOSE must be an absolute path to an existing goose executable: {}",
                path.display()
            );
            path
        }
        None => registry::goose_install_state().program()?.to_path_buf(),
    };
    let sha256 = duthub_core::store::sha256_file(&program).ok()?;
    let mut agent = AgentDefinition::goose();
    agent.program = Some(program.display().to_string());
    agent.expected_sha256 = Some(sha256);
    Some(agent)
}

/// Loud skip: goose is no longer embedded, so ACP tests need an installed one.
fn skip_without_goose() {
    eprintln!(
        "SKIP: goose is not installed (goose is no longer embedded) — \
         download goose from Tool Downloads or set DUTHUB_TEST_GOOSE to the goose executable"
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
    false
}

/// Read-only snapshot of `%APPDATA%\Block\goose` (paths, lengths, mtimes).
///
/// Used as a zero-write control: the exact same file set and mtimes must remain unchanged.
#[derive(Debug, PartialEq, Eq)]
struct AppDataSnapshot {
    /// Whether the directory exists at all (even creating an empty directory counts as a write).
    present: bool,
    files: Vec<FileStamp>,
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    relative: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

fn appdata_goose_dir() -> PathBuf {
    PathBuf::from(
        std::env::var_os("APPDATA")
            .expect("Windows test environment must define APPDATA for zero-write verification"),
    )
    .join("Block")
    .join("goose")
}

fn appdata_goose_snapshot() -> AppDataSnapshot {
    let root = appdata_goose_dir();
    let present = root.is_dir();
    let mut files = Vec::new();
    if present {
        collect_files(&root, &root, &mut files);
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    AppDataSnapshot { present, files }
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<FileStamp>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_files(root, &path, files);
        } else {
            files.push(FileStamp {
                relative: path.strip_prefix(root).unwrap_or(&path).to_path_buf(),
                len: metadata.len(),
                modified: metadata.modified().ok(),
            });
        }
    }
}

#[test]
fn host_keeps_goose_state_under_goose_path_root() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("root");
    let paths = sandbox::prepare(&work, "dut-root").unwrap();
    // Section 5.2 gap check: compare user goose directory file set and mtimes before/after.
    let appdata_before = appdata_goose_snapshot();
    let events = Arc::new(Events::default());
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(StubState),
        access_context(),
        Some(events.clone()),
    );
    host.start_with_agent("dut-root", Some(fake_binding()), agent)
        .unwrap();

    let ready = wait_until(Duration::from_secs(90), || {
        let view = host.view("dut-root").expect("view");
        let status = view.status();
        host.tick();
        matches!(status, SessionStatus::Ready)
    });
    let view = host.view("dut-root").expect("view");
    assert!(
        ready,
        "Host did not become ready: status={:?} stderr={} failed={:?}",
        view.status(),
        view.stderr_tail(),
        events.failed.lock().unwrap()
    );

    // GOOSE_PATH_ROOT takes effect: config and sessions stay under duthub's per-DUT directory.
    let managed = paths.goose_root.join("config").join("config.yaml");
    let config = std::fs::read_to_string(&managed).unwrap();
    assert!(config.contains("extensionmanager"));
    assert!(config.contains("enabled: false"));

    let sessions_db = paths
        .goose_root
        .join("data")
        .join("sessions")
        .join("sessions.db");
    assert!(
        wait_until(Duration::from_secs(20), || sessions_db.is_file()),
        "goose did not create sessions.db under GOOSE_PATH_ROOT: {}",
        sessions_db.display()
    );

    // session-id / transcript / sandbox also reside under this DUT's directory.
    let session_id_file = std::fs::read_to_string(&paths.session_id).unwrap();
    let session_id = view.session_id().expect("session id");
    assert_eq!(session_id_file.trim(), session_id);
    assert!(
        wait_until(Duration::from_secs(5), || paths.transcript.is_file()),
        "transcript was not created"
    );
    assert!(paths.sandbox.is_dir());
    assert!(view.port().is_some_and(|port| port > 0));

    let started = events.started.lock().unwrap().clone();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, "dut-root");
    assert_eq!(started[0].1, session_id);
    assert!(events.failed.lock().unwrap().is_empty());

    host.stop("dut-root");
    let stopped = wait_until(Duration::from_secs(20), || {
        host.tick();
        host.view("dut-root")
            .map(|view| view.status())
            .is_some_and(|status| status.is_terminal())
    });
    assert!(
        stopped,
        "session did not reach terminal state after stop: {:?} stderr={}",
        host.view("dut-root").map(|view| view.status()),
        view.stderr_tail()
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            !events.exited.lock().unwrap().is_empty()
        }),
        "on_exited callback did not fire"
    );
    drop(host);
    // Give the cleanup thread a brief window to flush before comparing user AppData.
    std::thread::sleep(Duration::from_millis(500));
    let appdata_after = appdata_goose_snapshot();
    assert_eq!(
        appdata_after, appdata_before,
        "goose wrote to %APPDATA%\\Block\\goose (GOOSE_PATH_ROOT did not fully converge): before={appdata_before:?} after={appdata_after:?}"
    );
    let _ = std::fs::remove_dir_all(&work);
}
