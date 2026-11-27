//! Serial runtime for session management, event delivery, and logging.
//!
//! `DeviceService` exposes synchronous methods for direct calls from the GUI and TUI.
//! Each session owns one task for both reads and writes on the Tokio runtime.

#![forbid(unsafe_code)]

pub mod boot_reference;
pub mod bus;
pub mod control;
pub mod demo;
pub mod garbled;
pub mod heartbeat;
pub mod logs;
#[cfg(feature = "real-hardware")]
pub mod real;
pub mod rec;
pub mod relay_watch;
pub mod session;
pub mod term;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use tokio::sync::broadcast;

use duthub_core::audit::AuditSink;
use duthub_core::ipc::EventEnvelope;
use duthub_core::lease::{ControlLeaseManager, LeaseHolder, WriteCtx, WriteIdentity};
use duthub_core::{DeviceError, DeviceResult, SerialPortInfo};

use crate::bus::EventBus;
use crate::logs::SessionLogWriter;
use crate::session::Registry;
pub use crate::session::{LineEnding, SessionCommand, SessionKind};

pub(crate) enum RuntimeOwner {
    Owned(tokio::runtime::Runtime),
    Shared(tokio::runtime::Handle),
}

impl RuntimeOwner {
    pub(crate) fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self {
            RuntimeOwner::Owned(rt) => rt.spawn(future),
            RuntimeOwner::Shared(handle) => handle.spawn(future),
        }
    }

    pub(crate) fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        match self {
            RuntimeOwner::Owned(rt) => rt.block_on(future),
            RuntimeOwner::Shared(handle) => handle.block_on(future),
        }
    }
}

pub struct DeviceService {
    runtime: RuntimeOwner,
    bus: Arc<EventBus>,
    registry: Arc<Mutex<Registry>>,
    logs_dir: PathBuf,
    archive_saving: Option<Arc<AtomicBool>>,
    artifact_quota: Option<Arc<duthub_core::dut_quota::DutQuota>>,
    demo_delay_ms: u64,
    /// Control lease manager shared with MCP and GUI layers.
    lease: Option<Arc<ControlLeaseManager>>,
    /// Audit sink recording write admission and denial events.
    audit: Option<Arc<AuditSink>>,
    /// Session log switch from serial settings; enabled by default.
    session_log_enabled: AtomicBool,
    /// Terminal scrollback capacity for newly opened sessions.
    scrollback_rows: AtomicUsize,
    /// Maximum rows carried in each screen snapshot.
    snapshot_max_rows: AtomicUsize,
    /// Per-DUT garbled-byte detection switch shared with active sessions.
    garble_enabled: Arc<Mutex<std::collections::HashMap<String, bool>>>,
}

impl std::fmt::Debug for DeviceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceService")
            .field("logs_dir", &self.logs_dir)
            .field("demo_delay_ms", &self.demo_delay_ms)
            .finish_non_exhaustive()
    }
}

