//! Each DUT requires a session host for the agent process (W2).
//!
//! - [`HostManager::start`] is a convenient interface provided by the default Goose framework.
//!   [`HostManager::start_with_agent`] accepts any ACP definitions defined in `agents.json` (these
//!   files are part of the overall startup process).
//! - [`HostManager::start`] returns results immediately. Operations such as unpacking,
//!   verification, initialization, and session management are all completed within the worker thread.
//! - [`HostManager::stop`] is handled solely by the UI thread: it cancels tokens, sets the system
//!   into a stopped state, cancels processes, and closes stdin. It waits for the worker thread to
//!   complete tasks related to killing processes.
//! - For worker threads involving watchdog mechanisms: if there are still unfinished tasks
//!   remaining, and no ACP events occur within 10 minutes, then the operation is considered "Failed",
//!   and the thread is stopped (without automatic restarting).
//! - W2b restart policy: a worker that reached `Ready` and then exited on its
//!   own is restarted at most three times inside ten minutes (1s/2s/4s
//!   backoff, see [`duthub_core::restart`]). An explicit stop cancels a pending
//!   restart, and startup failures/watchdog/protocol failures never restart.
//! - W2b OS isolation: each attempt hardens the per-DUT directory ACL and
//!   records [`duthub_core::os_acl::OsIsolationStatus`] in the session view;
//!   failures are notes, never startup blockers.
//! - W2c CLI profiles: [`HostManager::start_entry`] routes `kind=cli` Agents to
//!   the per-turn CLI worker in [`super::cli`]. `start_with_agent` keeps its
//!   original rejection semantics (CLI profiles are not ACP).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use duthub_core::agent_access::AgentAccessRuntime;
use duthub_core::agent_config::{AgentDefinition, AgentKind};
use duthub_core::model::SettingsFile;
use duthub_core::restart::{
    MAX_RESTART_DELAY, MAX_RESTARTS_PER_WINDOW, RESTART_WINDOW, RestartBudget,
};
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::probe;
use duthub_mcp::state::McpState;
use duthub_sandbox::NetworkPolicy;
use serde_json::Value;

use super::acp::{AcpClient, AcpEvent, CRITICAL_OVERFLOW_PREFIX, McpServerRef, SessionInfo};
use super::cli;
use super::listener::{self, SessionListener};
use super::model_env::{self, GooseBinding};
use super::registry::{self, AgentLaunchSpec};
use super::sandbox;
use super::session::{SessionStatus, SessionView, TranscriptEntry, TranscriptKind};
use super::transcript::TranscriptWriter;

/// Restart policy for Agent worker exits: only workers that reached `Ready`
/// at least once restart automatically; startup failures, explicit user stops,
/// and terminal watchdog/protocol failures end the session instead.
mod recovery {
    pub fn should_restart(ready_once: bool, stopped: bool, terminal_failure: bool) -> bool {
        ready_once && !stopped && !terminal_failure
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_ready_non_stopped_non_terminal_failures_restart() {
            assert!(should_restart(true, false, false));
            assert!(!should_restart(false, false, false));
            assert!(!should_restart(true, true, false));
            assert!(!should_restart(true, false, true));
            assert!(!should_restart(false, true, true));
        }
    }
}

/// The maximum duration without any events allowed when there are unfinished prompts pending
/// processing (F24: 10 minutes).
pub const PROMPT_HANG_TIMEOUT: Duration = Duration::from_mins(10);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(20);
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Resumed sessions must still expose the duthub MCP tools; bound the probe.
const MCP_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// UI → Capacity of the worker thread command queue. The UI interface uses `try_send` to send
/// messages, ensuring that the rendering thread remains unblocked at all times (press F4).
const COMMAND_QUEUE_CAPACITY: usize = 32;

pub trait HostEvents: Send + Sync {
    fn on_started(&self, dut: &str, session: &str);
    fn on_exited(&self, dut: &str, code: Option<i32>);
    fn on_failed(&self, dut: &str, message: &str);
}

#[derive(Debug)]
pub enum HostError {
    AlreadyRunning(String),
    NotRunning(String),
    NotReady(String),
    Invalid(String),
    Prompt(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::AlreadyRunning(dut) => write!(formatter, "DUT {dut} 的 Agent 会话已在运行"),
            HostError::NotRunning(dut) => write!(formatter, "DUT {dut} 没有 Agent 会话"),
            HostError::NotReady(dut) => write!(formatter, "DUT {dut} 的 Agent 会话尚未就绪"),
            HostError::Invalid(reason) => write!(formatter, "Agent 会话参数无效: {reason}"),
            HostError::Prompt(reason) => write!(formatter, "发送 prompt 失败: {reason}"),
        }
    }
}

impl std::error::Error for HostError {}

/// Where a new session takes its model binding from.
///
/// `FromSettings` resolves the same provider/model/key chain as the
/// "Agent Model" page (a deliberate choice of option); it is ignored for agents.
/// that bring their own credentials. `Resolved` is used by tests and callers
/// that already hold a key. `None` means "this Agent authenticates itself".
#[derive(Debug)]
pub enum BindingSource<'a> {
    Resolved(GooseBinding),
    FromSettings(&'a SettingsFile),
    None,
}

/// Result of [`HostManager::stop_and_join`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopJoin {
    /// No session entry existed for the DUT.
    NotRunning,
    /// The worker thread finished and its entry was removed.
    Joined,
    /// The worker did not finish within the timeout; the entry is kept so the
    /// caller cannot delete files the process may still hold.
    TimedOut,
}

/// Commands the UI thread delivers to either the ACP or the CLI worker.
pub(crate) enum WorkerCommand {
    Prompt(String),
    Stop,
    Cancel,
}

/// How one worker attempt ended; drives the restart policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct WorkerOutcome {
    /// The attempt reached `Ready` (the status transition succeeded).
    ready_reached: bool,
    /// The user asked to stop the session.
    stopped: bool,
    /// Watchdog or protocol fail-closed ended the attempt.
    terminal_failure: bool,
}

impl WorkerOutcome {
    fn stopped() -> Self {
        Self {
            stopped: true,
            ..Self::default()
        }
    }

    fn failed() -> Self {
        Self {
            terminal_failure: true,
            ..Self::default()
        }
    }
}

struct HostSession {
    view: Arc<SessionView>,
    listener_slot: Arc<Mutex<Option<SessionListener>>>,
    launch: Arc<Mutex<Option<AgentLaunchSpec>>>,
    tx: SyncSender<WorkerCommand>,
    join: Option<JoinHandle<()>>,
    pending: Arc<AtomicBool>,
    last_activity_ms: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    /// CLI sessions have no session id until the first `result` event, so
    /// `prompt` must not reject them as "not ready".
    is_cli: bool,
}

struct WorkerContext {
    dut_id: String,
    agent: AgentDefinition,
    binding: Option<GooseBinding>,
    launch: Arc<Mutex<Option<AgentLaunchSpec>>>,
    work_dir: PathBuf,
    view: Arc<SessionView>,
    state: Arc<dyn McpState>,
    access: Arc<AccessContext>,
    auth: Arc<AgentAccessRuntime>,
    events: Option<Arc<dyn HostEvents>>,
    acp: Arc<Mutex<Option<Arc<AcpClient>>>>,
    listener_slot: Arc<Mutex<Option<SessionListener>>>,
    pending: Arc<AtomicBool>,
    last_activity_ms: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    /// For testing purposes only: The listener URL is used for each session (negative scenarios
    /// involve injecting inaccessible MCP);
    /// The product path remains "`None`".
    mcp_url_override: Option<String>,
}

/// Each DUT serves as the host for one goose process.
pub struct HostManager {
    work_dir: PathBuf,
    state: Arc<dyn McpState>,
    access: Arc<AccessContext>,
    auth: Arc<AgentAccessRuntime>,
    events: Option<Arc<dyn HostEvents>>,
    sessions: HashMap<String, HostSession>,
    dirty: Arc<AtomicBool>,
}

impl std::fmt::Debug for HostManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostManager")
            .field("work_dir", &self.work_dir)
            .field("sessions", &self.sessions.len())
            .finish_non_exhaustive()
    }
}