impl DeviceService {
    /// Construct a `DeviceService` with its own background Tokio runtime.
    #[must_use]
    pub fn custom(bus_capacity: usize, logs_dir: PathBuf, demo_delay_ms: u64) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        Self::with_runtime_owner(
            RuntimeOwner::Owned(runtime),
            bus_capacity,
            logs_dir,
            demo_delay_ms,
        )
    }

    /// Construct a `DeviceService` sharing an externally owned Tokio runtime handle.
    #[must_use]
    pub fn with_runtime(
        handle: tokio::runtime::Handle,
        bus_capacity: usize,
        logs_dir: PathBuf,
        demo_delay_ms: u64,
    ) -> Self {
        Self::with_runtime_owner(
            RuntimeOwner::Shared(handle),
            bus_capacity,
            logs_dir,
            demo_delay_ms,
        )
    }

    fn with_runtime_owner(
        runtime: RuntimeOwner,
        bus_capacity: usize,
        logs_dir: PathBuf,
        demo_delay_ms: u64,
    ) -> Self {
        Self {
            runtime,
            bus: Arc::new(EventBus::new(bus_capacity)),
            registry: Arc::new(Mutex::new(Registry::default())),
            logs_dir,
            archive_saving: None,
            artifact_quota: None,
            demo_delay_ms,
            lease: None,
            audit: None,
            session_log_enabled: AtomicBool::new(true),
            scrollback_rows: AtomicUsize::new(crate::term::SCROLLBACK_HISTORY),
            snapshot_max_rows: AtomicUsize::new(crate::term::SNAPSHOT_MAX_ROWS),
            garble_enabled: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    #[must_use]
    pub fn with_log_archives(mut self, enabled: bool) -> Self {
        self.archive_saving = Some(Arc::new(AtomicBool::new(enabled)));
        self
    }

    /// Whether to persist session logs, the inverse of serial.sessionLogDisabled.
    #[must_use]
    pub fn with_session_logging(self, enabled: bool) -> Self {
        self.session_log_enabled.store(enabled, Ordering::Release);
        self
    }

    /// Number of terminal scrollback rows, with a minimum of 100.
    /// Retain the requested history and limit the size of each live frame.
    #[must_use]
    pub fn with_scrollback(self, rows: usize) -> Self {
        self.configure_scrollback(Some(rows));
        self
    }

    pub fn configure_scrollback(&self, rows: Option<usize>) {
        let (history, snapshot) = rows.map_or(
            (
                crate::term::SCROLLBACK_HISTORY,
                crate::term::SNAPSHOT_MAX_ROWS,
            ),
            |rows| (rows, rows.min(crate::term::LIVE_SNAPSHOT_ROW_CAP)),
        );
        self.set_terminal_buffer(history, snapshot);
    }

    /// Update the scrollback capacity and snapshot row limit after settings are saved.
    /// These settings apply to newly opened sessions.
    pub fn set_terminal_buffer(&self, history: usize, snapshot: usize) {
        self.scrollback_rows.store(
            history.clamp(
                crate::term::MIN_SCROLLBACK_ROWS,
                crate::term::MAX_SCROLLBACK_ROWS,
            ),
            Ordering::Release,
        );
        self.snapshot_max_rows.store(
            snapshot.clamp(
                crate::term::MIN_SCROLLBACK_ROWS,
                crate::term::LIVE_SNAPSHOT_ROW_CAP,
            ),
            Ordering::Release,
        );
    }

    /// Current history and snapshot limits, after applying the 100-row minimum.
    /// Used by the serial settings view and tests.
    pub fn terminal_buffer(&self) -> (usize, usize) {
        (
            self.scrollback_rows.load(Ordering::Acquire),
            self.snapshot_max_rows.load(Ordering::Acquire),
        )
    }

    /// Update both active log writers and the default for new sessions.
    /// MCP serial reads and UI notices continue to receive bus events.
    pub fn set_session_logging(&self, enabled: bool) {
        let writers: Vec<Arc<Mutex<SessionLogWriter>>> = {
            let registry = duthub_core::util::lock_unpoisoned(&self.registry);
            registry
                .session_ids()
                .iter()
                .filter_map(|id| registry.get(id))
                .map(|handle| handle.logs.clone())
                .collect()
        };
        for logs in writers {
            if let Ok(mut writer) = logs.lock() {
                writer.set_enabled(enabled);
            }
        }
        self.session_log_enabled.store(enabled, Ordering::Release);
    }

    /// Per-DUT garble detection. The map holds entries only for enabled DUTs.
    /// Active sessions share the map and apply changes on the next RX block or snapshot.
    /// `open_named_session` gives new sessions a reference to the same map.
    pub fn set_garbled_detection_for(&self, dut_id: &str, enabled: bool) {
        let mut switches = match self.garble_enabled.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if enabled {
            switches.insert(dut_id.to_string(), true);
        } else {
            switches.remove(dut_id);
        }
    }

    /// Whether garble detection is enabled for this DUT. The default is off.
    pub fn garbled_detection_enabled(&self, dut_id: &str) -> bool {
        match self.garble_enabled.lock() {
            Ok(switches) => switches.get(dut_id).copied().unwrap_or(false),
            Err(_) => false,
        }
    }

    #[must_use]
    pub fn with_portable_archives(
        mut self,
        root: PathBuf,
        enabled: bool,
        quota: Arc<duthub_core::dut_quota::DutQuota>,
    ) -> Self {
        self.logs_dir = root;
        self.artifact_quota = Some(quota);
        self.with_log_archives(enabled)
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "public API used by desktop callers"
    )]
    pub fn configure_serial_log(
        &self,
        dut_id: &str,
        options: SerialLogOptions<'_>,
    ) -> DeviceResult<()> {
        let (logs, sender) = {
            let registry = duthub_core::util::lock_unpoisoned(&self.registry);
            let Some(handle) = registry.get(dut_id) else {
                return Ok(());
            };
            (handle.logs.clone(), handle.log_tx.clone())
        };
        let reference = options.load_reference(dut_id);
        if let Some(sender) = sender {
            sender
                .send(crate::demo::LogMsg::SetLogMode {
                    split: options.split,
                    reference,
                })
                .map_err(|_| DeviceError::Invalid("日志写线程已退出".into()))
        } else {
            duthub_core::util::lock_unpoisoned(&logs).set_log_mode(options.split, reference)
        }
    }

    pub fn apply_boot_reference(&self, dut_id: &str, path: &std::path::Path) -> DeviceResult<()> {
        let reference = boot_reference::BootReference::load(path)
            .map_err(|error| DeviceError::Invalid(error.to_string()))?;
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let Some(handle) = registry.get(dut_id) else {
            return Ok(());
        };
        if let Some(sender) = &handle.log_tx {
            sender
                .send(crate::demo::LogMsg::SetBootReference(reference))
                .map_err(|_| DeviceError::Invalid("日志写线程已退出".into()))
        } else {
            let mut writer = duthub_core::util::lock_unpoisoned(&handle.logs);
            writer.flush_rx_prefix()?;
            writer.set_boot_reference(Some(reference));
            Ok(())
        }
    }

    pub fn log_control_boundary(&self, dut_id: &str, label: &str, split: bool) -> DeviceResult<()> {
        let (logs, log_tx) = {
            let registry = duthub_core::util::lock_unpoisoned(&self.registry);
            let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
            (handle.logs.clone(), handle.log_tx.clone())
        };
        let line = format!("[mark] {label}");
        if split {
            // Complete the durable boundary after earlier RX work and before
            // the control service dispatches a power operation.
            if let Some(tx) = log_tx {
                let (ack, rx) = std::sync::mpsc::channel();
                if tx
                    .send(crate::demo::LogMsg::MarkSync {
                        line: line.clone(),
                        ack,
                    })
                    .is_ok()
                {
                    rx.recv()
                        .map_err(|_| DeviceError::Invalid("日志写线程已退出，分段未确认".into()))?
                } else {
                    // Writer thread gone: inline mark + companion rec record.
                    let mut writer = duthub_core::util::lock_unpoisoned(&logs);
                    writer.power_mark(&line)
                }
            } else {
                let mut writer = duthub_core::util::lock_unpoisoned(&logs);
                writer.power_mark(&line)
            }
        } else {
            let mut writer = duthub_core::util::lock_unpoisoned(&logs);
            let result = writer.append_text(&line);
            let _ = writer.append_rec(crate::rec::RecKind::Note, line.as_bytes());
            result
        }
    }

    /// Set the lease manager for serial write admission and automatic GUI leases.
    #[must_use]
    pub fn with_lease_authority(mut self, lease: Arc<ControlLeaseManager>) -> Self {
        self.lease = Some(lease);
        self
    }

    /// Set the sink for write-denial audit records. The lease manager audits its own events.
    #[must_use]
    pub fn with_audit_sink(mut self, audit: Arc<AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// The lease manager shared by MCP control tools and the GUI control panel.
    pub fn lease_manager(&self) -> Option<Arc<ControlLeaseManager>> {
        self.lease.clone()
    }

    pub fn audit_sink(&self) -> Option<Arc<AuditSink>> {
        self.audit.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.bus.subscribe()
    }

    pub fn read_log(&self, session_id: &str, since_seq: u64, limit: usize) -> Vec<EventEnvelope> {
        self.bus.read_log(session_id, since_seq, limit)
    }

    pub fn read_log_tail(&self, session_id: &str, limit: usize) -> Vec<EventEnvelope> {
        self.bus.read_log_tail(session_id, limit)
    }

    pub fn read_serial_page(
        &self,
        session_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> (u64, Vec<EventEnvelope>) {
        self.bus.read_serial_page(session_id, since_seq, limit)
    }

    /// Latest buffered sequence number, or zero if the session or its buffer is absent.
    pub fn session_last_seq(&self, dut_id: &str) -> u64 {
        self.bus.last_seq(&format!("sess-{dut_id}"))
    }

    /// Return the latest rendered VT100 screen as plain text for an active session.
    pub fn serial_screen_text(&self, dut_id: &str) -> Option<String> {
        let screen = self.bus.latest_screen(&format!("sess-{dut_id}"))?;
        Some(crate::term::render_screen_text(&screen))
    }

    /// Write a storage-only boundary. An unopened session returns `NotFound`.
    pub fn log_mark(&self, dut_id: &str, label: &str) -> DeviceResult<()> {
        if self.archive_saving.is_some() {
            return self.log_control_boundary(dut_id, label, true);
        }
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        handle
            .cmd_tx
            .try_send(session::SessionCommand::Mark(label.to_string()))
            .map_err(|e| match e {
                tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
            })
    }

    /// Wait for the session log writer to persist all currently queued messages.
    /// Readers and tests can use this barrier while the session is still open.
    pub fn flush_session_logs(&self, dut_id: &str) {
        let tx = {
            let registry = duthub_core::util::lock_unpoisoned(&self.registry);
            registry.get(dut_id).and_then(|h| h.log_tx.clone())
        };
        if let Some(tx) = tx {
            let (ack, rx) = std::sync::mpsc::channel();
            if tx.send(crate::demo::LogMsg::Flush(ack)).is_ok() {
                let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
            }
        }
    }

    /// Enumerate serial ports. Hardware builds return only ports discovered by the OS.
    pub fn list_ports(&self) -> Vec<SerialPortInfo> {
        #[cfg(feature = "real-hardware")]
        {
            real::available_ports().unwrap_or_else(|error| {
                tracing::warn!(%error, "serial port enumeration failed");
                Vec::new()
            })
        }
        #[cfg(not(feature = "real-hardware"))]
        {
            demo_ports()
        }
    }

    /// Open a session and return its sess-<dut> ID. A port already in use returns Busy.
    pub fn open_session(
        &self,
        dut_id: &str,
        port: &str,
        baud: u32,
        kind: SessionKind,
    ) -> DeviceResult<String> {
        self.open_named_session(dut_id, dut_id, port, baud, kind)
    }

    #[tracing::instrument(name = "serial.open", skip(self))]
    pub fn open_named_session(
        &self,
        dut_id: &str,
        name: &str,
        port: &str,
        baud: u32,
        kind: SessionKind,
    ) -> DeviceResult<String> {
        self.open_named_session_with_log_options(
            dut_id,
            name,
            port,
            baud,
            kind,
            SerialLogOptions {
                split: true,
                reference: None,
            },
        )
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "public API used by desktop callers"
    )]
    pub fn open_named_session_with_log_options(
        &self,
        dut_id: &str,
        name: &str,
        port: &str,
        baud: u32,
        kind: SessionKind,
        options: SerialLogOptions<'_>,
    ) -> DeviceResult<String> {
        let reference = options.load_reference(dut_id);
        let mut registry = duthub_core::util::lock_unpoisoned(&self.registry);
        if registry.is_reserved(dut_id) {
            return Err(DeviceError::Busy);
        }
        registry.lock_port("local", port)?;
        let session_id = format!("sess-{dut_id}");
        let mut writer = if let Some(saving) = &self.archive_saving {
            if let Some(quota) = &self.artifact_quota {
                SessionLogWriter::portable_archive(
                    &self.logs_dir,
                    dut_id,
                    name,
                    saving.clone(),
                    quota.clone(),
                )
            } else {
                SessionLogWriter::archive(&self.logs_dir, dut_id, name, saving.clone())
            }
        } else {
            match SessionLogWriter::new(&self.logs_dir, &session_id) {
                Ok(writer) => writer,
                Err(err) => {
                    registry.unlock_port("local", port);
                    return Err(err);
                }
            }
        };
        if let Err(error) = writer.set_log_mode(options.split, reference) {
            registry.unlock_port("local", port);
            return Err(error);
        }
        let logs = Arc::new(Mutex::new(writer));
        // Initialize the writer from the session logging setting.
        if !self.session_log_enabled.load(Ordering::Acquire) {
            duthub_core::util::lock_unpoisoned(&logs).set_enabled(false);
        }
        self.bus.drop_session(&session_id);
        let bus = self.bus.clone();
        let session_for_task = session_id.clone();
        let opened_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spec = session::SessionSpec {
            dut_id,
            session_id: &session_for_task,
            kind,
            port,
            baud,
            demo_line_delay_ms: self.demo_delay_ms,
            scrollback_rows: self.scrollback_rows.load(Ordering::Acquire),
            snapshot_max_rows: self.snapshot_max_rows.load(Ordering::Acquire),
            logs,
            opened_flag: Some(opened_flag.clone()),
            lease_authority: self
                .lease
                .clone()
                .map(|m| m.clone() as Arc<dyn duthub_core::lease::LeaseAuthority>),
            audit: self.audit.clone(),
            garble_enabled: Arc::clone(&self.garble_enabled),
        };
        let result = self
            .runtime
            .block_on(async move { session::spawn_session(bus, spec) });
        match result {
            Ok((mut handle, _cmd)) => {
                let join = handle
                    .join
                    .take()
                    .expect("spawned session has a join handle");
                let actor = join.id();
                let host = handle.host.clone();
                let port = handle.port.clone();
                let observed_session = session_id.clone();
                registry.insert(handle);
                if let Some(lease) = &self.lease {
                    lease.set_gui_session_name(dut_id, format!("GUI/{port}"));
                    let _ = lease.acquire(dut_id, &LeaseHolder::Gui, None);
                }
                drop(registry);

                let registry = self.registry.clone();
                let bus = self.bus.clone();
                let lease = self.lease.clone();
                let dut_id = dut_id.to_owned();
                self.runtime.spawn(async move {
                    let _ = join.await;
                    let final_seq = {
                        let mut registry = duthub_core::util::lock_unpoisoned(&registry);
                        if registry.finish(&dut_id, actor).is_none() {
                            return;
                        }
                        if let Some(lease) = lease {
                            lease.release_for_holder(&dut_id, &LeaseHolder::Gui);
                        }
                        let final_seq = bus.last_seq(&observed_session);
                        registry.unlock_port(&host, &port);
                        final_seq
                    };
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    bus.expire_closed_session(&observed_session, final_seq);
                });
                Ok(session_id)
            }
            Err(err) => {
                registry.unlock_port("local", port);
                Err(err)
            }
        }
    }

    /// Request closure. The actor releases its port and lease after it exits.
    #[tracing::instrument(name = "serial.close", skip(self), fields(dut_id, ok))]
    pub fn close_session(&self, dut_id: &str) -> DeviceResult<()> {
        let mut registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let Some(handle) = registry.begin_close(dut_id) else {
            tracing::warn!(dut_id, "serial.close not found");
            return Err(DeviceError::NotFound);
        };
        let _ = handle.cmd_tx.try_send(session::SessionCommand::Close);
        let abort = handle.abort.clone();
        drop(handle);
        drop(registry);
        self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if !abort.is_finished() {
                abort.abort();
            }
        });
        Ok(())
    }

    /// Queue a line for the session, applying its configured line ending.
    ///
    /// Normalize CRLF and standalone LF to CR before sending multiline text (P2-034).
    /// Bare LF disrupts shell editing echoes and can create unprefixed TX log lines.
    /// The `LineEnding` policy still applies to the end of the normalized payload.
    /// Raw keyboard bytes bypass this step. The UI normalizes pasted text separately.
    ///
    /// Check the writer identity while holding the registry lock, before `try_send`.
    /// The GUI has a human lease, and an Agent must authenticate its lease.
    /// See `WriteIdentity` for the system heartbeat exception.
    #[tracing::instrument(name = "serial.write", skip(self, text), fields(dut_id, len = text.len(), ok))]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "Keep the existing public write API"
    )]
    pub fn write_line(&self, dut_id: &str, writer: WriteIdentity, text: &str) -> DeviceResult<()> {
        // Collapse CRLF first so the standalone LF pass cannot leave duplicated CRs.
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        self.enqueue_line(dut_id, &writer, |lease| SessionCommand::Write {
            text: normalized,
            lease,
        })
    }

    pub(crate) fn write_heartbeat_batch(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
        batch: Arc<heartbeat::HeartbeatBatch>,
    ) -> DeviceResult<()> {
        self.enqueue_line(dut_id, writer, |lease| {
            SessionCommand::WriteHeartbeatBatch {
                text: batch.wire_text(),
                batch,
                lease,
            }
        })
    }

    fn enqueue_line(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
        command: impl FnOnce(Option<WriteCtx>) -> SessionCommand,
    ) -> DeviceResult<()> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        // Check the lease and generation at admission. MCP has already authorized the request.
        let lease = self.admit(dut_id, writer)?;
        handle.cmd_tx.try_send(command(lease)).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
        })
    }

    #[tracing::instrument(name = "serial.set_baud", skip(self), fields(dut_id, baud, ok))]
    pub fn set_baud(&self, dut_id: &str, baud: u32) -> DeviceResult<()> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        handle
            .cmd_tx
            .try_send(session::SessionCommand::SetBaud(baud))
            .map_err(|_| DeviceError::Disconnected)
    }

    /// Set the line ending for subsequent writes (none, LF, or CRLF).
    pub fn set_line_ending(&self, dut_id: &str, ending: LineEnding) -> DeviceResult<()> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        handle
            .cmd_tx
            .try_send(session::SessionCommand::SetEnding(ending))
            .map_err(|_| DeviceError::Disconnected)
    }

    /// Queue raw keyboard bytes without appending a line ending or emitting a TX echo.
    /// The UI uses this path for characters, CR for Enter, and DEL for Backspace.
    /// Admission and the serial actor enforce the same lease rules as `Self::write_line`.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "public API used by desktop callers"
    )]
    pub fn write_bytes(
        &self,
        dut_id: &str,
        writer: WriteIdentity,
        bytes: &[u8],
    ) -> DeviceResult<()> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        let lease = self.admit(dut_id, &writer)?;
        handle
            .cmd_tx
            .try_send(session::SessionCommand::WriteBytes {
                bytes: bytes.to_vec(),
                lease,
            })
            .map_err(|e| match e {
                tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
            })
    }

    /// Check the lease before enqueuing a write and while holding the registry lock.
    /// Return Ok(None) when no lease manager was configured.
    /// Audit denials as `write.deny` and return `DeviceError::Lease`. GUI priority still applies.
    fn admit(&self, dut_id: &str, writer: &WriteIdentity) -> Result<Option<WriteCtx>, DeviceError> {
        let Some(lease) = &self.lease else {
            return Ok(None);
        };
        match lease.admit_queued_write(dut_id, writer) {
            Ok(ctx) => {
                // Skip per-keystroke GUI audit entries to avoid flooding the audit log.
                // Record admitted Agent or preapproved writes as write.admit.
                if matches!(
                    writer,
                    WriteIdentity::Agent { .. } | WriteIdentity::Approval
                ) && let Some(audit) = &self.audit
                {
                    let who = match writer {
                        WriteIdentity::Approval => "approval",
                        _ => "agent",
                    };
                    let detail = match &ctx {
                        Some(c) if !c.lease_id.is_empty() => {
                            format!("{who} 写入 lease={} gen={}", c.lease_id, c.generation)
                        }
                        Some(c) if who == "approval" => {
                            format!("{who} 写入（审批放行，纪元 fence gen={}）", c.generation)
                        }
                        _ => format!("{who} 写入（无租约，auth 关过渡）"),
                    };
                    audit.record(
                        "write.admit",
                        None,
                        None,
                        Some(dut_id),
                        duthub_core::audit::AuditOutcome::Ok,
                        Some(&detail),
                    );
                }
                Ok(ctx)
            }
            Err(err) => {
                if let Some(audit) = &self.audit {
                    audit.record(
                        "write.deny",
                        None,
                        None,
                        Some(dut_id),
                        duthub_core::audit::AuditOutcome::Denied,
                        Some(&err.message()),
                    );
                }
                Err(err.to_device_error())
            }
        }
    }

    /// Resize the terminal grid for a real session after the window is resized.
    /// Ignore a missing session. The UI resends the dimensions after the session opens.
    pub fn resize_terminal(&self, dut_id: &str, cols: usize, rows: usize) -> DeviceResult<()> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        handle
            .cmd_tx
            .try_send(session::SessionCommand::Resize { cols, rows })
            .map_err(|_| DeviceError::Disconnected)
    }

    /// Reset the current session's local terminal emulation.
    pub fn reset_terminal(&self, dut_id: &str) -> DeviceResult<()> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
        handle
            .cmd_tx
            .try_send(SessionCommand::ResetTerminal)
            .map_err(|err| match err {
                tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
            })
    }

    pub fn read_terminal_history(
        &self,
        dut_id: &str,
        skip: usize,
    ) -> DeviceResult<tokio::sync::oneshot::Receiver<crate::term::TerminalHistoryPage>> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
        let (reply, receiver) = tokio::sync::oneshot::channel();
        handle
            .cmd_tx
            .try_send(SessionCommand::ReadTerminalHistory { skip, reply })
            .map_err(|err| match err {
                tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
            })?;
        Ok(receiver)
    }

    /// Wake a reconnect backoff so it retries immediately.
    /// On a healthy connection, `notify_one` leaves one permit for the next backoff.
    pub fn manual_reconnect(&self, dut_id: &str) -> DeviceResult<()> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
        handle.reconnect_notify.notify_one();
        Ok(())
    }

    /// Summaries of all active sessions for rendering the UI tabs.
    pub fn sessions(&self) -> Vec<SessionSummary> {
        let registry = duthub_core::util::lock_unpoisoned(&self.registry);
        registry
            .session_ids()
            .into_iter()
            .filter_map(|dut| {
                registry.get(&dut).map(|h| SessionSummary {
                    dut_id: h.dut_id.clone(),
                    session_id: h.session_id.clone(),
                    kind: h.kind,
                    port: h.port.clone(),
                })
            })
            .collect()
    }

    pub fn session_source(&self, dut_id: &str) -> Option<&'static str> {
        match duthub_core::util::lock_unpoisoned(&self.registry)
            .get(dut_id)
            .map(|h| h.kind)
        {
            Some(SessionKind::Demo) => Some("demo"),
            Some(SessionKind::Real) => Some("device"),
            None => None,
        }
    }

    pub fn real_session_opened(&self, dut_id: &str) -> bool {
        self.registry
            .lock()
            .unwrap()
            .get(dut_id)
            .is_some_and(|handle| {
                handle.kind == SessionKind::Real && handle.opened_flag.load(Ordering::Acquire)
            })
    }

    pub fn logs_dir(&self) -> &PathBuf {
        &self.logs_dir
    }

    /// `.rec` segments of a session's structured record log, oldest first.
    /// Normal mode: `logs_dir/sess-{dut_id}.rec(.N)`. Archive modes locate the
    /// DUT folder by stable `dut_id` (owner-file match — the folder name follows
    /// the archived DUT name and may differ from any display name), then
    /// collect both layouts (portable `logs/NAME-*.rec`, flat
    /// `dut-<hex>-*.rec`).
    pub fn session_rec_segments(&self, dut_id: &str) -> Vec<PathBuf> {
        if self.archive_saving.is_none() {
            return crate::rec::list_rec_segments(&self.logs_dir, &format!("sess-{dut_id}"));
        }
        let Ok(entries) = std::fs::read_dir(&self.logs_dir) else {
            return Vec::new();
        };
        let mut folder = None;
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();
            // Owner-file match ties the folder to this stable dut_id regardless
            // of what the archived DUT is named.
            if duthub_core::dut_artifacts::open(&self.logs_dir, dut_id, &dir_name, false).is_ok() {
                folder = Some(entry.path());
                break;
            }
        }
        let Some(folder) = folder else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for dir in [
            folder.join(duthub_core::dut_artifacts::SERIAL_LOGS_DIR),
            folder.join(duthub_core::dut_artifacts::LEGACY_LOGS_DIR),
            folder,
        ] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let prefix = duthub_core::log_archive::archive_prefix(dut_id);
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().to_string();
                let owned = std::path::Path::new(&file_name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("rec"))
                    && (dir.ends_with(duthub_core::dut_artifacts::SERIAL_LOGS_DIR)
                        || dir.ends_with(duthub_core::dut_artifacts::LEGACY_LOGS_DIR)
                        || file_name.starts_with(&prefix));
                if owned {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        found
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub dut_id: String,
    pub session_id: String,
    pub kind: SessionKind,
    pub port: String,
}