impl HostManager {
    pub fn new(
        work_dir: PathBuf,
        state: Arc<dyn McpState>,
        access: Arc<AccessContext>,
        events: Option<Arc<dyn HostEvents>>,
    ) -> Self {
        let auth = access.auth.clone();
        Self {
            work_dir,
            state,
            access,
            auth,
            events,
            sessions: HashMap::new(),
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Launch the default Goose Agent session for the DUT (compatible with existing callers/tests).
    ///
    /// If the "goose" entry in `<work_dir>/agents.json` contains a reference to an external
    /// program, then this method will also be used (similar to the UI path). In case the registry
    /// becomes corrupted or unreadable, the internal goose mechanism will be used instead (fail
    /// closed: no processing of any external entries that haven't been verified using hashing).
    pub fn start(&mut self, dut_id: &str, binding: GooseBinding) -> Result<(), HostError> {
        let agent = duthub_core::agent_config::load_agents(&self.work_dir)
            .ok()
            .and_then(|file| file.goose().cloned())
            .unwrap_or_else(AgentDefinition::goose);
        self.start_with_agent(dut_id, Some(binding), agent)
    }

    /// Registry-driven start used by the connection page (W4b).
    ///
    /// ACP entries resolve the optional binding source here. CLI profiles take
    /// the per-turn worker path instead. `start_with_agent` keeps its exact
    /// signature and semantics (including the CLI rejection).
    pub fn start_entry(
        &mut self,
        dut_id: &str,
        binding: BindingSource<'_>,
        agent: AgentDefinition,
    ) -> Result<(), HostError> {
        // W2c: CLI profiles are not ACP programs. They keep their own
        // credentials (no duthub model binding) and run one process per turn.
        if agent.kind == AgentKind::Cli {
            return self.start_cli_entry(dut_id, agent);
        }
        let binding = match binding {
            BindingSource::Resolved(binding) => Some(binding),
            BindingSource::FromSettings(settings) if agent.is_goose() => {
                Some(model_env::resolve_binding(settings).map_err(HostError::Invalid)?)
            }
            // Non-goose Agents use their own credentials; a duthub model key
            // must never be forwarded to a third-party ACP program.
            BindingSource::FromSettings(_) | BindingSource::None => None,
        };
        self.start_with_agent(dut_id, binding, agent)
    }

    /// Start the session based on the Agent definitions in `agents.json`. Setting `binding = None`
    /// indicates that this Agent comes with a built-in model authentication mechanism. This feature
    /// is available only for non-"goose" agents. For "goose" agents, it's necessary to configure
    /// the model bindings before starting the agent.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "Keep the existing public launch API"
    )]
    pub fn start_with_agent(
        &mut self,
        dut_id: &str,
        binding: Option<GooseBinding>,
        agent: AgentDefinition,
    ) -> Result<(), HostError> {
        let dut_id = dut_id.trim();
        if dut_id.is_empty() {
            return Err(HostError::Invalid("DUT id 为空".into()));
        }
        // The CLI is rejected immediately by the UI thread, preventing users from encountering
        // errors after clicking "Start" and until the work thread processes the request.
        registry::ensure_launchable(&agent)
            .map_err(|error| HostError::Invalid(error.to_string()))?;
        // F19: During the start process, all elements such as trimming, protocol verification, Base
        // URL, models, and keys must be standardized. For legacy callers, (non-directory paths)
        // cannot bypass the protocol allowlist either.
        let binding = binding
            .as_ref()
            .map(model_env::normalize_binding)
            .transpose()
            .map_err(HostError::Invalid)?;
        if agent.is_goose() && binding.is_none() {
            return Err(HostError::Invalid(
                "goose 会话需要模型绑定：请先在「Agent 模型」中保存 provider/model/Token".into(),
            ));
        }
        // F8: First, the old records that have been completed and whose worker threads have joined
        // have been discarded. Entries that are still in process of being finalized will be
        // retained. At any given moment, only one agent is allowed to operate on the DUT
        // (otherwise, rapid restarts could result in two processes sharing resources).
        // GOOSE_PATH_ROOT）。
        self.remove_finished(dut_id);
        if self.sessions.contains_key(dut_id) {
            return Err(HostError::AlreadyRunning(dut_id.to_owned()));
        }

        let view = Arc::new(SessionView::new());
        view.set_status(SessionStatus::Starting);
        let (tx, rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let acp = Arc::new(Mutex::new(None));
        let listener_slot = Arc::new(Mutex::new(None));
        let launch = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicBool::new(false));
        let last_activity_ms = Arc::new(AtomicU64::new(now_ms()));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_name = format!("duthub-agent-{dut_id}-{}", agent.id);
        let context = WorkerContext {
            dut_id: dut_id.to_owned(),
            agent,
            binding,
            launch: launch.clone(),
            work_dir: self.work_dir.clone(),
            view: view.clone(),
            state: self.state.clone(),
            access: self.access.clone(),
            auth: self.auth.clone(),
            events: self.events.clone(),
            acp: acp.clone(),
            listener_slot: listener_slot.clone(),
            pending: pending.clone(),
            last_activity_ms: last_activity_ms.clone(),
            stop_flag: stop_flag.clone(),
            dirty: self.dirty.clone(),
            mcp_url_override: None,
        };
        let join = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || run_worker(&context, &rx))
            .map_err(|error| HostError::Invalid(format!("Agent 工作线程启动失败: {error}")))?;
        self.sessions.insert(
            dut_id.to_owned(),
            HostSession {
                view,
                listener_slot,
                launch,
                tx,
                join: Some(join),
                pending,
                last_activity_ms,
                stop_flag,
                is_cli: false,
            },
        );
        Ok(())
    }

    /// Start a CLI profile session (W2c). Validation happens on the UI thread
    /// so a misconfigured program is rejected before a worker exists.
    fn start_cli_entry(&mut self, dut_id: &str, agent: AgentDefinition) -> Result<(), HostError> {
        let dut_id = dut_id.trim();
        if dut_id.is_empty() {
            return Err(HostError::Invalid("DUT id 为空".into()));
        }
        cli::ensure_cli_launchable(&agent).map_err(HostError::Invalid)?;
        self.remove_finished(dut_id);
        if self.sessions.contains_key(dut_id) {
            return Err(HostError::AlreadyRunning(dut_id.to_owned()));
        }

        let view = Arc::new(SessionView::new());
        view.set_status(SessionStatus::Starting);
        let (tx, rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let listener_slot = Arc::new(Mutex::new(None));
        let launch = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicBool::new(false));
        let last_activity_ms = Arc::new(AtomicU64::new(now_ms()));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_name = format!("duthub-agent-{dut_id}-{}", agent.id);
        let context = cli::CliWorkerContext {
            dut_id: dut_id.to_owned(),
            agent,
            work_dir: self.work_dir.clone(),
            view: view.clone(),
            state: self.state.clone(),
            access: self.access.clone(),
            auth: self.auth.clone(),
            events: self.events.clone(),
            launch: launch.clone(),
            listener_slot: listener_slot.clone(),
            pending: pending.clone(),
            last_activity_ms: last_activity_ms.clone(),
            stop_flag: stop_flag.clone(),
            dirty: self.dirty.clone(),
            mcp_url_override: None,
        };
        let join = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                // `run_cli_worker` records its own terminal state; the JoinHandle
                // only exists so `tick`/`stop_and_join` can reconcile.
                let _ = cli::run_cli_worker(&context, &rx);
            })
            .map_err(|error| HostError::Invalid(format!("Agent 工作线程启动失败: {error}")))?;
        self.sessions.insert(
            dut_id.to_owned(),
            HostSession {
                view,
                listener_slot,
                launch,
                tx,
                join: Some(join),
                pending,
                last_activity_ms,
                stop_flag,
                is_cli: true,
            },
        );
        Ok(())
    }

    /// UI thread stopped: Token revoked, stop intention set, process terminated. No further actions
    /// are taken regarding stdin writing, waiting for other threads to complete, or killing the
    /// process tree (using F4). All operations such as canceling, closing stdin, and
    /// waiting/terminating the process tree are performed within the working thread.
    pub fn stop(&mut self, dut_id: &str) {
        let Some(session) = self.sessions.get(dut_id) else {
            return;
        };
        // A live worker may sit in a restart backoff with the view already
        // back in a terminal state (the crash path stops normally). The stop
        // flag still has to be set so the pending restart is cancelled.
        let worker_finished = session.join.as_ref().is_none_or(JoinHandle::is_finished);
        if session.view.status().is_terminal() && worker_finished {
            return;
        }
        session.stop_flag.store(true, Ordering::Relaxed);
        // F3: Only changes the state from non-terminating to "Stopping". It's possible that the
        // worker thread has been set to "Failed/Stopped" earlier.
        session
            .view
            .try_transition_if(|status| !status.is_terminal(), SessionStatus::Stopping);
        session.pending.store(false, Ordering::Relaxed);
        if let Some(listener) = session.listener_slot.lock().unwrap().take() {
            listener::revoke(listener, &self.auth);
        }
        // Contiguous queues: Even when the queue is full, the stop_flag allows the worker threads
        // to exit during the next round.
        let _ = session.tx.try_send(WorkerCommand::Stop);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Stop the session and wait (bounded) for the worker thread to finish.
    ///
    /// Deletion flows (W4a) must only remove the DUT's files after `Joined`;
    /// on `TimedOut` the worker may still own the process and the sandbox.
    pub fn stop_and_join(&mut self, dut_id: &str, timeout: Duration) -> StopJoin {
        if !self.sessions.contains_key(dut_id) {
            return StopJoin::NotRunning;
        }
        self.stop(dut_id);
        let deadline = Instant::now() + timeout;
        loop {
            let finished = self
                .sessions
                .get(dut_id)
                .is_some_and(|session| session.join.as_ref().is_none_or(JoinHandle::is_finished));
            if finished {
                break;
            }
            if Instant::now() >= deadline {
                return StopJoin::TimedOut;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        if let Some(mut session) = self.sessions.remove(dut_id)
            && let Some(join) = session.join.take()
        {
            let _ = join.join();
        }
        self.dirty.store(true, Ordering::Relaxed);
        StopJoin::Joined
    }

    /// Cancel the current round (do not terminate the process); UI will only send commands, while
    /// stdin will be written to the worker thread (F4).
    pub fn cancel(&mut self, dut_id: &str) {
        let Some(session) = self.sessions.get(dut_id) else {
            return;
        };
        let _ = session.tx.try_send(WorkerCommand::Cancel);
    }

    pub fn stop_all(&mut self) {
        let dut_ids: Vec<String> = self.sessions.keys().cloned().collect();
        for dut_id in dut_ids {
            self.stop(&dut_id);
        }
    }

    /// Sending prompts (only for Ready states). Completed observation of transcripts and statuses
    /// via [`HostManager::view`).
    pub fn prompt(&mut self, dut_id: &str, text: &str) -> Result<(), HostError> {
        let Some(session) = self.sessions.get(dut_id) else {
            return Err(HostError::NotRunning(dut_id.to_owned()));
        };
        if text.trim().is_empty() {
            return Err(HostError::Invalid("prompt 不能为空".into()));
        }
        // ACP sessions need a sessionId before a prompt can be routed. CLI
        // sessions spawn a fresh process per turn; their first result event
        // supplies the id, so an empty id is not a readiness failure.
        if session.view.session_id().is_none() && !session.is_cli {
            return Err(HostError::NotReady(dut_id.to_owned()));
        }
        // F3: Ready → Busy. When using CAS, it is necessary to avoid overwriting the state during
        // the transition from Ready to Stopping/terminated state.
        let current = session.view.status();
        if !session
            .view
            .try_transition(&SessionStatus::Ready, SessionStatus::Busy)
        {
            return Err(match current {
                SessionStatus::Busy => HostError::NotReady("上一轮尚未结束".into()),
                _ => HostError::NotReady(dut_id.to_owned()),
            });
        }
        match session.tx.try_send(WorkerCommand::Prompt(text.to_owned())) {
            Ok(()) => {
                session.pending.store(true, Ordering::Relaxed);
                session.last_activity_ms.store(now_ms(), Ordering::Relaxed);
                self.dirty.store(true, Ordering::Relaxed);
                Ok(())
            }
            // Delivery failed: Restore the status to Ready, so as to prevent sessions from getting
            // stuck in the Busy state.
            Err(TrySendError::Full(_)) => {
                session
                    .view
                    .try_transition(&SessionStatus::Busy, SessionStatus::Ready);
                Err(HostError::Prompt("命令队列已满，请稍后重试".into()))
            }
            Err(TrySendError::Disconnected(_)) => {
                session
                    .view
                    .try_transition(&SessionStatus::Busy, SessionStatus::Ready);
                Err(HostError::Prompt("工作线程已结束".into()))
            }
        }
    }

    /// Arc view (does not clone the transcript Vec per frame; rendering layer locks
    /// `view.transcript`).
    #[must_use]
    pub fn view(&self, dut_id: &str) -> Option<Arc<SessionView>> {
        self.sessions
            .get(dut_id)
            .map(|session| session.view.clone())
    }

    /// The programs/parameters/environments actually parsed during this session (including whether
    /// the external "goose" was utilized).
    #[must_use]
    pub fn launch(&self, dut_id: &str) -> Option<AgentLaunchSpec> {
        self.sessions
            .get(dut_id)
            .and_then(|session| session.launch.lock().unwrap().clone())
    }

    /// Recycling of completed worker threads; indicating whether there has been any changes in
    /// their status since the last tick.
    ///
    /// F3/F8 reconciliation: If the session remains in an non-terminating state after the `join`
    /// operation is completed (due to work thread panics or other exceptions), it will be marked as
    /// failed. Entries that are in a terminating state will be retained until the next `start`
    /// operation, at which time they can be reclaimed (this information needs to be displayed in
    /// the UI).
    pub fn tick(&mut self) -> bool {
        let mut reconciled: Vec<(String, String)> = Vec::new();
        for (dut_id, session) in &mut self.sessions {
            if !session.join.as_ref().is_some_and(JoinHandle::is_finished) {
                continue;
            }
            let Some(join) = session.join.take() else {
                continue;
            };
            let panicked = join.join().is_err();
            if !session.view.status().is_terminal() {
                let message = if panicked {
                    "Agent 工作线程异常退出（panic）"
                } else {
                    "Agent 工作线程已结束但未设置终止状态"
                };
                session.view.fail(message.to_owned());
                session
                    .view
                    .push_entry(TranscriptEntry::error(message.to_owned()));
                reconciled.push((dut_id.clone(), message.to_owned()));
                session.stop_flag.store(true, Ordering::Relaxed);
            }
        }
        if !reconciled.is_empty()
            && let Some(events) = &self.events
        {
            for (dut_id, message) in &reconciled {
                events.on_failed(dut_id, message);
            }
        }
        let dirty = self.dirty.swap(false, Ordering::Relaxed);
        dirty || !reconciled.is_empty()
    }

    /// Mark the view/work thread as completed (for UI purposes); does not affect session records.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Entries are only reclaimed when the session has been terminated and the worker thread has
    /// completed its tasks. Threads continue to be retained until they are actually finished
    /// processing tasks. This prevents situations where two processes share the same
    /// `GOOSE_PATH_ROOT(F8)` during rapid restarts.
    fn remove_finished(&mut self, dut_id: &str) -> bool {
        let finished = self.sessions.get(dut_id).is_some_and(|session| {
            session.view.status().is_terminal()
                // `join == None` indicates that the tick has already been joined. In this case, the
                // process is considered to be completed.
                && session.join.as_ref().is_none_or(JoinHandle::is_finished)
        });
        if !finished {
            return false;
        }
        let Some(mut session) = self.sessions.remove(dut_id) else {
            return false;
        };
        if let Some(join) = session.join.take() {
            let _ = join.join();
        }
        true
    }
}

impl Drop for HostManager {
    fn drop(&mut self) {
        self.stop_all();
        for (_, mut session) in self.sessions.drain() {
            if let Some(join) = session.join.take() {
                let _ = join.join();
            }
        }
    }
}

fn run_worker(context: &WorkerContext, rx: &Receiver<WorkerCommand>) {
    let mut budget = RestartBudget::new();
    let mut ready_once = false;
    let mut restarts = 0usize;
    loop {
        let outcome = prepare_and_run(context, rx);
        ready_once |= outcome.ready_reached;
        cleanup_attempt(context);

        if outcome.stopped || is_stop_requested(context) {
            finalize_stopped(context);
            break;
        }
        if !recovery::should_restart(ready_once, outcome.stopped, outcome.terminal_failure) {
            break;
        }
        if !budget.allow(Instant::now()) {
            // A stop that raced the crash wins over the budget failure.
            if is_stop_requested(context) {
                finalize_stopped(context);
            } else {
                fail_restart_budget(context);
            }
            break;
        }
        if !context.view.reset_for_restart() {
            // Only a `Stopped` session is revived. `Failed` is terminal, and a
            // concurrent stop has already moved the view to `Stopping`.
            if is_stop_requested(context) {
                finalize_stopped(context);
            }
            break;
        }
        restarts += 1;
        let delay = budget.next_delay().unwrap_or(MAX_RESTART_DELAY);
        context.view.push_entry(TranscriptEntry::system(format!(
            "Agent 会话异常退出，{} 秒后自动重启（第 {restarts}/{MAX_RESTARTS_PER_WINDOW} 次）",
            delay.as_secs()
        )));
        context.dirty.store(true, Ordering::Relaxed);
        if !wait_for_restart(context, rx, delay) {
            finalize_stopped(context);
            break;
        }
    }
    cleanup_attempt(context);
    // F3 fallback: Any abnormal return paths must result in a termination state. Otherwise, the
    // session will never be able to be restarted.
    if !context.view.status().is_terminal() {
        let message = "Agent 工作线程意外结束".to_owned();
        context
            .view
            .push_entry(TranscriptEntry::error(message.clone()));
        if context.view.fail(message.clone())
            && let Some(events) = &context.events
        {
            events.on_failed(&context.dut_id, &message);
        }
        context.dirty.store(true, Ordering::Relaxed);
    }
}

/// Run one attempt and classify its outcome for the restart policy.
fn prepare_and_run(context: &WorkerContext, rx: &Receiver<WorkerCommand>) -> WorkerOutcome {
    match run_attempt(context, rx) {
        Ok(outcome) => outcome,
        Err(message) => {
            if is_stop_requested(context) {
                return WorkerOutcome::stopped();
            }
            let message = redact_binding(&message, context.binding.as_ref());
            context
                .view
                .push_entry(TranscriptEntry::error(message.clone()));
            if context.view.fail(message.clone())
                && let Some(events) = &context.events
            {
                events.on_failed(&context.dut_id, &message);
            }
            context.dirty.store(true, Ordering::Relaxed);
            WorkerOutcome::failed()
        }
    }
}

fn run_attempt(
    context: &WorkerContext,
    rx: &Receiver<WorkerCommand>,
) -> Result<WorkerOutcome, String> {
    let paths =
        sandbox::prepare(&context.work_dir, &context.dut_id).map_err(|error| error.to_string())?;
    // Fail closed early if the host cannot enforce OS sandbox containment.
    let probe_status = duthub_sandbox::status();
    if !probe_status.supports_agent_isolation() {
        return Err(format!(
            "OS 沙箱后端不可用或缺少必需隔离能力，已拒绝启动 Agent（{}）",
            probe_status.summary()
        ));
    }
    // Resolve external/downloaded ACP program path, SHA-256, and version gate.
    let launch =
        registry::resolve(&context.work_dir, &context.agent).map_err(|error| error.to_string())?;
    *context.launch.lock().unwrap() = Some(launch.clone());
    if context.stop_flag.load(Ordering::Relaxed) {
        return Ok(WorkerOutcome::stopped());
    }
    let sandbox_state = sandbox::prepare_agent_sandbox(
        &context.work_dir,
        &context.dut_id,
        &paths,
        launch.program.parent(),
        &[paths.goose_root.as_path()],
        &[],
        NetworkPolicy::LoopbackAndOutbound { proxy_url: None },
    )?;
    context.view.push_entry(TranscriptEntry::system(format!(
        "OS 隔离状态：{}",
        sandbox_state.status.summary()
    )));
    context.view.set_os_isolation(sandbox_state.status.clone());
    context.dirty.store(true, Ordering::Relaxed);
    // duthub owns the sandbox/isolation variables. The registry rejects them on
    // save; re-check here so a hand-built AgentDefinition cannot bypass it.
    for (key, _) in &launch.env {
        if duthub_core::agent_config::is_reserved_env_key(key) {
            return Err(format!(
                "Agent {} 的 envKeys 包含 duthub 保留变量 {key}，已拒绝启动",
                launch.agent_name
            ));
        }
    }
    let env = if let Some(binding) = context.binding.as_ref() {
        model_env::write_config(&paths.goose_root)?;
        let model = model_env::goose_env(binding, &paths.goose_root)?;
        // Model variables win. envKeys are applied first and a collision is an
        // explicit error: previously envKeys could silently override
        // GOOSE_PROVIDER/OPENAI_BASE_URL and redirect the model binding.
        merge_model_env(&launch.env, model)?
    } else {
        // The caller explicitly declares that this agent uses self-authentication. It only injects
        // its own keyring environment variables, and does not pass the Duthub model token to
        // third-party applications.
        launch.env.clone()
    };
    // Product path: Loopback listener for each session + read-only token.
    // The test path (mcp_url_override) uses the injected URL directly, without registering any
    // listeners.
    let (port, token, url) = if let Some(url) = &context.mcp_url_override {
        (None, "dth-test-token".to_owned(), url.clone())
    } else {
        let listener = listener::issue(
            context.state.clone(),
            context.access.clone(),
            context.auth.clone(),
            &context.dut_id,
        )
        .map_err(|error| error.to_string())?;
        let port = listener.port;
        let token = listener.token.clone();
        context
            .view
            .set_credential_id(listener.credential_id.clone());
        *context.listener_slot.lock().unwrap() = Some(listener);
        (Some(port), token, format!("http://127.0.0.1:{port}/mcp"))
    };
    if context.stop_flag.load(Ordering::Relaxed) {
        return Ok(WorkerOutcome::stopped());
    }

    let acp = Arc::new(
        AcpClient::spawn_in_cell(
            &launch.program,
            &launch.args,
            &sandbox_state.cell,
            &sandbox_state.policy,
            &env,
            64 * 1024,
            context.stop_flag.clone(),
        )
        .map_err(|error| error.to_string())?,
    );
    *context.acp.lock().unwrap() = Some(acp.clone());
    let initialize = acp
        .initialize(INITIALIZE_TIMEOUT)
        .map_err(|error| error.to_string())?;
    if !initialize.mcp_http {
        return Err(format!(
            "{} 未声明 mcpCapabilities.http，无法注入 duthub MCP",
            launch.agent_name
        ));
    }
    if context.stop_flag.load(Ordering::Relaxed) {
        return Ok(WorkerOutcome::stopped());
    }
    let mcp = McpServerRef {
        name: "duthub",
        url: &url,
        token: &token,
    };
    // F11: `resume.json` is authoritative (agent id + program hash). The
    // legacy `session-id` file is only a goose fallback when no structured
    // marker exists, and it is resumed without a hash check (with a note).
    let resume_candidate = read_resume_candidate(
        &paths,
        &launch,
        context.agent.is_goose(),
        context.view.as_ref(),
    );
    let (session, resume) = resume_or_new(
        resume_candidate.as_deref(),
        initialize.load_session,
        |session_id| acp.load_session(session_id, &paths.sandbox, &mcp, SESSION_TIMEOUT),
        || acp.new_session(&paths.sandbox, &mcp, SESSION_TIMEOUT),
    )
    .map_err(|error| error.to_string())?;
    match &resume {
        // F1: every newly created session — Fresh *and* Degraded — must prove
        // its runtime tool surface. GOOSE_MODE=auto would never emit a
        // permission request, so extensionResults is the only gate.
        ResumeOutcome::Fresh | ResumeOutcome::Degraded(_) => {
            if launch.agent_id == registry::GOOSE_AGENT_ID {
                validate_extension_results(&session.extension_results)
                    .map_err(|error| format!("goose 工具面校验失败: {error}"))?;
            } else {
                // Generic ACP: do not assume goose's extensionResults shape;
                // the Agent declares its own tools and duthub only injects the
                // MCP listener (initialize already checked mcpCapabilities.http).
                context.view.push_entry(TranscriptEntry::system(format!(
                    "非 goose ACP Agent（{}）：未校验 goose extensionResults，duthub MCP 已注入",
                    launch.agent_name
                )));
            }
            if let ResumeOutcome::Degraded(reason) = &resume {
                context
                    .view
                    .push_entry(TranscriptEntry::system(reason.clone()));
            }
        }
        ResumeOutcome::Resumed => {
            // `session/load` does not return extensionResults. Re-validate the
            // actual MCP tool surface through the new per-session listener;
            // otherwise a stale/foreign session would stay Ready with no tools.
            let tools =
                probe::list_tool_names(&url, &token, MCP_PROBE_TIMEOUT).map_err(|error| {
                    remove_resume_markers(&paths, context.view.as_ref());
                    format!("已恢复的会话未通过 MCP 工具面复验（{error}）；请重新启动以新建会话")
                })?;
            if tools.is_empty() {
                remove_resume_markers(&paths, context.view.as_ref());
                return Err("已恢复的会话未暴露任何 duthub MCP 工具；请重新启动以新建会话".into());
            }
            context.view.push_entry(TranscriptEntry::system(format!(
                "已恢复上次会话（{}）；MCP 工具面复验通过（{} 个工具）",
                session.session_id,
                tools.len()
            )));
        }
    }
    if context.stop_flag.load(Ordering::Relaxed) {
        return Ok(WorkerOutcome::stopped());
    }
    persist_resume_state(&paths, &launch, &session.session_id)?;
    context.view.set_session_id(session.session_id.clone());
    if let Some(port) = port {
        context.view.set_port(port);
    }
    // It is possible that the user has already chosen to stop the process during initialization. Do
    // not change the state from "Stopping" to "Ready".
    let ready_reached = context
        .view
        .try_transition(&SessionStatus::Starting, SessionStatus::Ready);
    let model_detail = context
        .binding
        .as_ref()
        .map_or_else(|| "Agent 自带认证".to_owned(), GooseBinding::redacted_debug);
    context.view.push_entry(TranscriptEntry::system(format!(
        "Agent 会话已启动（{} · {} · {}）",
        launch.agent_name,
        model_detail,
        session.current_mode.as_deref().unwrap_or("default mode")
    )));
    if let Some(events) = &context.events {
        events.on_started(&context.dut_id, &session.session_id);
    }
    context.dirty.store(true, Ordering::Relaxed);

    let transcript = TranscriptWriter::open(&paths.transcript, context.view.clone());
    let mut last_stderr_poll = Instant::now();
    let mut stop_requested = false;
    let mut terminal_failure = false;
    loop {
        // The UI "stop" function simply sets a flag and executes the corresponding command. Even if
        // the "stop" command is discarded due to a full queue, it will still be executed in the
        // next iteration (F4).
        if context.stop_flag.load(Ordering::Relaxed) {
            stop_requested = true;
        }
        let mut fatal = None;
        match rx.try_recv() {
            Ok(WorkerCommand::Prompt(text)) => {
                if let Some(session_id) = context.view.session_id() {
                    match acp.prompt(&session_id, &text) {
                        Ok(()) => {
                            context.last_activity_ms.store(now_ms(), Ordering::Relaxed);
                            context.pending.store(true, Ordering::Relaxed);
                            context.view.try_transition_if(
                                |status| {
                                    !status.is_terminal() && *status != SessionStatus::Stopping
                                },
                                SessionStatus::Busy,
                            );
                            record_entry(
                                &context.view,
                                &transcript,
                                TranscriptEntry::new(TranscriptKind::User, text),
                            );
                            context.dirty.store(true, Ordering::Relaxed);
                        }
                        Err(error) => {
                            let message = format!("发送 prompt 失败: {error}");
                            record_entry(
                                &context.view,
                                &transcript,
                                TranscriptEntry::error(message.clone()),
                            );
                            fatal = Some(message);
                        }
                    }
                } else {
                    fatal = Some("会话尚无 sessionId，无法发送 prompt".into());
                }
            }
            Ok(WorkerCommand::Stop) | Err(TryRecvError::Disconnected) => stop_requested = true,
            Ok(WorkerCommand::Cancel) => {
                if let Some(session_id) = context.view.session_id() {
                    acp.cancel(&session_id);
                }
            }
            Err(TryRecvError::Empty) => {}
        }
        if stop_requested {
            break;
        }
        if let Some(message) = fatal.take() {
            fail_session(context, &message);
            terminal_failure = true;
            break;
        }

        let acp_activity = acp.last_activity_unix_ms();
        context
            .last_activity_ms
            .fetch_max(acp_activity, Ordering::Relaxed);
        for event in acp.drain() {
            context.last_activity_ms.store(now_ms(), Ordering::Relaxed);
            match event {
                AcpEvent::AgentChunk(text) => {
                    record_entry(
                        &context.view,
                        &transcript,
                        TranscriptEntry::new(TranscriptKind::Agent, text),
                    );
                    context.dirty.store(true, Ordering::Relaxed);
                }
                AcpEvent::AgentThought(text) => {
                    record_entry(
                        &context.view,
                        &transcript,
                        TranscriptEntry::new(TranscriptKind::Thought, text),
                    );
                    context.dirty.store(true, Ordering::Relaxed);
                }
                AcpEvent::ToolCall { id, title, status } => {
                    let label = if title.is_empty() { &id } else { &title };
                    record_entry(
                        &context.view,
                        &transcript,
                        TranscriptEntry::new(TranscriptKind::Tool, format!("{label} [{status}]")),
                    );
                    context.dirty.store(true, Ordering::Relaxed);
                }
                AcpEvent::PromptDone { stop_reason } => {
                    context.pending.store(false, Ordering::Relaxed);
                    context.view.try_transition_if(
                        |status| !status.is_terminal() && *status != SessionStatus::Stopping,
                        SessionStatus::Ready,
                    );
                    record_entry(
                        &context.view,
                        &transcript,
                        TranscriptEntry::system(format!("本轮结束（{stop_reason}）")),
                    );
                    context.dirty.store(true, Ordering::Relaxed);
                }
                AcpEvent::PermissionDenied { title } => {
                    record_entry(
                        &context.view,
                        &transcript,
                        TranscriptEntry::system(format!("已拒绝权限请求：{title}")),
                    );
                    context.dirty.store(true, Ordering::Relaxed);
                }
                AcpEvent::ProtocolError(message) => {
                    record_entry(
                        &context.view,
                        &transcript,
                        TranscriptEntry::error(message.clone()),
                    );
                    context.dirty.store(true, Ordering::Relaxed);
                    // The key event queue overflow situation is treated as a failure condition: The
                    // process will be terminated, regardless of whether it is waiting for a prompt
                    // or not.
                    let critical_overflow = message.starts_with(CRITICAL_OVERFLOW_PREFIX);
                    if critical_overflow || context.pending.swap(false, Ordering::Relaxed) {
                        fatal = Some(message);
                    }
                }
                // The exit process is handled by the `try_wait` branch below. This branch is also
                // responsible for handling the on_exited and cleanup operations.
                AcpEvent::Exited(_) => {}
            }
        }
        if let Some(message) = fatal {
            fail_session(context, &message);
            terminal_failure = true;
            break;
        }

        if let Some(code) = acp.try_wait().map_err(|error| error.to_string())? {
            context.view.set_stderr_tail(acp.stderr_tail());
            // When the user actively stops the process, the process exit code (which may not be 0,
            // as generated by the kill command) is not considered a failure.
            let stopping = context.stop_flag.load(Ordering::Relaxed)
                || context.view.status() == SessionStatus::Stopping;
            if code != 0 && !stopping {
                // A crash after `Ready` is a restart candidate. Record it and
                // leave the final Failed decision to the restart budget:
                // `stop_normally` moves the non-terminal view to `Stopped`,
                // which is the only state `reset_for_restart` revives.
                let message = format!(
                    "{} 进程退出（code {}）",
                    launch.agent_name,
                    code_text(Some(code))
                );
                record_entry(&context.view, &transcript, TranscriptEntry::error(message));
            }
            context.view.stop_normally();
            if let Some(events) = &context.events {
                events.on_exited(&context.dut_id, Some(code));
            }
            context.dirty.store(true, Ordering::Relaxed);
            return Ok(WorkerOutcome {
                ready_reached,
                stopped: stopping,
                terminal_failure: false,
            });
        }

        if context.pending.load(Ordering::Relaxed) {
            let idle = now_ms().saturating_sub(context.last_activity_ms.load(Ordering::Relaxed));
            if idle >= u64::try_from(PROMPT_HANG_TIMEOUT.as_millis()).unwrap_or(u64::MAX) {
                let message = "prompt 超过 10 分钟无任何 ACP 事件，已停止会话".to_owned();
                context
                    .view
                    .push_entry(TranscriptEntry::error(message.clone()));
                fail_session(context, &message);
                terminal_failure = true;
                break;
            }
        }
        if last_stderr_poll.elapsed() >= Duration::from_millis(200) {
            context.view.set_stderr_tail(acp.stderr_tail());
            last_stderr_poll = Instant::now();
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    if stop_requested {
        // Explicit stop = the user ended the session: drop both resume markers
        // so the next start is a fresh session. Markers left by a crash or a
        // self-exit are kept and resumed on the next start.
        remove_resume_markers(&paths, context.view.as_ref());
    }
    if !terminal_failure {
        context
            .view
            .try_transition_if(|status| !status.is_terminal(), SessionStatus::Stopping);
    }
    if let Some(session_id) = context.view.session_id() {
        acp.cancel(&session_id);
    }
    acp.close_stdin();
    let status = acp
        .wait_timeout(STOP_TIMEOUT)
        .map_err(|error| error.to_string())?;
    // Even if the main process has successfully terminated, it is still necessary to use
    // TerminateJobObject to clean up any remaining child processes within that job. The handles are
    // closed by the Drop method of the SandboxedProcess object.
    acp.kill();
    let code = status;
    context.view.set_stderr_tail(acp.stderr_tail());
    if !terminal_failure {
        context.view.stop_normally();
    }
    if let Some(events) = &context.events {
        events.on_exited(&context.dut_id, code);
    }
    context.dirty.store(true, Ordering::Relaxed);
    transcript.finish();
    Ok(WorkerOutcome {
        ready_reached,
        stopped: stop_requested || context.stop_flag.load(Ordering::Relaxed),
        terminal_failure,
    })
}

/// If a failure occurs, report the issue once. Only change the status from "Not Terminated" to
/// "Failed" (F3). If the process has already been stopped or terminated, no further reporting will
/// be performed.
fn fail_session(context: &WorkerContext, message: &str) {
    if context.stop_flag.load(Ordering::Relaxed) {
        return;
    }
    if context.view.fail(message.to_owned())
        && let Some(events) = &context.events
    {
        events.on_failed(&context.dut_id, message);
    }
    context.dirty.store(true, Ordering::Relaxed);
}

/// True when the user has asked to stop (flag or a view already moved to
/// `Stopping` by [`HostManager::stop`]).
fn is_stop_requested(context: &WorkerContext) -> bool {
    context.stop_flag.load(Ordering::Relaxed) || context.view.status() == SessionStatus::Stopping
}

/// Release everything one attempt owns (listener credential + process tree).
/// The next attempt must start from a clean port and process slot.
fn cleanup_attempt(context: &WorkerContext) {
    if let Some(listener) = context.listener_slot.lock().unwrap().take() {
        listener::revoke(listener, &context.auth);
    }
    if let Some(acp) = context.acp.lock().unwrap().take() {
        acp.close_stdin();
        let _ = acp.wait_timeout(Duration::from_millis(500));
        // The exit of the main process does not mean that there are no remaining sub-processes
        // within the job (F5); all processes are finally terminated together.
        acp.kill();
    }
    // The in-flight prompt died with the attempt. Without this reset the next
    // attempt would inherit `pending = true` and the watchdog could kill an
    // idle session ten minutes later for a prompt it never received.
    context.pending.store(false, Ordering::Relaxed);
    context.last_activity_ms.store(now_ms(), Ordering::Relaxed);
}

/// Explicit stop: drop the resume markers (the user ended the session), settle
/// in `Stopped`, and report `on_exited` once.
fn finalize_stopped(context: &WorkerContext) {
    // A stop requested during initialize also lands here: both resume markers
    // must go, otherwise the next start would silently revive the session.
    if let Ok(paths) = sandbox::prepare(&context.work_dir, &context.dut_id) {
        remove_resume_markers(&paths, context.view.as_ref());
    }
    if context.view.stop_normally()
        && let Some(events) = &context.events
    {
        events.on_exited(&context.dut_id, None);
    }
    context.dirty.store(true, Ordering::Relaxed);
}

/// The restart budget is spent: the session ends in `Failed` (never silently
/// restarts again).
fn fail_restart_budget(context: &WorkerContext) {
    let message = format!(
        "Agent 在 {} 分钟内自动重启 {} 次后仍异常退出，已停止自动重启",
        RESTART_WINDOW.as_secs() / 60,
        MAX_RESTARTS_PER_WINDOW
    );
    let message = redact_binding(&message, context.binding.as_ref());
    context
        .view
        .push_entry(TranscriptEntry::error(message.clone()));
    // The crash path settled in `Stopped`; only that state becomes Failed here.
    let failed = context.view.try_transition(
        &SessionStatus::Stopped,
        SessionStatus::Failed(message.clone()),
    );
    if failed && let Some(events) = &context.events {
        events.on_failed(&context.dut_id, &message);
    }
    context.dirty.store(true, Ordering::Relaxed);
}

/// Sleep through the backoff delay, waking early when the user stops. Returns
/// whether the restart should continue.
fn wait_for_restart(
    context: &WorkerContext,
    rx: &Receiver<WorkerCommand>,
    delay: Duration,
) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if context.stop_flag.load(Ordering::Relaxed) {
            return false;
        }
        match rx.try_recv() {
            Ok(WorkerCommand::Stop) | Err(TryRecvError::Disconnected) => {
                context.stop_flag.store(true, Ordering::Relaxed);
                return false;
            }
            // No session is `Ready` during the backoff, so a prompt or cancel
            // cannot be served; it is ignored rather than queued forever.
            Ok(WorkerCommand::Prompt(_) | WorkerCommand::Cancel) | Err(TryRecvError::Empty) => {}
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    !context.stop_flag.load(Ordering::Relaxed)
}

/// Mirror one entry into the in-memory transcript and the async file writer.
pub(crate) fn record_entry(
    view: &SessionView,
    transcript: &TranscriptWriter,
    entry: TranscriptEntry,
) {
    view.push_entry(entry.clone());
    transcript.push(entry);
}

/// Apply `envKeys` first, then the model environment. A duplicate key is an
/// explicit error: an envKeys entry must never silently replace the provider,
/// model, base URL, or sandbox variables duthub injects for goose.
fn merge_model_env(
    agent_env: &[(String, String)],
    model_env: Vec<(String, String)>,
) -> Result<Vec<(String, String)>, String> {
    let mut merged = Vec::with_capacity(agent_env.len() + model_env.len());
    merged.extend_from_slice(agent_env);
    for (key, value) in model_env {
        if merged
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(&key))
        {
            return Err(format!(
                "环境变量冲突：Agent envKeys 已定义 {key}，与模型配置注入的变量同名；\
                 请从 Agent 的 envKeys 中移除该键"
            ));
        }
        merged.push((key, value));
    }
    Ok(merged)
}

/// Structured resume marker written next to the legacy `session-id` file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeState {
    version: u32,
    agent_id: String,
    program_sha256: String,
    session_id: String,
}

fn resume_state_version() -> u32 {
    1
}

fn read_resume_state(path: &Path) -> Result<Option<ResumeState>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|error| format!("JSON 解析失败: {error}"))?;
    let raw_version = value
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| u64::from(resume_state_version()));
    if raw_version != u64::from(resume_state_version()) {
        return Err(format!("不支持的版本 {raw_version}"));
    }
    let version = resume_state_version();
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("缺少 {key}"))
    };
    Ok(Some(ResumeState {
        version,
        agent_id: field("agentId")?,
        program_sha256: field("programSha256")?,
        session_id: field("sessionId")?,
    }))
}

/// Persist both resume markers after a successful new/resumed session.
pub(crate) fn persist_resume_state(
    paths: &sandbox::AgentPaths,
    launch: &AgentLaunchSpec,
    session_id: &str,
) -> Result<(), String> {
    let value = serde_json::json!({
        "version": resume_state_version(),
        "agentId": launch.agent_id,
        "programSha256": launch.sha256.to_ascii_lowercase(),
        "sessionId": session_id,
    });
    let text = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("序列化 resume.json 失败: {error}"))?;
    std::fs::write(&paths.resume, format!("{text}\n"))
        .map_err(|error| format!("写入 resume.json 失败: {error}"))?;
    std::fs::write(&paths.session_id, session_id)
        .map_err(|error| format!("写入 session-id 失败: {error}"))?;
    Ok(())
}

/// Delete both resume markers; missing files are fine. Failures are surfaced in
/// the transcript but never abort the session lifecycle.
pub(crate) fn remove_resume_markers(paths: &sandbox::AgentPaths, view: &SessionView) {
    for (path, label) in [
        (&paths.resume, "resume.json"),
        (&paths.session_id, "session-id"),
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => view.push_entry(TranscriptEntry::system(format!(
                "清理 {label} 失败（下次启动可能尝试恢复旧会话）: {error}"
            ))),
        }
    }
}