#[derive(Debug)]
pub struct SerialLogOptions<'a> {
    pub split: bool,
    pub reference: Option<&'a std::path::Path>,
}

impl SerialLogOptions<'_> {
    fn load_reference(&self, dut_id: &str) -> Option<boot_reference::BootReference> {
        self.reference.filter(|_| self.split).and_then(|path| {
            match boot_reference::BootReference::load(path) {
                Ok(reference) => Some(reference),
                Err(error) => {
                    tracing::warn!(%dut_id, path = %path.display(), %error, "Could not read the boot log reference");
                    None
                }
            }
        })
    }
}

#[cfg(not(feature = "real-hardware"))]
fn demo_ports() -> Vec<SerialPortInfo> {
    vec![
        SerialPortInfo {
            port: "COM3".into(),
            description: Some("USB-SERIAL CH340 (demo)".into()),
        },
        SerialPortInfo {
            port: "COM7".into(),
            description: Some("Silicon Labs CP2102 (demo)".into()),
        },
        SerialPortInfo {
            port: "COM12".into(),
            description: Some("FTDI FT232R (demo)".into()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::ipc::UiEvent;
    use std::time::Duration;

    fn test_service(tag: &str) -> DeviceService {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DeviceService::custom(64, dir, 0)
    }

    fn absorb_texts(rx: &mut broadcast::Receiver<EventEnvelope>) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(env) => {
                    if let UiEvent::SerialChunk { text, .. } = env.event {
                        out.push(text);
                    }
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        out
    }

    #[test]
    fn open_twice_on_same_port_is_busy() {
        let service = test_service("busy");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        let err = service
            .open_session("dut-b", "COM3", 115_200, SessionKind::Demo)
            .unwrap_err();
        assert!(matches!(err, DeviceError::Busy));
        // A different port can open independently.
        service
            .open_session("dut-b", "COM5", 115_200, SessionKind::Demo)
            .unwrap();
    }

    #[test]
    fn same_dut_cannot_open_twice() {
        let service = test_service("dupdut");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        assert!(
            service
                .open_session("dut-a", "COM7", 115_200, SessionKind::Demo)
                .is_err()
        );
    }

    #[test]
    fn close_releases_port_and_emits_disconnected() {
        let service = test_service("close");
        let _rx = service.subscribe();
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        service.close_session("dut-a").unwrap();
        // A different DUT can take the port only after the original actor exits.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match service.open_session("dut-b", "COM3", 115_200, SessionKind::Demo) {
                Ok(_) => break,
                Err(DeviceError::Busy) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => panic!("expected eventual port release: {result:?}"),
            }
        }
        // The task publishes Disconnected when it exits.
        let mut disconnected = false;
        for _ in 0..100 {
            disconnected = !service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .filter(|e| matches!(e.event, UiEvent::SerialDisconnected { .. }))
                .collect::<Vec<_>>()
                .is_empty();
            if disconnected {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(disconnected, "closing must emit a Disconnected event");
    }

    #[test]
    fn write_line_reaches_demo_driver() {
        let service = test_service("write");
        let mut rx = service.subscribe();
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(120)); // Boot logs.
        let _ = absorb_texts(&mut rx);
        service
            .write_line("dut-a", WriteIdentity::gui(), "uname -a")
            .unwrap();
        let mut found = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            let log = service.read_log("sess-dut-a", 0, usize::MAX);
            found = log.iter().any(|e| {
                matches!(&e.event, UiEvent::SerialChunk { text, .. } if text.contains("aarch64 GNU/Linux"))
            });
            if found {
                break;
            }
        }
        assert!(found, "write should receive simulated uname response");
        service.close_session("dut-a").ok();
    }

    #[test]
    fn logs_are_written_to_disk() {
        let service = test_service("logs");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        service
            .write_line("dut-a", WriteIdentity::gui(), "uptime")
            .unwrap();
        std::thread::sleep(Duration::from_millis(80));
        service.close_session("dut-a").ok();
        let text_file = service.logs_dir().join("sess-dut-a.log");
        let raw_file = service.logs_dir().join("sess-dut-a.bin");
        let text = std::fs::read_to_string(&text_file).expect("text log should exist");
        assert!(text.contains("Started Serial Getty"));
        assert!(text.contains("uptime"));
        let raw = std::fs::read(&raw_file).expect("raw log should exist");
        assert!(std::str::from_utf8(&raw).unwrap().contains("Serial Getty"));
    }

    /// Writes emit TX echoes and a new line ending policy applies to the next write.
    #[test]
    fn set_line_ending_and_tx_echo_flow_through_service() {
        let service = test_service("tx-echo");
        let _rx = service.subscribe();
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        service
            .write_line("dut-a", WriteIdentity::gui(), "probe-1")
            .unwrap();
        service.set_line_ending("dut-a", LineEnding::Crlf).unwrap();
        service
            .write_line("dut-a", WriteIdentity::gui(), "probe-2")
            .unwrap();
        let mut texts = Vec::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            texts = service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .filter_map(|e| match &e.event {
                    UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            let have_a = texts.iter().any(|t| t == "[TX] probe-1");
            let have_b = texts.iter().any(|t| t == "[TX] probe-2");
            if have_a && have_b {
                break;
            }
        }
        assert!(
            texts.iter().any(|t| t == "[TX] probe-1"),
            "write must produce a TX echo line"
        );
        assert!(
            texts.iter().any(|t| t == "[TX] probe-2"),
            "write after SetEnding must still produce a TX echo"
        );
        // The raw log confirms the second write used CRLF.
        service.flush_session_logs("dut-a");
        let raw = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        assert!(
            raw.contains("probe-2\r\n"),
            "CRLF policy must be written to raw log"
        );
        // The first write used the default LF.
        assert!(raw.contains("probe-1\n"));
        assert!(
            !raw.contains("probe-1\r\n"),
            "default policy must not write CRLF"
        );
        service.close_session("dut-a").ok();
    }

    /// Multiline `write_line` sends CR between lines and appends the configured ending.
    /// The TX log entry stays on a single line.
    #[test]
    fn write_line_normalizes_newlines_to_cr() {
        let service = test_service("multiline");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        service
            .write_line("dut-a", WriteIdentity::gui(), "echo C1\r\necho C2")
            .unwrap();
        let mut done = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            done = service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(&e.event, UiEvent::SerialChunk { text, .. } if text.contains("Executed (demo)"))
                });
            if done {
                break;
            }
        }
        assert!(done, "write must reach the demo shell");
        service.close_session("dut-a").ok();
        // The raw log contains CR separators and the default LF ending.
        let raw = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        assert!(raw.contains("echo C1\recho C2\n"), "payload: {raw:?}");
        assert!(
            !raw.contains("echo C1\necho C2"),
            "bare LF must not pass through: {raw:?}"
        );
        assert!(
            !raw.contains("echo C1\r\n"),
            "CRLF must not pass through verbatim: {raw:?}"
        );
        // The TX note escapes CR as text and has no unprefixed continuation lines.
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-a.log")).unwrap();
        assert!(
            text.lines().any(|line| line == "[TX] echo C1\\recho C2"),
            ".log must contain single-line [TX] annotation: {text:?}"
        );
        assert!(
            !text.lines().any(|line| line == "echo C2"),
            ".log must not contain unprefixed continuation lines: {text:?}"
        );
    }

    /// Manual reconnect is harmless in an open demo session and returns `NotFound` if absent.
    #[test]
    fn manual_reconnect_is_wakeup_not_error() {
        let service = test_service("reconnect");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        service.manual_reconnect("dut-a").unwrap();
        std::thread::sleep(Duration::from_millis(60));
        // The demo session stays active after the wake-up and accepts writes.
        service
            .write_line("dut-a", WriteIdentity::gui(), "still-alive")
            .unwrap();
        assert!(matches!(
            service.manual_reconnect("dut-missing"),
            Err(duthub_core::DeviceError::NotFound)
        ));
        service.close_session("dut-a").ok();
    }

    #[test]
    fn log_mark_stays_on_disk_and_out_of_the_serial_stream() {
        let service = test_service("mark");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        assert!(matches!(
            service.log_mark("dut-missing", "x"),
            Err(DeviceError::NotFound)
        ));
        let path = service.logs_dir().join("sess-dut-a.log");
        let mark = "[mark] 继电器 power ON";
        service.log_mark("dut-a", "继电器 power ON").unwrap();
        for _ in 0..50 {
            if std::fs::read_to_string(&path).is_ok_and(|text| text.contains(mark)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(std::fs::read_to_string(&path).unwrap().contains(mark));
        assert!(
            std::fs::read(service.logs_dir().join("sess-dut-a.bin"))
                .is_ok_and(|raw| String::from_utf8_lossy(&raw).contains(mark))
        );
        assert!(
            !service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .any(|event| {
                    matches!(&event.event, UiEvent::SerialChunk { text, .. } if text.contains(mark))
                })
        );
        service.close_session("dut-a").unwrap();
    }

    /// Construct the same test service with an explicit session logging setting.
    fn test_service_with(tag: &str, session_logging: bool) -> DeviceService {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DeviceService::custom(64, dir, 0).with_session_logging(session_logging)
    }

    /// Write a line and wait for the demo response to reach the log.
    /// The flush barrier guarantees that earlier queued records have reached disk.
    fn write_until_on_disk(service: &DeviceService, dut: &str, line: &str, expect: &str) {
        service.write_line(dut, WriteIdentity::gui(), line).unwrap();
        for _ in 0..100 {
            service.flush_session_logs(dut);
            let path = service.logs_dir().join(format!("sess-{dut}.log"));
            if std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains(expect)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("The write did not reach disk within two seconds: {expect}");
    }

    /// Enqueue RX before submitting a synchronous segment marker.
    /// The marker must follow the queued RX and open the next segment.
    /// Read the files immediately after the call returns to verify the synchronous boundary.
    #[test]
    fn log_control_boundary_drains_pending_rx_before_mark() {
        let service = test_service("boundary-drain");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        // The demo response is published to the bus and queued for logging first.
        write_until_on_disk(&service, "dut-a", "probe-before-mark", "Executed (demo)");
        // The synchronous marker acknowledges after all earlier records are persisted.
        service
            .log_control_boundary("dut-a", "POWER ON BOUNDARY", true)
            .unwrap();
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-a.log")).unwrap();
        let rx_pos = text
            .find("Executed (demo)")
            .expect("RX response must be on disk");
        let mark_pos = text
            .find("[mark] POWER ON BOUNDARY")
            .expect("mark line must land in the current segment");
        assert!(
            mark_pos > rx_pos,
            "mark must follow previously queued RX: {text:?}"
        );
        assert!(
            service.logs_dir().join("sess-dut-a.log.1").exists(),
            "split=true must rotate to a new segment (.log.1)"
        );
        service.close_session("dut-a").ok();
    }

    #[test]
    fn log_control_boundary_reports_sync_error_at_rotation_cap() {
        let service = test_service("boundary-cap");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        let mut attempts = 0usize;
        let err = loop {
            attempts += 1;
            assert!(
                attempts <= 64,
                "rotation must fail at cap instead of succeeding indefinitely"
            );
            match service.log_control_boundary("dut-a", &format!("seg-{attempts}"), true) {
                Ok(()) => {}
                Err(err) => break err,
            }
        };
        assert_eq!(
            attempts, 21,
            "each mark must rotate: failure should occur on attempt 21 (MAX_ROTATIONS=20)"
        );
        assert!(
            err.to_string().contains("分段"),
            "expected rotation cap error: {err}"
        );
        assert!(
            service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .all(|event| {
                    !matches!(
                        &event.event,
                        UiEvent::SerialChunk { text, .. }
                            if text.contains("日志分段失败") || text.starts_with("[mark] seg-")
                    )
                })
        );
        service.close_session("dut-a").ok();
    }

    /// The logging switch reaches all active session writers.
    /// While disabled, both files stay byte-for-byte unchanged and bus events continue.
    /// Re-enabling logging appends to the same files.
    #[test]
    fn session_logging_toggle_fans_out_to_active_sessions() {
        let service = test_service("log-fanout");
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        service
            .open_session("dut-b", "COM5", 115_200, SessionKind::Demo)
            .unwrap();
        write_until_on_disk(&service, "dut-a", "probe-on-a", "Executed (demo)");
        write_until_on_disk(&service, "dut-b", "probe-on-b", "Executed (demo)");
        let first_log_before = std::fs::read(service.logs_dir().join("sess-dut-a.log")).unwrap();
        let first_bin_before = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        let second_log_before = std::fs::read(service.logs_dir().join("sess-dut-b.log")).unwrap();
        let second_bin_before = std::fs::read(service.logs_dir().join("sess-dut-b.bin")).unwrap();

        // Disable both active writers without suppressing bus events.
        service.set_session_logging(false);
        service
            .write_line("dut-a", WriteIdentity::gui(), "probe-off-a")
            .unwrap();
        service
            .write_line("dut-b", WriteIdentity::gui(), "probe-off-b")
            .unwrap();
        let mut bus_seen = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            bus_seen = service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(&e.event,
                        UiEvent::SerialChunk { text, .. } if text.contains("[TX] probe-off-a"))
                });
            if bus_seen {
                break;
            }
        }
        assert!(
            bus_seen,
            "bus events must continue when file logging is disabled"
        );
        service.flush_session_logs("dut-a");
        service.flush_session_logs("dut-b");
        assert_eq!(
            std::fs::read(service.logs_dir().join("sess-dut-a.log")).unwrap(),
            first_log_before,
            "dut-a .log must stay unchanged while logging is disabled"
        );
        assert_eq!(
            std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap(),
            first_bin_before,
            "dut-a .bin must stay unchanged while logging is disabled"
        );
        assert_eq!(
            std::fs::read(service.logs_dir().join("sess-dut-b.log")).unwrap(),
            second_log_before,
            "dut-b .log must stay unchanged while logging is disabled"
        );
        assert_eq!(
            std::fs::read(service.logs_dir().join("sess-dut-b.bin")).unwrap(),
            second_bin_before,
            "dut-b .bin must stay unchanged while logging is disabled"
        );

        // Resume writing to the existing files without rotating or recreating them.
        service.set_session_logging(true);
        write_until_on_disk(&service, "dut-a", "probe-resume-a", "Executed (demo)");
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-a.log")).unwrap();
        assert!(
            text.contains("[TX] probe-on-a") && text.contains("[TX] probe-resume-a"),
            "re-enabling must append to the same file: {text:?}"
        );
        service.close_session("dut-a").ok();
        service.close_session("dut-b").ok();
    }

    /// A service created with logging disabled opens sessions with inactive writers.
    /// Enabling logging updates the active writer and the default for subsequent sessions.
    /// Both existing and newly opened sessions then persist records.
    #[test]
    fn new_sessions_initialize_from_session_logging_flag() {
        let service = test_service_with("log-init", false);
        service
            .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        service
            .write_line("dut-a", WriteIdentity::gui(), "probe-while-disabled")
            .unwrap();
        let mut seen = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            seen = service.read_log("sess-dut-a", 0, usize::MAX).iter().any(
                |e| {
                    matches!(&e.event,
                        UiEvent::SerialChunk { text, .. } if text.contains("[TX] probe-while-disabled"))
                },
            );
            if seen {
                break;
            }
        }
        assert!(
            seen,
            "disabling file logging must not affect bus echo delivery"
        );
        service.flush_session_logs("dut-a");
        let log = std::fs::read(service.logs_dir().join("sess-dut-a.log")).unwrap();
        let bin = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        assert!(
            log.is_empty(),
            "session opened with logging disabled must write nothing to .log: {log:?}"
        );
        assert!(
            bin.is_empty(),
            "session opened with logging disabled must write nothing to .bin: {bin:?}"
        );
        // Re-enabling logging applies to the existing DUT and to newly opened sessions.
        service.set_session_logging(true);
        service
            .open_session("dut-b", "COM5", 115_200, SessionKind::Demo)
            .unwrap();
        write_until_on_disk(&service, "dut-b", "probe-after-enable", "Executed (demo)");
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-b.log")).unwrap();
        assert!(
            text.contains("[TX] probe-after-enable"),
            "new session must persist according to re-enabled flag: {text:?}"
        );
        service.close_session("dut-a").ok();
        service.close_session("dut-b").ok();
    }

    /// Check the configured history and ensure it does not enlarge live frames.
    #[test]
    fn terminal_buffer_clamps_to_minimum_100() {
        let service = test_service("buf-clamp");
        assert_eq!(
            service.terminal_buffer(),
            (
                crate::term::SCROLLBACK_HISTORY,
                crate::term::SNAPSHOT_MAX_ROWS
            ),
            "default preserves 1,000,000 history rows and 400 live snapshot rows"
        );
        service.set_terminal_buffer(50, 50);
        assert_eq!(
            service.terminal_buffer(),
            (100, 100),
            "values below minimum must clamp to 100"
        );
        service.set_terminal_buffer(500, 300);
        assert_eq!(
            service.terminal_buffer(),
            (500, 300),
            "explicit valid values must apply verbatim"
        );
        service.configure_scrollback(Some(2_000_000));
        assert_eq!(
            service.terminal_buffer(),
            (2_000_000, crate::term::LIVE_SNAPSHOT_ROW_CAP)
        );
    }

    /// Garble detection defaults to off and can be toggled per DUT.
    /// The shared map stores enabled entries and is reused by new sessions.
    #[test]
    fn garbled_detection_switch_is_per_dut() {
        let service = test_service("garble-switch");
        assert!(!service.garbled_detection_enabled("dut-a"));
        assert!(!service.garbled_detection_enabled("dut-b"));

        service.set_garbled_detection_for("dut-a", true);
        assert!(service.garbled_detection_enabled("dut-a"));
        assert!(
            !service.garbled_detection_enabled("dut-b"),
            "must not leak across DUTs"
        );

        service.set_garbled_detection_for("dut-a", false);
        assert!(!service.garbled_detection_enabled("dut-a"));

        // Idempotent writes stay consistent.
        service.set_garbled_detection_for("dut-b", true);
        service.set_garbled_detection_for("dut-b", true);
        assert!(service.garbled_detection_enabled("dut-b"));
    }
}