/// Pick the session id to load, if any.
///
/// - `resume.json` with a matching agent id and program hash → verify + load.
/// - `resume.json` with a different agent/hash → ignore (never resume another
///   Agent's session) and delete both markers.
/// - no `resume.json` + goose + legacy `session-id` → attempt a load without a
///   hash check (older versions only wrote the id); a note explains this.
pub(crate) fn read_resume_candidate(
    paths: &sandbox::AgentPaths,
    launch: &AgentLaunchSpec,
    goose: bool,
    view: &SessionView,
) -> Option<String> {
    match read_resume_state(&paths.resume) {
        Ok(Some(state)) => {
            let agent_matches = state.agent_id.trim() == launch.agent_id;
            let hash_matches = state
                .program_sha256
                .trim()
                .eq_ignore_ascii_case(launch.sha256.trim());
            if agent_matches && hash_matches {
                return Some(state.session_id);
            }
            view.push_entry(TranscriptEntry::system(format!(
                "resume.json 与当前 Agent/程序不匹配（agent={}，sha256={}），已忽略并按新会话启动",
                state.agent_id, state.program_sha256
            )));
            remove_resume_markers(paths, view);
            None
        }
        Ok(None) => {
            if !goose {
                return None;
            }
            let legacy = read_stored_session_id(&paths.session_id);
            if legacy.is_some() {
                view.push_entry(TranscriptEntry::system(
                    "检测到旧版 session-id（未记录 Agent/程序哈希），尝试恢复；失败将新建会话",
                ));
            }
            legacy
        }
        Err(error) => {
            view.push_entry(TranscriptEntry::system(format!(
                "resume.json 无法解析（{error}），已忽略并按新会话启动"
            )));
            // A corrupt structured marker must not fall back to the legacy id:
            // the two files may belong to different sessions.
            remove_resume_markers(paths, view);
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeOutcome {
    /// No available session-id found. Creating a new session instead.
    Fresh,
    /// `session/load` succeeded. The existing session will continue to be used.
    Resumed,
    /// The unavailable/failed instance has been restored, and a new session has been created. The
    /// message must clearly indicate that the instance has been downgraded to a new session.
    Degraded(String),
}

/// Read the session-id from the last write to disk. Missing/blank values are considered as not
/// present.
fn read_stored_session_id(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let session_id = text.trim();
    (!session_id.is_empty()).then(|| session_id.to_owned())
}

/// F11: Prioritize the restoration of existing sessions. If there are any issues, new sessions will
/// be rejected.
fn resume_or_new<E, Load, Create>(
    stored_session: Option<&str>,
    load_supported: bool,
    load: Load,
    create: Create,
) -> Result<(SessionInfo, ResumeOutcome), E>
where
    E: std::fmt::Display,
    Load: FnOnce(&str) -> Result<bool, E>,
    Create: FnOnce() -> Result<SessionInfo, E>,
{
    let Some(session_id) = stored_session else {
        return create().map(|session| (session, ResumeOutcome::Fresh));
    };
    if !load_supported {
        return create().map(|session| {
            (
                session,
                ResumeOutcome::Degraded(
                    "Agent 未声明 session/load 能力；已降级为新会话".to_owned(),
                ),
            )
        });
    }
    match load(session_id) {
        Ok(true) => Ok((
            SessionInfo {
                session_id: session_id.to_owned(),
                // `AcpClient::load_session` only returns a boolean value (no extensionResults is
                // returned).
                extension_results: Value::Array(Vec::new()),
                current_mode: None,
            },
            ResumeOutcome::Resumed,
        )),
        Ok(false) => create().map(|session| {
            (
                session,
                ResumeOutcome::Degraded("Agent 不支持 session/load；已降级为新会话".to_owned()),
            )
        }),
        Err(error) => {
            let reason = format!("恢复会话失败（{error}）；已降级为新会话");
            create().map(|session| (session, ResumeOutcome::Degraded(reason)))
        }
    }
}

/// F1: The `_meta.extensionResults` of `session.new` must be exactly equal to "duthub".
///
/// Closing the built-in extensions in managed config is just a prerequisite; it's still necessary
/// to verify the actual results during runtime. Otherwise, when the `GOOSE_MODE` configuration
/// changes or the MCP becomes unreachable, the tool will automatically expand without any warning.
pub fn validate_extension_results(results: &Value) -> Result<(), String> {
    let Some(entries) = results.as_array() else {
        return Err("session/new 未返回 _meta.extensionResults 数组".into());
    };
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unnamed>")
        })
        .collect();
    if entries.len() != 1 {
        return Err(format!(
            "期望恰好 1 个扩展（duthub，success=true），实际 {} 个: {names:?}",
            entries.len()
        ));
    }
    let entry = &entries[0];
    if entry.get("name").and_then(Value::as_str) != Some("duthub") {
        return Err(format!("期望仅 duthub 扩展，实际: {names:?}"));
    }
    if entry.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "duthub MCP 扩展未成功（success={}）: {}",
            entry.get("success").cloned().unwrap_or(Value::Null),
            compact_entry(entry)
        ));
    }
    Ok(())
}

fn compact_entry(entry: &Value) -> String {
    let text = serde_json::to_string(entry).unwrap_or_default();
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(400).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn code_text(code: Option<i32>) -> String {
    code.map_or_else(|| "signal/unknown".into(), |code| code.to_string())
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn redact_binding(message: &str, binding: Option<&GooseBinding>) -> String {
    binding
        .filter(|binding| binding.api_key.len() >= 4)
        .map_or_else(
            || message.to_owned(),
            |binding| message.replace(&binding.api_key, "[REDACTED]"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::agent_access::AgentAccessSettings;
    use duthub_core::lease::ControlLeaseManager;
    use duthub_core::{AuditSink, Provider};
    use duthub_mcp::snapshot::SnapshotState;
    use serde_json::json;

    #[test]
    fn extension_validation_requires_exactly_one_successful_duthub() {
        assert!(
            validate_extension_results(&json!([{ "name": "duthub", "success": true }])).is_ok()
        );
        for bad in [
            json!([]),
            json!([{ "name": "duthub", "success": false }]),
            json!([{ "name": "developer", "success": true }]),
            json!([
                { "name": "duthub", "success": true },
                { "name": "tom", "success": true }
            ]),
            json!({ "name": "duthub" }),
            json!([{ "name": "duthub" }]),
        ] {
            assert!(
                validate_extension_results(&bad).is_err(),
                "It is necessary to reject the abnormal tool surface: {bad}."
            );
        }
    }

    fn fresh_session(id: &str) -> SessionInfo {
        SessionInfo {
            session_id: id.to_owned(),
            extension_results: json!([{ "name": "duthub", "success": true }]),
            current_mode: None,
        }
    }

    #[test]
    fn resume_with_stored_id_and_capability_keeps_session() {
        let (session, outcome) = resume_or_new(
            Some("stored-session"),
            true,
            |id| {
                assert_eq!(id, "stored-session");
                Ok::<bool, String>(true)
            },
            || Ok::<_, String>(fresh_session("new-session")),
        )
        .unwrap();
        assert_eq!(outcome, ResumeOutcome::Resumed);
        assert_eq!(session.session_id, "stored-session");
    }

    #[test]
    fn resume_without_stored_id_creates_fresh_session() {
        let (session, outcome) = resume_or_new(
            None,
            true,
            |_| Ok::<bool, String>(true),
            || Ok::<_, String>(fresh_session("new-session")),
        )
        .unwrap();
        assert_eq!(outcome, ResumeOutcome::Fresh);
        assert_eq!(session.session_id, "new-session");
    }

    #[test]
    fn resume_falls_back_to_new_session_with_degraded_note() {
        // The ability to use loadSession has not been declared.
        let (session, outcome) = resume_or_new(
            Some("stale-session"),
            false,
            |_| Ok::<bool, String>(true),
            || Ok::<_, String>(fresh_session("new-1")),
        )
        .unwrap();
        assert_eq!(session.session_id, "new-1");
        assert!(matches!(
            &outcome,
            ResumeOutcome::Degraded(reason)
                if reason.contains("已降级为新会话") && reason.contains("未声明")
        ));

        // Declared the ability, but the session/load returned MethodNotFound.
        let (session, outcome) = resume_or_new(
            Some("stale-session"),
            true,
            |_| Ok::<bool, String>(false),
            || Ok::<_, String>(fresh_session("new-2")),
        )
        .unwrap();
        assert_eq!(session.session_id, "new-2");
        assert!(matches!(
            &outcome,
            ResumeOutcome::Degraded(reason)
                if reason.contains("已降级为新会话") && reason.contains("不支持")
        ));

        // Claims to have certain capabilities, but reports errors related to sessions/loads.
        let (session, outcome) = resume_or_new(
            Some("stale-session"),
            true,
            |_| Err::<bool, String>("rpc failed".into()),
            || Ok::<_, String>(fresh_session("new-3")),
        )
        .unwrap();
        assert_eq!(session.session_id, "new-3");
        assert!(matches!(
            &outcome,
            ResumeOutcome::Degraded(reason)
                if reason.contains("已降级为新会话") && reason.contains("rpc failed")
        ));
    }

    #[test]
    fn stored_session_id_is_trimmed_and_blank_is_ignored() {
        let dir = std::env::temp_dir().join(format!("duthub-session-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-id");
        assert_eq!(
            read_stored_session_id(&path),
            None,
            "Missing files are considered to be absent."
        );
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(
            read_stored_session_id(&path),
            None,
            "Blank files are considered to be empty."
        );
        std::fs::write(&path, "  abc-123 \n").unwrap();
        assert_eq!(
            read_stored_session_id(&path).as_deref(),
            Some("abc-123"),
            "It is necessary to perform trimming before returning the result."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_env_is_applied_after_env_keys_and_conflicts_fail() {
        let agent_env = vec![("ACME_KEY".to_owned(), "one".to_owned())];
        let model_env = vec![
            ("GOOSE_MODEL".to_owned(), "model-a".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "sk-model".to_owned()),
        ];
        let merged = merge_model_env(&agent_env, model_env.clone()).unwrap();
        assert_eq!(
            merged,
            [
                ("ACME_KEY".to_owned(), "one".to_owned()),
                ("GOOSE_MODEL".to_owned(), "model-a".to_owned()),
                ("OPENAI_API_KEY".to_owned(), "sk-model".to_owned()),
            ]
        );
        let error = merge_model_env(
            &[("openai_api_key".to_owned(), "sk-env-keys".to_owned())],
            model_env,
        )
        .expect_err(
            "Keys with the same name must trigger an error instead of being silently overwritten.",
        );
        assert!(error.contains("冲突"), "{error}");
    }

    fn launch_spec(agent_id: &str, sha256: &str) -> AgentLaunchSpec {
        AgentLaunchSpec {
            agent_id: agent_id.into(),
            agent_name: agent_id.into(),
            program: PathBuf::from("C:/tools/agent.exe"),
            args: Vec::new(),
            env: Vec::new(),
            embedded: false,
            sha256: sha256.into(),
            version: None,
            signature: "NotSigned".into(),
        }
    }

    fn resume_state_json(agent_id: &str, sha256: &str, session_id: &str) -> String {
        serde_json::json!({
            "version": 1,
            "agentId": agent_id,
            "programSha256": sha256,
            "sessionId": session_id,
        })
        .to_string()
    }

    #[test]
    fn resume_state_rejects_agent_or_program_hash_mismatch() {
        let work =
            std::env::temp_dir().join(format!("duthub-resume-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&work).unwrap();
        let paths = sandbox::prepare(&work, "dut-resume-state").unwrap();
        let sha = "a".repeat(64);
        let launch = launch_spec("goose", &sha);
        let view = SessionView::new();

        std::fs::write(
            &paths.resume,
            resume_state_json("goose", &sha, "session-ok"),
        )
        .unwrap();
        assert_eq!(
            read_resume_candidate(&paths, &launch, true, &view).as_deref(),
            Some("session-ok"),
            "When both the agent and the hash match, it's necessary to restore the state."
        );

        // Different agent: never resume another Agent's session.
        std::fs::write(
            &paths.resume,
            resume_state_json("acme", &sha, "cross-agent"),
        )
        .unwrap();
        assert_eq!(read_resume_candidate(&paths, &launch, true, &view), None);
        assert!(
            !paths.resume.exists(),
            "Invalid resume.json file must be cleaned up."
        );
        assert!(!paths.session_id.exists());

        // Same agent, different program binary.
        std::fs::write(
            &paths.resume,
            resume_state_json("goose", &"b".repeat(64), "old-binary"),
        )
        .unwrap();
        std::fs::write(&paths.session_id, "legacy-should-not-load").unwrap();
        assert_eq!(read_resume_candidate(&paths, &launch, true, &view), None);
        assert!(!paths.resume.exists());
        assert!(
            !paths.session_id.exists(),
            "The rejected resume.json file cannot be restored to the legacy session-id."
        );
        assert!(
            view.transcript_snapshot()
                .iter()
                .any(|entry| entry.text.contains("不匹配")),
            "It is necessary to keep the annotation unmatched"
        );

        // Legacy session-id only for goose and only without resume.json.
        std::fs::write(&paths.session_id, "legacy-session").unwrap();
        std::fs::write(&paths.resume, "{not json").unwrap();
        assert_eq!(read_resume_candidate(&paths, &launch, true, &view), None);
        assert!(
            view.transcript_snapshot()
                .iter()
                .any(|entry| entry.text.contains("无法解析"))
        );
        assert!(
            !paths.resume.exists() && !paths.session_id.exists(),
            "The damaged resume.json file must be cleaned up, and cannot be reverted to the legacy session-id mode."
        );
        std::fs::write(&paths.session_id, "legacy-session").unwrap();
        assert_eq!(
            read_resume_candidate(&paths, &launch, true, &view).as_deref(),
            Some("legacy-session"),
            "In older versions of Goose, even when there is no resume.json file, attempts are still made to restore the session-id."
        );
        assert!(
            view.transcript_snapshot()
                .iter()
                .any(|entry| entry.text.contains("旧版 session-id"))
        );
        assert_eq!(
            read_resume_candidate(&paths, &launch, false, &view),
            None,
            "Legacy session IDs must not be used for purposes other than those specified."
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn persist_resume_state_writes_both_markers_and_roundtrips() {
        let work =
            std::env::temp_dir().join(format!("duthub-resume-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&work).unwrap();
        let paths = sandbox::prepare(&work, "dut-resume-write").unwrap();
        let sha = "c".repeat(64);
        let launch = launch_spec("goose", &sha);
        persist_resume_state(&paths, &launch, "new-session").unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.session_id).unwrap(),
            "new-session"
        );
        let state = read_resume_state(&paths.resume).unwrap().unwrap();
        assert_eq!(state.agent_id, "goose");
        assert_eq!(state.program_sha256, sha);
        assert_eq!(state.session_id, "new-session");
        let view = SessionView::new();
        remove_resume_markers(&paths, &view);
        assert!(!paths.resume.exists() && !paths.session_id.exists());
        let _ = std::fs::remove_dir_all(&work);
    }

    /// goose definition for the worker-path tests: `DUTHUB_TEST_GOOSE` wins,
    /// otherwise the product resolver decides (downloaded install > PATH).
    /// `None` = goose is not installed (goose is no longer embedded), so the
    /// caller skips with a visible reason.
    #[cfg(windows)]
    fn test_goose_agent() -> Option<AgentDefinition> {
        if let Some(raw) = std::env::var_os("DUTHUB_TEST_GOOSE") {
            let program = std::path::PathBuf::from(raw);
            assert!(
                program.is_absolute() && program.is_file(),
                "DUTHUB_TEST_GOOSE must be a valid absolute path to the goose executable file: {}",
                program.display()
            );
            let sha256 = duthub_core::store::sha256_file(&program).ok()?;
            let mut agent = AgentDefinition::goose();
            agent.program = Some(program.display().to_string());
            agent.expected_sha256 = Some(sha256);
            return Some(agent);
        }
        crate::agent_host::registry::goose_install_state()
            .program()
            .is_some()
            .then(AgentDefinition::goose)
    }

    #[cfg(windows)]
    #[test]
    fn unreachable_mcp_marks_session_failed_and_stops_process() {
        let Some(agent) = test_goose_agent() else {
            eprintln!(
                "SKIP: Goose is unavailable. Install it from Tool Downloads or set DUTHUB_TEST_GOOSE to the executable."
            );
            return;
        };
        let work =
            std::env::temp_dir().join(format!("duthub-agent-host-unit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&work).unwrap();
        let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
        let access = Arc::new(AccessContext::new(
            auth.clone(),
            Arc::new(ControlLeaseManager::new(None)),
            AuditSink::memory(),
        ));
        let view = Arc::new(SessionView::new());
        view.set_status(SessionStatus::Starting);
        let acp = Arc::new(Mutex::new(None));
        let listener_slot = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicBool::new(false));
        let last_activity_ms = Arc::new(AtomicU64::new(now_ms()));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(false));
        let launch = Arc::new(Mutex::new(None));
        let context = WorkerContext {
            dut_id: "dut-unreachable-mcp".into(),
            agent,
            binding: Some(GooseBinding {
                provider: Provider::Openai,
                base_url: "http://127.0.0.1:1/v1".into(),
                model: "fake-model".into(),
                api_key: "sk-unit-secret".into(),
            }),
            launch,
            work_dir: work.clone(),
            view: view.clone(),
            state: Arc::new(SnapshotState::empty()),
            access,
            auth,
            events: None,
            acp: acp.clone(),
            listener_slot: listener_slot.clone(),
            pending,
            last_activity_ms,
            stop_flag,
            dirty: dirty.clone(),
            mcp_url_override: Some("http://127.0.0.1:1/mcp".into()),
        };
        let (_tx, rx) = mpsc::channel();
        run_worker(&context, &rx);

        match view.status() {
            SessionStatus::Failed(message) => {
                assert!(
                    message.contains("工具面")
                        || message.contains("MCP")
                        || message.contains("扩展")
                        || message.contains("session/new"),
                    "unexpected failure: {message}"
                );
            }
            other => panic!("An unreachable MCP endpoint must fail the session: {other:?}"),
        }
        assert!(
            acp.lock().unwrap().is_none(),
            "The failed path must release the ACP process (including all related processes)."
        );
        assert!(listener_slot.lock().unwrap().is_none());
        assert!(dirty.load(Ordering::Relaxed));
        let _ = std::fs::remove_dir_all(&work);
    }
}
