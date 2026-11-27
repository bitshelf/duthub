//! Serial session commands, shared logging, and the demo driver.
//!
//! The demo replays normal boot and kernel panic fixtures.
//! It then starts an interactive simulated shell for testing without hardware.
//! The hardware driver uses the same command channel with a real serial port
//! and renders the device's own echo through the terminal emulator.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use tokio::sync::mpsc;

use std::time::Duration;

use tokio::sync::Notify;

use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::ipc::{ChunkStyle, EventEnvelope, UiEvent};
use duthub_core::lease::{LeaseAuthority, WriteCtx};
use duthub_core::{DeviceError, DeviceResult};

use crate::bus::EventBus;
use crate::logs::SessionLogWriter;
use crate::rec::RecKind;
use crate::session::LineEnding;

/// Maximum number of bytes merged into one keyboard write.
/// The limit covers typical keystroke bursts without delaying other commands.
/// Keep the batching helper available when the hardware feature is disabled.
#[cfg_attr(not(feature = "real-hardware"), allow(dead_code))]
pub(crate) const TX_MERGE_BYTES: usize = 256;

/// Merge consecutive queued byte writes with the same lease context.
/// Return the first incompatible command for ordered dispatch by the caller.
/// Keep each original command separate in the batch for auditing.
/// The admission check already ran for each command.
/// The actor rechecks the shared lease just before writing the batch.
#[cfg_attr(not(feature = "real-hardware"), allow(dead_code))]
pub(crate) fn absorb_keystrokes(
    cmd_rx: &mut tokio::sync::mpsc::Receiver<SessionCommand>,
    first: Vec<u8>,
    lease: Option<&WriteCtx>,
) -> (Vec<Vec<u8>>, Option<SessionCommand>) {
    let total = |batch: &[Vec<u8>]| batch.iter().map(std::vec::Vec::len).sum::<usize>();
    let mut batch = vec![first];
    let mut deferred = None;
    while total(&batch) < TX_MERGE_BYTES {
        match cmd_rx.try_recv() {
            Ok(SessionCommand::WriteBytes {
                bytes: more,
                lease: more_lease,
            }) if more_lease.as_ref() == lease && total(&batch) + more.len() <= TX_MERGE_BYTES => {
                batch.push(more);
            }
            Ok(other) => {
                deferred = Some(other);
                break;
            }
            Err(_) => break,
        }
    }
    (batch, deferred)
}

/// Commands sent to the task that owns the serial handle.
pub enum SessionCommand {
    /// A line of input with or without an existing line ending.
    /// The lease context comes from write admission and can be absent on permitted unleased paths.
    Write {
        text: String,
        lease: Option<WriteCtx>,
    },
    /// A complete multiline heartbeat probe with its source script for the demo.
    WriteHeartbeatBatch {
        text: String,
        batch: std::sync::Arc<crate::heartbeat::HeartbeatBatch>,
        lease: Option<WriteCtx>,
    },
    /// Send keyboard bytes as supplied, including Enter and Backspace.
    /// The hardware path adds neither a line ending nor local echo.
    WriteBytes {
        bytes: Vec<u8>,
        lease: Option<WriteCtx>,
    },
    /// Change the baud rate. Hardware sessions reopen the port.
    SetBaud(u32),
    /// Resize the terminal grid after a viewport change.
    Resize { cols: usize, rows: usize },
    /// Reset local terminal emulation.
    ResetTerminal,
    ReadTerminalHistory {
        skip: usize,
        reply: tokio::sync::oneshot::Sender<crate::term::TerminalHistoryPage>,
    },
    /// Set the suffix used by subsequent line writes.
    SetEnding(LineEnding),
    /// Write a segment marker to the text log and its corresponding index.
    Mark(String),
    /// Request session shutdown.
    Close,
}

impl std::fmt::Debug for SessionCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Write { text, lease } => f
                .debug_struct("Write")
                .field("text_len", &text.len())
                .field("lease", lease)
                .finish(),
            Self::WriteHeartbeatBatch { text, lease, .. } => f
                .debug_struct("WriteHeartbeatBatch")
                .field("text_len", &text.len())
                .field("lease", lease)
                .finish_non_exhaustive(),
            Self::WriteBytes { bytes, lease } => f
                .debug_struct("WriteBytes")
                .field("bytes_len", &bytes.len())
                .field("lease", lease)
                .finish(),
            Self::SetBaud(baud) => f.debug_tuple("SetBaud").field(baud).finish(),
            Self::Resize { cols, rows } => f
                .debug_struct("Resize")
                .field("cols", cols)
                .field("rows", rows)
                .finish(),
            Self::ResetTerminal => f.write_str("ResetTerminal"),
            Self::ReadTerminalHistory { skip, .. } => f
                .debug_struct("ReadTerminalHistory")
                .field("skip", skip)
                .finish_non_exhaustive(),
            Self::SetEnding(ending) => f.debug_tuple("SetEnding").field(ending).finish(),
            Self::Mark(label) => f.debug_tuple("Mark").field(label).finish(),
            Self::Close => f.write_str("Close"),
        }
    }
}

/// Ordered messages sent to the session log writer.
pub(crate) enum LogMsg {
    /// Record raw receive bytes and their lossy UTF-8 text.
    Raw {
        bytes: Vec<u8>,
        text: String,
    },
    /// Record transmitted bytes and their TX annotation.
    Tx {
        wire: Vec<u8>,
        text: String,
    },
    /// Record keyboard bytes in the binary log only.
    Keystrokes(Vec<u8>),
    /// Append a text annotation.
    Note(String),
    /// Append a segment marker and emit an error annotation if it fails.
    Mark(String),
    /// Return the marker result to a control boundary that must wait for persistence.
    /// Return the result after earlier log messages have drained.
    MarkSync {
        line: String,
        ack: std::sync::mpsc::Sender<DeviceResult<()>>,
    },
    SetBootReference(crate::boot_reference::BootReference),
    SetLogMode {
        split: bool,
        reference: Option<crate::boot_reference::BootReference>,
    },
    /// Acknowledge after all earlier log messages have reached disk.
    Flush(std::sync::mpsc::Sender<()>),
}

/// Complete the write after releasing the log mutex.
enum LogOutcome {
    Ok,
    Failed(DeviceError),
    SyncAck(
        std::sync::mpsc::Sender<DeviceResult<()>>,
        Option<DeviceError>,
    ),
}

/// Write one queued message, reporting storage failures outside the serial event stream.
fn apply_log_msg(
    logs: &Arc<Mutex<SessionLogWriter>>,
    msg: LogMsg,
    mut on_error: impl FnMut(&DeviceError),
) {
    let outcome = {
        let Ok(mut writer) = logs.lock() else {
            return;
        };
        if !matches!(
            &msg,
            LogMsg::Raw { .. } | LogMsg::MarkSync { .. } | LogMsg::SetLogMode { .. }
        ) && let Err(error) = writer.flush_rx_prefix()
        {
            tracing::warn!(%error, "Failed to flush the pending serial log prefix");
        }
        match msg {
            LogMsg::Raw { bytes, text } => writer
                .append_rx(bytes, &text)
                .map_or_else(LogOutcome::Failed, |()| LogOutcome::Ok),
            LogMsg::Tx { wire, text } => {
                let _ = writer.append(&wire, &text);
                let _ = writer.append_rec(RecKind::Tx, &wire);
                LogOutcome::Ok
            }
            LogMsg::Keystrokes(bytes) => {
                let _ = writer.append_raw(&bytes);
                let _ = writer.append_rec(RecKind::Keystrokes, &bytes);
                LogOutcome::Ok
            }
            LogMsg::Note(line) => {
                let _ = writer.note(&line);
                let _ = writer.append_rec(RecKind::Note, line.as_bytes());
                LogOutcome::Ok
            }
            LogMsg::Mark(line) => {
                // Rec first: `mark()` splits afterwards (normal mode), so the
                // mark record lands in the OLD segment — same placement as the
                // `.log` mark line. A rec error never turns a landed mark into
                // LogOutcome::Failed.
                let _ = writer.append_rec(RecKind::Mark, line.as_bytes());
                let result = writer.mark(&line);
                match result {
                    Ok(()) => LogOutcome::Ok,
                    Err(err) => LogOutcome::Failed(err),
                }
            }
            LogMsg::MarkSync { line, ack } => {
                let result = writer.power_mark(&line);
                match result {
                    Ok(()) => LogOutcome::SyncAck(ack, None),
                    Err(err) => LogOutcome::SyncAck(ack, Some(err)),
                }
            }
            LogMsg::SetBootReference(reference) => {
                writer.set_boot_reference(Some(reference));
                LogOutcome::Ok
            }
            LogMsg::SetLogMode { split, reference } => writer
                .set_log_mode(split, reference)
                .map_or_else(LogOutcome::Failed, |()| LogOutcome::Ok),
            LogMsg::Flush(ack) => {
                let _ = ack.send(());
                LogOutcome::Ok
            }
        }
    };
    match outcome {
        LogOutcome::Ok => {}
        LogOutcome::Failed(err) => on_error(&err),
        LogOutcome::SyncAck(ack, error) => {
            if let Some(err) = &error {
                on_error(err);
            }
            let _ = ack.send(match error {
                Some(err) => Err(err),
                None => Ok(()),
            });
        }
    }
}

/// Write and flush each record on a dedicated thread.
/// This keeps disk I/O off the serial echo path.
/// Exit only once the channel is closed and every queued message has been received.
fn run_log_writer(
    logs: &Arc<Mutex<SessionLogWriter>>,
    rx: &std::sync::mpsc::Receiver<LogMsg>,
    session_id: &str,
    dut_id: &str,
    finished: &(Mutex<bool>, Condvar),
) {
    let mut last_error = None;
    let mut on_error = |err: &DeviceError| {
        let error = err.to_string();
        if last_error.as_deref() != Some(error.as_str()) {
            tracing::warn!(%session_id, %dut_id, %error, "Serial log write failed");
            last_error = Some(error);
        }
    };
    while let Ok(msg) = rx.recv() {
        apply_log_msg(logs, msg, &mut on_error);
    }
    let pending_error = logs
        .lock()
        .ok()
        .and_then(|mut writer| writer.flush_rx_prefix().err());
    if let Some(error) = pending_error {
        on_error(&error);
    }
    let (lock, cvar) = finished;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
}

/// Send byte and text records to the writer and publish events to the session buffer and bus.
pub struct SessionCtx {
    pub dut_id: String,
    pub session_id: String,
    bus: Arc<EventBus>,
    logs: Option<Arc<Mutex<SessionLogWriter>>>,
    /// Optional asynchronous channel for receive, transmit, and annotation records.
    log_tx: Option<std::sync::mpsc::Sender<LogMsg>>,
    /// Completion signal used during shutdown after closing the channel.
    log_finished: Option<Arc<(Mutex<bool>, Condvar)>>,
    reconnect_notify: Option<std::sync::Arc<Notify>>,
    /// Optional lease authority for the final write check.
    lease_authority: Option<Arc<dyn LeaseAuthority>>,
    /// Optional audit sink for denied writes.
    audit: Option<Arc<AuditSink>>,
    /// Per-DUT garbled-detection switches shared with `DeviceService`.
    /// `None` = the session never enables detection (demo/tests).
    garble_enabled: Option<Arc<Mutex<HashMap<String, bool>>>>,
    /// Raw-byte garble meter (baud-rate mismatch heuristic; RX path only).
    garble: Mutex<crate::garbled::GarbleMeter>,
}

impl std::fmt::Debug for SessionCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCtx")
            .field("dut_id", &self.dut_id)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl SessionCtx {
    pub async fn begin_actor_io(&self) -> Option<tokio::sync::RwLockReadGuard<'_, ()>> {
        match self.lease_authority.as_ref() {
            Some(authority) => Some(authority.serial_io().read().await),
            None => None,
        }
    }
    pub fn new(dut_id: &str, session_id: &str, bus: Arc<EventBus>) -> Self {
        Self {
            dut_id: dut_id.to_string(),
            session_id: session_id.to_string(),
            bus,
            logs: None,
            log_tx: None,
            log_finished: None,
            reconnect_notify: None,
            lease_authority: None,
            audit: None,
            garble_enabled: None,
            garble: Mutex::new(crate::garbled::GarbleMeter::default()),
        }
    }

    /// Share per-DUT garbled-output settings with the device service.
    /// Runtime updates affect active sessions.
    #[must_use]
    pub fn with_garble(mut self, map: Arc<Mutex<HashMap<String, bool>>>) -> Self {
        self.garble_enabled = Some(map);
        self
    }

    /// Reads the per-DUT switch. The map lock is always released before the
    /// meter lock is taken (single lock order, no nesting).
    fn garble_on(&self) -> bool {
        let Some(map) = &self.garble_enabled else {
            return false;
        };
        match map.lock() {
            Ok(guard) => guard.get(&self.dut_id).copied().unwrap_or(false),
            // A poisoned switch map means a writer panicked; fail closed.
            Err(_) => false,
        }
    }

    fn with_garble_meter<R>(
        &self,
        update: impl FnOnce(&mut crate::garbled::GarbleMeter) -> R,
    ) -> R {
        match self.garble.lock() {
            Ok(mut meter) => update(&mut meter),
            // A poisoned meter must not break the RX path; recover and keep
            // counting instead of panicking on every block.
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    /// Current garble statistics for the next snapshot (also applies a runtime
    /// switch flip: turning detection off resets the meter to `Clean`).
    pub fn garble_stats(&self) -> duthub_core::ipc::GarbledStats {
        let enabled = self.garble_on();
        self.with_garble_meter(|meter| {
            meter.set_enabled(enabled);
            meter.stats()
        })
    }

    /// Clears the detection window (terminal reset / baud-rate change).
    pub fn reset_garble(&self) {
        self.with_garble_meter(crate::garbled::GarbleMeter::reset);
    }

    /// Attach the log writer and start its dedicated thread.
    /// Each message still writes and flushes before processing the next one.
    /// The serial actor can continue rendering echo while the writer performs disk I/O.
    #[must_use]
    pub fn with_logs(mut self, logs: Arc<Mutex<SessionLogWriter>>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<LogMsg>();
        let finished = Arc::new((Mutex::new(false), Condvar::new()));
        let spawned = std::thread::Builder::new()
            .name(format!("serial-log-{}", self.session_id))
            .spawn({
                let logs_for_thread = Arc::clone(&logs);
                let session_id = self.session_id.clone();
                let dut_id = self.dut_id.clone();
                let finished = Arc::clone(&finished);
                move || run_log_writer(&logs_for_thread, &rx, &session_id, &dut_id, &finished)
            });
        if spawned.is_ok() {
            self.log_tx = Some(tx);
            self.log_finished = Some(finished);
        }
        self.logs = Some(logs);
        self
    }

    /// Clone the log sender for synchronous control boundaries.
    pub(crate) fn log_sender(&self) -> Option<std::sync::mpsc::Sender<LogMsg>> {
        self.log_tx.clone()
    }

    /// Queue a log message, or write inline if the thread is unavailable.
    fn enqueue_log(&self, msg: LogMsg) {
        let msg = match &self.log_tx {
            Some(tx) => match tx.send(msg) {
                Ok(()) => return,
                Err(std::sync::mpsc::SendError(undelivered)) => undelivered,
            },
            None => msg,
        };
        if let Some(logs) = &self.logs {
            apply_log_msg(logs, msg, |error| {
                tracing::warn!(
                    session_id = %self.session_id, dut_id = %self.dut_id,
                    %error, "Serial log write failed"
                );
            });
        }
    }

    /// Wait until all earlier log messages have reached disk.
    pub fn flush_logs(&self) {
        if let Some(tx) = &self.log_tx {
            let (ack, rx) = std::sync::mpsc::channel();
            if tx.send(LogMsg::Flush(ack)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_secs(3));
            }
        }
    }

    /// Attach the signal that wakes a reconnect backoff.
    #[must_use]
    pub fn with_reconnect_notify(mut self, notify: std::sync::Arc<Notify>) -> Self {
        self.reconnect_notify = Some(notify);
        self
    }

    /// Attach the authority that checks a lease immediately before a serial write.
    #[must_use]
    pub fn with_lease_authority(mut self, authority: Option<Arc<dyn LeaseAuthority>>) -> Self {
        self.lease_authority = authority;
        self
    }

    /// Attach the audit sink.
    #[must_use]
    pub fn with_audit(mut self, audit: Option<Arc<AuditSink>>) -> Self {
        self.audit = audit;
        self
    }

    /// Recheck any command lease immediately before the serial write.
    /// On failure, return false and record an error annotation and denial.
    pub fn admit_actor_write(&self, lease: &Option<WriteCtx>) -> bool {
        let Some(ctx) = lease else {
            return true; // No lease required (legacy tests / transitional path).
        };
        let Some(authority) = &self.lease_authority else {
            return true;
        };
        match authority.validate_ctx(&self.dut_id, ctx) {
            Ok(()) => true,
            Err(err) => {
                let note = format!("[lease] 过期写入已拦截：{}", err.message());
                self.emit_text(&note, ChunkStyle::Error);
                if let Some(audit) = &self.audit {
                    audit.record(
                        "write.deny",
                        None,
                        None,
                        Some(&self.dut_id),
                        AuditOutcome::Denied,
                        Some(&format!(
                            "actor 复核拒绝 lease={} gen={}：{}",
                            ctx.lease_id,
                            ctx.generation,
                            err.message()
                        )),
                    );
                }
                false
            }
        }
    }

    fn write_log(&self, line: &str) {
        self.enqueue_log(LogMsg::Note(line.to_string()));
    }

    pub fn emit_text(&self, text: &str, style: ChunkStyle) -> EventEnvelope {
        self.write_log(text);
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(text, style),
        )
    }

    pub fn emit_disconnected(&self, reason: &str) -> EventEnvelope {
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            UiEvent::SerialDisconnected {
                reason: reason.to_string(),
            },
        )
    }

    /// The demo echoes transmitted text into the terminal stream.
    /// The binary log stores the wire bytes and the text log stores the TX annotation.
    pub fn emit_tx(&self, wire_payload: &str, display_text: &str) {
        let display = single_line_tx(display_text);
        self.enqueue_log(LogMsg::Tx {
            wire: wire_payload.as_bytes().to_vec(),
            text: format!("[TX] {display}"),
        });
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(format!("[TX] {display}"), ChunkStyle::Good),
        );
    }

    /// Real line writes store wire bytes and a TX annotation in the logs.
    /// They do not emit local terminal echo or a transmit event.
    /// The emulator renders the target shell's own output.
    pub fn log_tx(&self, wire_payload: &[u8], display_text: &str) {
        let display = single_line_tx(display_text);
        self.enqueue_log(LogMsg::Tx {
            wire: wire_payload.to_vec(),
            text: format!("[TX] {display}"),
        });
    }

    /// Log the raw receive bytes and their lossy UTF-8 representation.
    /// Publish a text chunk for MCP reads, recording, and audit.
    /// The terminal itself displays the separate grid snapshot.
    pub fn emit_raw(&self, bytes: &[u8]) {
        // Garble detection samples the raw bytes before any lossy decoding and
        // before the terminal emulator (which swallows most control bytes).
        // Meter state only: no disk I/O and no extra bus event here.
        let garble_on = self.garble_on();
        self.with_garble_meter(|meter| {
            meter.set_enabled(garble_on);
            meter.observe(bytes);
        });
        let text = String::from_utf8_lossy(bytes).to_string();
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(text.clone(), ChunkStyle::Normal),
        );
        self.enqueue_log(LogMsg::Raw {
            bytes: bytes.to_vec(),
            text,
        });
    }

    /// For real keyboard writes, record only the original bytes.
    /// The device echo supplies subsequent text.
    pub fn log_keystrokes(&self, bytes: &[u8]) {
        self.enqueue_log(LogMsg::Keystrokes(bytes.to_vec()));
    }

    /// Publish a new hardware terminal snapshot for complete viewport replacement.
    /// Consume the snapshot to avoid a deep copy.
    /// The garble statistics are stamped here so every snapshot carries the
    /// latest raw-byte verdict without changing the `term` snapshot builder.
    pub fn emit_screen(&self, mut screen: duthub_core::ipc::ScreenSnapshot) {
        screen.garbled = self.garble_stats();
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            UiEvent::SerialScreen {
                screen: std::sync::Arc::new(screen),
            },
        );
    }

    /// Queue a storage-only log boundary.
    pub fn emit_mark(&self, label: &str) {
        self.enqueue_log(LogMsg::Mark(format!("[mark] {label}")));
    }

    /// Wait for reconnect backoff or a manual reconnect notification.
    pub async fn reconnect_wait(&self, backoff: Duration) {
        match &self.reconnect_notify {
            Some(notify) => {
                let notified = notify.notified();
                tokio::pin!(notified);
                tokio::select! {
                    () = &mut notified => {}
                    () = tokio::time::sleep(backoff) => {}
                }
            }
            None => tokio::time::sleep(backoff).await,
        }
    }
}

impl Drop for SessionCtx {
    fn drop(&mut self) {
        // Close the log sender and let the thread drain before it exits.
        // This preserves the queued bytes and text when the session closes.
        self.log_tx = None;
        if let Some(finished) = &self.log_finished {
            let (lock, cvar) = &**finished;
            let guard = lock.lock().unwrap();
            let _ = cvar
                .wait_timeout_while(guard, Duration::from_secs(3), |done| !*done)
                .unwrap();
        }
    }
}

/// Escape CR and LF as text in TX annotations.
/// This keeps each annotation on one log line.
/// Readers cannot mistake continuations for device output.
/// The binary log still preserves the transmitted bytes exactly.
fn single_line_tx(text: &str) -> String {
    text.replace('\r', "\\r").replace('\n', "\\n")
}

/// Highlight kernel errors and prompts and render other lines normally.
#[must_use]
pub fn classify_line(text: &str) -> ChunkStyle {
    let lower = text.to_ascii_lowercase();
    if lower.contains("panic")
        || lower.contains("oops")
        || lower.contains("bug:")
        || lower.contains("call trace")
        || lower.contains("error")
        || lower.contains("failed")
    {
        return ChunkStyle::Error;
    }
    let trimmed_end = text.trim_end();
    if trimmed_end.ends_with('#') || trimmed_end.ends_with('$') || trimmed_end.ends_with("login:") {
        return ChunkStyle::Prompt;
    }
    ChunkStyle::Normal
}

// The hardware driver feeds bytes directly into the terminal emulator.
// The target shell supplies echo, prompts, and cursor movement.
// The text log receives the same read chunks without reconstructing lines.

// ---------------------------------------------------------------------------
// Demo driver
// ---------------------------------------------------------------------------

const BOOT_FIXTURE: &str = include_str!("../../../tests/fixture-logs/rk3576-boot.log");
const PANIC_FIXTURE: &str = include_str!("../../../tests/fixture-logs/kernel-panic.log");

/// Replay data for demo boot and simulated kernel panic.
pub fn boot_fixture() -> Vec<String> {
    BOOT_FIXTURE.lines().map(str::to_string).collect()
}

pub fn panic_fixture() -> Vec<String> {
    PANIC_FIXTURE.lines().map(str::to_string).collect()
}

/// Demo driver: replays boot logs, runs an interactive shell, and exits on `Close` or cancellation.
pub async fn run_demo_session(
    ctx: SessionCtx,
    mut cmd_rx: mpsc::Receiver<SessionCommand>,
    line_delay_ms: u64,
) {
    let mut ending = LineEnding::default();
    for line in boot_fixture() {
        ctx.emit_text(&line, classify_line(&line));
        tokio::time::sleep(std::time::Duration::from_millis(line_delay_ms)).await;
    }
    ctx.emit_text("RK3576 login: root", ChunkStyle::Normal);
    ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCommand::Write { text, lease } => {
                // Actor-side `[E]` recheck: verify lease generation immediately before write;
                // if stale or taken over, drop the write (note + audit) without touching serial.
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                // TX echo matches the real driver: Good style + `[TX]` prefix (the demo shell
                // prompt line below simulates device-side local echo).
                ctx.emit_tx(&ending.apply(&text), &text);
                let command = text.trim();
                if command.is_empty() {
                    ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
                    continue;
                }
                ctx.emit_text(&format!("root@rk3576:~# {command}"), ChunkStyle::Prompt);
                emit_demo_response(&ctx, command);
                ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
            }
            SessionCommand::WriteHeartbeatBatch { text, batch, lease } => {
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                ctx.emit_tx(&ending.apply(&text), &text);
                ctx.emit_text(&batch.begin_marker(), ChunkStyle::Normal);
                for command in batch
                    .script
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                {
                    emit_demo_response(&ctx, command);
                }
                ctx.emit_text(&batch.end_marker(), ChunkStyle::Normal);
            }
            SessionCommand::SetBaud(_) => {
                ctx.emit_text("(demo) 波特率变更已模拟", ChunkStyle::Dim);
            }
            SessionCommand::Mark(label) => {
                ctx.emit_mark(&label);
            }
            SessionCommand::SetEnding(new_ending) => {
                ending = new_ending;
                ctx.emit_text(
                    &format!("(demo) 行尾策略 → {}", new_ending.label()),
                    ChunkStyle::Dim,
                );
            }
            SessionCommand::WriteBytes { bytes, lease } => {
                // Actor-side `[E]` recheck (same as `Write`).
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                // Demo has no emulator: echo keystroke bytes via lossy UTF-8.
                let command = String::from_utf8_lossy(&bytes).trim().to_string();
                if !command.is_empty() {
                    ctx.emit_text(&format!("Executed (demo): {command}"), ChunkStyle::Normal);
                }
                ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
            }
            SessionCommand::Resize { .. }
            | SessionCommand::ResetTerminal
            | SessionCommand::ReadTerminalHistory { .. } => {
                // Demo has no terminal grid; ignore.
            }
            SessionCommand::Close => break,
        }
    }
    ctx.emit_disconnected("session closed");
}

fn emit_demo_response(ctx: &SessionCtx, command: &str) {
    match command {
        "pwd" => {
            ctx.emit_text("/root", ChunkStyle::Normal);
        }
        "whoami" => {
            ctx.emit_text("root", ChunkStyle::Normal);
        }
        "uname -a" => {
            ctx.emit_text(
                "Linux rk3576 6.1.118 #1 SMP PREEMPT aarch64 GNU/Linux",
                ChunkStyle::Normal,
            );
        }
        "ls" => {
            ctx.emit_text(
                "bin  boot  dev  etc  home  lib  media  mnt  opt  proc  root  run  sys  tmp  usr  var",
                ChunkStyle::Normal,
            );
        }
        "uptime" => {
            ctx.emit_text(
                " 00:12:40 up 4 days,  2 users,  load average: 0.05, 0.07, 0.06",
                ChunkStyle::Normal,
            );
        }
        c if c.starts_with("dmesg") => {
            for line in panic_fixture() {
                ctx.emit_text(&line, classify_line(&line));
            }
        }
        "clear" => {
            // The UI handles `clear` locally; the demo driver only emits a notice.
            ctx.emit_text("(demo) 屏幕已清空（本地终端处理 clear）", ChunkStyle::Dim);
        }
        "help" => {
            ctx.emit_text(
                "Demo commands: uname -a, whoami, pwd, ls, uptime, dmesg, clear",
                ChunkStyle::Dim,
            );
        }
        "reboot" => {
            ctx.emit_text("Reboot requested (simulation only)", ChunkStyle::Normal);
        }
        other => {
            ctx.emit_text(&format!("Executed (demo): {other}"), ChunkStyle::Normal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_flags_kernel_errors() {
        assert_eq!(
            classify_line("Kernel panic - not syncing: Fatal"),
            ChunkStyle::Error
        );
        assert_eq!(classify_line("[   42.90]Oops: 96000047"), ChunkStyle::Error);
        assert_eq!(
            classify_line("[   3.512] systemd[1]: Started Serial Getty."),
            ChunkStyle::Normal
        );
        assert_eq!(classify_line("root@rk3576:~# "), ChunkStyle::Prompt);
    }

    #[tokio::test]
    async fn demo_session_replays_boot_and_answers_uname() {
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let ctx = SessionCtx::new("dut-demo", "sess-demo", bus.clone());
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        // Wait for boot log replay to finish.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        tx.send(SessionCommand::Write {
            text: "uname -a\n".into(),
            lease: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let log: Vec<String> = bus
            .read_log("sess-demo", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let boot_len = boot_fixture().len();
        assert!(
            log.len() >= boot_len + 3,
            "expected boot lines + input echo + response + prompt"
        );
        assert!(log.iter().any(|l| l.contains("aarch64 GNU/Linux")));
        assert!(log.iter().any(|l| l.ends_with("root@rk3576:~# uname -a")));
        drop(tx);
        let _ = handle.await;
        // Final event after close is Disconnected, and sequence stops advancing.
        let after_close = bus.last_seq("sess-demo");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            after_close,
            bus.last_seq("sess-demo"),
            "no further events should arrive after exit"
        );
    }

    #[tokio::test]
    async fn write_produces_tx_echo_and_setending_takes_effect() {
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let ctx = SessionCtx::new("dut-tx", "sess-tx", bus.clone());
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        tx.send(SessionCommand::Write {
            text: "echo-a".into(),
            lease: None,
        })
        .await
        .unwrap();
        tx.send(SessionCommand::SetEnding(LineEnding::Crlf))
            .await
            .unwrap();
        tx.send(SessionCommand::Write {
            text: "echo-b".into(),
            lease: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let lines: Vec<String> = bus
            .read_log("sess-tx", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|l| l == "[TX] echo-a"),
            "write must produce TX echo line; got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "[TX] echo-b"),
            "write after SetEnding must still produce TX echo"
        );
        drop(tx);
        let _ = handle.await;
    }

    /// Actor-side `[E]` recheck: a command carrying a stale or unknown lease context
    /// is dropped before serial I/O with an error note and never emits a TX echo (INV-04).
    #[tokio::test]
    async fn actor_recheck_drops_stale_generation_write() {
        use duthub_core::audit::AuditSink;
        use duthub_core::lease::{ControlLeaseManager, WriteCtx};
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let authority: Arc<dyn LeaseAuthority> =
            Arc::new(ControlLeaseManager::new(Some(AuditSink::memory())));
        let audit = AuditSink::memory();
        let ctx = SessionCtx::new("dut-e", "sess-e", bus.clone())
            .with_lease_authority(Some(authority.clone()))
            .with_audit(Some(audit.clone()));
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        // No holder owns this lease in the manager: validation rejects and drops it.
        let stale = Some(WriteCtx {
            lease_id: "lease-stale".into(),
            generation: 7,
        });
        tx.send(SessionCommand::Write {
            text: "stale-cmd".into(),
            lease: stale,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let lines: Vec<String> = bus
            .read_log("sess-e", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|l| l.contains("过期写入已拦截")),
            "expected stale-write interception note: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l == "[TX] stale-cmd"),
            "stale write must not touch serial or emit TX echo: {lines:?}"
        );
        // Audit `write.deny`.
        let denied = audit
            .recent(50)
            .iter()
            .any(|e| e.action == "write.deny" && e.dut.as_deref() == Some("dut-e"));
        assert!(denied, "actor denial must be recorded in audit");
        drop(tx);
        let _ = handle.await;
    }

    #[test]
    fn fixtures_contain_panic_evidence() {
        let panic = panic_fixture().join("\n");
        assert!(panic.contains("Kernel panic - not syncing"));
        assert!(panic.contains("Call trace:"));
        assert!(
            boot_fixture()
                .iter()
                .any(|l| l.contains("Started Serial Getty"))
        );
    }

    /// Single-line `[TX]` annotations: both `emit_tx` (terminal bus) and `log_tx`
    /// (`.log` audit stream) escape newlines in `display_text` as literal `\n`/`\r`
    /// with no unprefixed continuation lines, while `.bin` retains exact wire bytes.
    #[test]
    fn tx_annotation_is_single_line_for_multiline_text() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/tx-single-line")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-txml").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-txml", "sess-txml", bus.clone()).with_logs(logs);
        let expected = "[TX] echo C1\\necho C2";
        // `emit_tx`: terminal bus stream must stay on one line.
        ctx.emit_tx("echo C1\necho C2\n", "echo C1\necho C2");
        let chunks: Vec<String> = bus
            .read_log("sess-txml", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            chunks.iter().any(|t| t == expected),
            "[TX] bus annotation must be single-line with escaped newlines: {chunks:?}"
        );
        // `log_tx`: `.log` audit stream must also stay on one line.
        ctx.log_tx(b"echo C1\recho C2\r\n", "echo C1\r\necho C2");
        ctx.flush_logs();
        let text = std::fs::read_to_string(dir.join("sess-txml.log")).unwrap();
        assert!(
            text.lines().any(|line| line == "[TX] echo C1\\r\\necho C2"),
            ".log must contain single-line [TX] annotation: {text:?}"
        );
        assert!(
            !text.lines().any(|line| line == "echo C2"),
            ".log must not contain unprefixed [TX] continuation lines: {text:?}"
        );
        // `.bin` preserves exact wire bytes without escaping.
        let raw = std::fs::read(dir.join("sess-txml.bin")).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&raw),
            "echo C1\necho C2\necho C1\recho C2\r\n"
        );
    }

    /// Log writer persists records in enqueue order (lossless guarantee): after
    /// async queueing of RX/TX/keystrokes/notes/marks, `.log` and `.bin` preserve call order.
    #[test]
    fn log_writer_preserves_enqueue_order_and_drains() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log-order")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-logorder").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-lo", "sess-logorder", bus.clone()).with_logs(logs);
        ctx.emit_raw(b"rx-1");
        ctx.log_tx(b"tx-1", "tx-1");
        ctx.log_keystrokes(b"key-1");
        ctx.emit_text("[note] hello", ChunkStyle::Dim);
        ctx.emit_mark("seg-a");
        ctx.emit_raw(b"rx-2");
        ctx.flush_logs();

        let text = std::fs::read_to_string(dir.join("sess-logorder.log")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let pos = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("The log is missing {needle}: {text:?}"))
        };
        assert_eq!(pos("rx-1"), 0, "RX text must appear first");
        assert!(pos("[TX] tx-1") > pos("rx-1"));
        assert!(pos("[note] hello") > pos("[TX] tx-1"));
        assert!(pos("[mark] seg-a") > pos("[note] hello"));
        // Mark splits subsequent output into a new segment file: `rx-2` must appear in a segment.
        let seg_files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                let name = p.to_string_lossy();
                name.contains("sess-logorder") && name.contains(".log")
            })
            .collect();
        assert!(
            seg_files.iter().any(|p| {
                std::fs::read_to_string(p)
                    .unwrap_or_default()
                    .contains("rx-2")
            }),
            "RX after mark must land in the new segment: {seg_files:?}"
        );
        let raw = std::fs::read(dir.join("sess-logorder.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        assert!(raw.contains("rx-1tx-1key-1"), "byte order: {raw:?}");
    }

    /// Drop drain: dropping `SessionCtx` without calling `flush_logs` must still
    /// drain all pending messages on the writer thread before exit (zero byte loss).
    #[test]
    fn ctx_drop_drains_pending_log_messages() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log-drop")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-logdrop").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-ld", "sess-logdrop", bus).with_logs(logs);
        for i in 0..64 {
            ctx.emit_raw(format!("drop-{i}").as_bytes());
        }
        drop(ctx); // Drop directly without flushing.
        let text = std::fs::read_to_string(dir.join("sess-logdrop.log")).unwrap();
        assert!(
            text.contains("drop-63"),
            "last record must be persisted: {text:?}"
        );
        assert!(
            text.contains("drop-0"),
            "first record must be persisted: {text:?}"
        );
    }

    // ---- TX batching (`absorb_keystrokes`) ----

    fn push_writes(
        tx: &tokio::sync::mpsc::Sender<SessionCommand>,
        lease: Option<&WriteCtx>,
        payloads: &[&[u8]],
    ) {
        for p in payloads {
            tx.try_send(SessionCommand::WriteBytes {
                bytes: p.to_vec(),
                lease: lease.cloned(),
            })
            .unwrap();
        }
    }

    /// Consecutive same-lease keystrokes merge into one batch in enqueue order while
    /// keeping each original command as a separate element for per-command audit logging.
    #[test]
    fn consecutive_same_lease_writes_merge_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-1".into(),
            generation: 3,
        });
        push_writes(&tx, lease.as_ref(), &[b"a", b"bc", b"def"]);
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"x".to_vec(), lease.as_ref());
        assert!(deferred.is_none(), "no foreign command should remain");
        assert_eq!(
            batch.concat(),
            b"xabcdef".to_vec(),
            "byte order must be preserved"
        );
        assert_eq!(batch.len(), 4, "original command boundaries preserved");
    }

    /// Foreign-lease keystrokes are never merged; non-keystroke commands immediately truncate and return.
    #[test]
    fn foreign_lease_and_other_commands_are_deferred() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let gui = Some(WriteCtx {
            lease_id: "gui-1".into(),
            generation: 1,
        });
        let agent = Some(WriteCtx {
            lease_id: "agent-9".into(),
            generation: 7,
        });
        push_writes(&tx, agent.as_ref(), &[b"agent"]);
        tx.try_send(SessionCommand::Resize { cols: 80, rows: 24 })
            .unwrap();
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"gui".to_vec(), gui.as_ref());
        assert_eq!(
            batch,
            vec![b"gui".to_vec()],
            "foreign lease must not be merged"
        );
        let Some(SessionCommand::WriteBytes { bytes, lease }) = deferred else {
            panic!("Return input with a different lease to the queue");
        };
        assert_eq!(bytes, b"agent".to_vec());
        assert_eq!(lease, agent);
        // Absorb again: `Resize` must be returned untouched.
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"z".to_vec(), agent.as_ref());
        assert_eq!(
            batch,
            vec![b"z".to_vec()],
            "same lease with no more queued keystrokes"
        );
        let Some(SessionCommand::Resize { cols, rows }) = deferred else {
            panic!("Return the Resize command unchanged");
        };
        assert_eq!((cols, rows), (80, 24));
    }

    /// Truncate at the merge byte cap: oversized keystrokes are deferred whole without splitting.
    #[test]
    fn merge_stops_at_byte_cap() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-1".into(),
            generation: 2,
        });
        let big = vec![b'k'; 200];
        let big2 = vec![b'm'; 100];
        tx.try_send(SessionCommand::WriteBytes {
            bytes: big2.clone(),
            lease: lease.clone(),
        })
        .unwrap();
        let (batch, deferred) = absorb_keystrokes(&mut rx, big, lease.as_ref());
        assert_eq!(batch.iter().map(std::vec::Vec::len).sum::<usize>(), 200);
        let Some(SessionCommand::WriteBytes { bytes, lease: l }) = deferred else {
            panic!("Leave the entire oversized input for the next batch");
        };
        assert_eq!(bytes, big2, "oversized keystroke must not be split");
        assert_eq!(l, lease);
    }

    /// Unleased path (`lease = None`) with an empty queue returns only the initial payload.
    #[test]
    fn empty_queue_returns_first_only() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        drop(tx);
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"only".to_vec(), None);
        assert_eq!(batch, vec![b"only".to_vec()]);
        assert!(deferred.is_none());
    }

    // ---- TX batching boundaries (PERF-1: behavior on both sides of cap) ----

    /// PERF-1 boundary 1: 100 + 156 = 256 hits the exact cap (`<=` absorbs it),
    /// then the loop exits without pulling the next same-lease keystroke from the queue.
    #[test]
    fn merge_accumulates_to_exact_cap_and_leaves_next_queued() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-cap".into(),
            generation: 1,
        });
        let hundred = vec![b'a'; 100];
        let one_fiftysix = vec![b'b'; 156];
        let ten = vec![b'c'; 10];
        push_writes(
            &tx,
            lease.as_ref(),
            &[one_fiftysix.as_slice(), ten.as_slice()],
        );
        let (batch, deferred) = absorb_keystrokes(&mut rx, hundred, lease.as_ref());
        assert_eq!(
            batch.len(),
            2,
            "command reaching exact cap must be included"
        );
        assert_eq!(
            batch.iter().map(std::vec::Vec::len).sum::<usize>(),
            TX_MERGE_BYTES,
            "merged size must equal exact cap"
        );
        assert!(
            deferred.is_none(),
            "exiting at exact cap must not pull the next command"
        );
        let SessionCommand::WriteBytes { bytes, lease: l } = rx.try_recv().unwrap() else {
            panic!("Expected the remaining command to be keyboard input");
        };
        assert_eq!(
            bytes, ten,
            "overflowing keystroke remains whole in the queue"
        );
        assert_eq!(l, lease);
        assert!(rx.try_recv().is_err(), "queue should now be empty");
    }

    /// PERF-1 boundary 2: when the initial keystroke already equals the cap, nothing
    /// else is absorbed and the next same-lease command stays queued untouched.
    #[test]
    fn first_command_at_cap_merges_nothing() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-cap2".into(),
            generation: 1,
        });
        let at_cap = vec![b'x'; TX_MERGE_BYTES];
        let small = vec![b'y'; 5];
        push_writes(&tx, lease.as_ref(), &[small.as_slice()]);
        let (batch, deferred) = absorb_keystrokes(&mut rx, at_cap.clone(), lease.as_ref());
        assert_eq!(
            batch,
            vec![at_cap],
            "at cap: batch holds only the first command without splitting"
        );
        assert!(deferred.is_none());
        let SessionCommand::WriteBytes { bytes, lease: l } = rx.try_recv().unwrap() else {
            panic!("Leave the entire same-lease command for the next batch");
        };
        assert_eq!(bytes, small);
        assert_eq!(l, lease);
    }

    /// PERF-1 sustained burst: 10 x 60 B same-lease commands (600 B > 256 B cap)
    /// absorb in 3 rounds (4 + 4 + 2), each <= cap with every command intact and
    /// globally ordered without drops or reordering.
    #[test]
    fn sustained_burst_merges_in_order_without_splitting_commands() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-burst".into(),
            generation: 4,
        });
        let cmds: Vec<Vec<u8>> = (0..10u8).map(|i| vec![b'0' + i; 60]).collect();
        let rest: Vec<&[u8]> = cmds[1..].iter().map(std::vec::Vec::as_slice).collect();
        push_writes(&tx, lease.as_ref(), &rest);
        let (b1, d1) = absorb_keystrokes(&mut rx, cmds[0].clone(), lease.as_ref());
        let Some(SessionCommand::WriteBytes { bytes, lease: l }) = d1 else {
            panic!("Return the entire fifth command to the queue");
        };
        assert_eq!(bytes, cmds[4]);
        assert_eq!(l, lease);
        let (b2, d2) = absorb_keystrokes(&mut rx, bytes, lease.as_ref());
        let Some(SessionCommand::WriteBytes { bytes, lease: l }) = d2 else {
            panic!("Return the entire ninth command to the queue");
        };
        assert_eq!(bytes, cmds[8]);
        assert_eq!(l, lease);
        let (b3, d3) = absorb_keystrokes(&mut rx, bytes, lease.as_ref());
        assert!(d3.is_none(), "entire burst should be consumed");
        for batch in [&b1, &b2, &b3] {
            assert!(
                batch.iter().map(std::vec::Vec::len).sum::<usize>() <= TX_MERGE_BYTES,
                "batch size must not exceed merge cap"
            );
        }
        assert_eq!(b1.len(), 4);
        assert_eq!(b2.len(), 4);
        assert_eq!(b3.len(), 2);
        let merged: Vec<&Vec<u8>> = b1.iter().chain(b2.iter()).chain(b3.iter()).collect();
        let expected: Vec<&Vec<u8>> = cmds.iter().collect();
        assert_eq!(
            merged, expected,
            "cross-round batching must not reorder or split commands"
        );
    }

    /// PERF-1 interleaved lease truncation: an agent write inside a GUI burst truncates
    /// absorption immediately without skipping ahead to later GUI commands, preserving global order.
    #[test]
    fn interleaved_lease_burst_truncates_at_foreign_command() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let gui = Some(WriteCtx {
            lease_id: "gui".into(),
            generation: 1,
        });
        let agent = Some(WriteCtx {
            lease_id: "agent".into(),
            generation: 2,
        });
        let c0 = vec![b'a'; 50];
        let c1 = vec![b'b'; 50];
        let c2 = vec![b'c'; 50];
        let c3 = vec![b'd'; 50];
        push_writes(&tx, agent.as_ref(), &[c1.as_slice()]);
        push_writes(&tx, gui.as_ref(), &[c2.as_slice(), c3.as_slice()]);
        // Round 1: `c0` (gui) followed by foreign `c1` truncates immediately and returns `c1`.
        let (b1, d1) = absorb_keystrokes(&mut rx, c0.clone(), gui.as_ref());
        assert_eq!(
            b1,
            vec![c0.clone()],
            "batch before foreign lease must contain only the first command"
        );
        let Some(SessionCommand::WriteBytes {
            bytes: deferred1,
            lease: l1,
        }) = d1
        else {
            panic!("Return input with a different lease unchanged");
        };
        assert_eq!(deferred1, c1);
        assert_eq!(l1, agent);
        // Round 2: deferred `c1` (agent) followed by gui `c2` truncates again, proving `c2` stayed before `c3`.
        let (b2, d2) = absorb_keystrokes(&mut rx, deferred1, agent.as_ref());
        assert_eq!(b2, vec![c1.clone()]);
        let Some(SessionCommand::WriteBytes {
            bytes: deferred2,
            lease: l2,
        }) = d2
        else {
            panic!("Return input with a different lease unchanged");
        };
        assert_eq!(deferred2, c2);
        assert_eq!(l2, gui);
        // Round 3: `c2` and `c3` share the GUI lease and merge cleanly.
        let (b3, d3) = absorb_keystrokes(&mut rx, deferred2, gui.as_ref());
        assert!(d3.is_none());
        assert_eq!(b3, vec![c2.clone(), c3.clone()]);
    }

    // ---- RX log pipeline (PERF-2: synchronous bus publish + disk I/O only on writer thread) ----

    /// PERF-2: `emit_raw` synchronously publishes `SerialChunk` to the bus (preserving
    /// MCP `serial_read` / audit visibility) with zero disk I/O on the caller thread.
    /// Holding the log mutex blocks the writer thread deterministically so `.log`/`.bin`
    /// stay empty until released, then drain in order on `flush_logs`.
    #[test]
    fn emit_raw_publishes_to_bus_synchronously_and_defers_disk_to_writer() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/perf2-bus-sync")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-busio").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-busio", "sess-busio", bus.clone()).with_logs(logs.clone());
        // 1) Hold the log mutex so the writer thread blocks on lock before writing to disk.
        let guard = logs.lock().unwrap();
        ctx.emit_raw(b"rx-bus-1");
        ctx.emit_raw(b"rx-bus-2");
        ctx.log_tx(b"tx-bus", "tx-bus");
        // 2) `emit_raw` returns with chunks already visible on the bus synchronously.
        let chunks: Vec<String> = bus
            .read_log("sess-busio", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            chunks,
            vec!["rx-bus-1".to_string(), "rx-bus-2".to_string()],
            "RX chunks must be published to the bus synchronously"
        );
        // 3) While the writer thread is blocked, both log files stay empty (no caller-thread disk I/O).
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-busio.log")).unwrap(),
            "",
            "zero caller-thread disk I/O: .log must stay empty while writer is blocked"
        );
        assert!(
            std::fs::read(dir.join("sess-busio.bin"))
                .unwrap()
                .is_empty(),
            "zero caller-thread disk I/O: .bin must stay empty while writer is blocked"
        );
        drop(guard);
        // 4) Flush barrier ensures all queued messages are written and flushed in order.
        ctx.flush_logs();
        let text = std::fs::read_to_string(dir.join("sess-busio.log")).unwrap();
        assert_eq!(
            text, "rx-bus-1\nrx-bus-2\n[TX] tx-bus\n",
            "writer thread must flush queued records in order after unlock"
        );
        assert_eq!(
            String::from_utf8_lossy(&std::fs::read(dir.join("sess-busio.bin")).unwrap()),
            "rx-bus-1rx-bus-2tx-bus",
            ".bin byte order must match enqueue order"
        );
    }

    // ---- Session shutdown drain (LOG-6: Drop drain + byte-exact preservation) ----

    /// LOG-6: dropping `SessionCtx` without flushing drains every queued record on the
    /// writer thread via condvar wait; `.bin` matches the exact byte concatenation of
    /// RX/TX/keystrokes/notes (including multibyte UTF-8, `\r\n`, and `0x7f`).
    #[test]
    fn ctx_drop_drains_mixed_stream_byte_exact_in_order() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log6-drop-order")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-drop6").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-drop6", "sess-drop6", bus).with_logs(logs);
        ctx.emit_raw(b"R1");
        ctx.log_tx(b"T1", "T1");
        ctx.log_keystrokes(b"K1");
        ctx.emit_text("[note] N1", ChunkStyle::Dim);
        ctx.emit_raw("R2-中文".as_bytes());
        ctx.log_tx(b"T2\r\n", "T2");
        ctx.log_keystrokes(&[b'K', b'2', 0x7f]);
        ctx.emit_text("[note] N2", ChunkStyle::Dim);
        drop(ctx); // Drop directly without flushing; Drop closes the channel and waits for drain.
        let mut expected = Vec::new();
        expected.extend_from_slice(b"R1");
        expected.extend_from_slice(b"T1");
        expected.extend_from_slice(b"K1");
        expected.extend_from_slice(b"[note] N1");
        expected.extend_from_slice("R2-中文".as_bytes());
        expected.extend_from_slice(b"T2\r\n");
        expected.extend_from_slice(&[b'K', b'2', 0x7f]);
        expected.extend_from_slice(b"[note] N2");
        let raw = std::fs::read(dir.join("sess-drop6.bin")).unwrap();
        assert_eq!(
            raw, expected,
            ".bin must equal exact byte concatenation in enqueue order"
        );
        let text = std::fs::read_to_string(dir.join("sess-drop6.log")).unwrap();
        assert_eq!(
            text, "R1\n[TX] T1\n[note] N1\nR2-中文\n[TX] T2\n[note] N2\n",
            ".log line order must match enqueue order"
        );
    }

    /// LOG-7: structured `.rec` stream drains losslessly alongside `.log`/`.bin` on drop,
    /// preserving record kinds, exact payloads (multibyte UTF-8, `\r\n`, `0x7f`), and
    /// non-decreasing timestamps for zero-copy parsing.
    #[test]
    fn ctx_drop_drains_rec_records_in_order() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log7-rec-order")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-drop7").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-drop7", "sess-drop7", bus).with_logs(logs);
        ctx.emit_raw(b"R1");
        ctx.log_tx(b"T1", "T1");
        ctx.log_keystrokes(b"K1");
        ctx.emit_text("[note] N1", ChunkStyle::Dim);
        ctx.emit_raw("R2-中文".as_bytes());
        ctx.log_tx(b"T2\r\n", "T2");
        ctx.log_keystrokes(&[b'K', b'2', 0x7f]);
        ctx.emit_text("[note] N2", ChunkStyle::Dim);
        ctx.emit_mark("供电边界");
        drop(ctx);
        let raw = std::fs::read(dir.join("sess-drop7.rec")).unwrap();
        let batch = crate::rec::parse_records(&raw);
        let kinds: Vec<crate::rec::RecKind> =
            batch.records.iter().map(|record| record.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecKind::Raw,
                RecKind::Tx,
                RecKind::Keystrokes,
                RecKind::Note,
                RecKind::Raw,
                RecKind::Tx,
                RecKind::Keystrokes,
                RecKind::Note,
                RecKind::Mark,
            ],
            ".rec record order must match enqueue order"
        );
        let payloads: Vec<&[u8]> = batch.records.iter().map(|record| record.payload).collect();
        assert_eq!(
            payloads,
            vec![
                b"R1".as_slice(),
                b"T1".as_slice(),
                b"K1".as_slice(),
                b"[note] N1".as_slice(),
                "R2-中文".as_bytes(),
                b"T2\r\n".as_slice(),
                &[b'K', b'2', 0x7f].as_slice(),
                b"[note] N2".as_slice(),
                "[mark] 供电边界".as_bytes(),
            ],
            ".rec payloads must match enqueued bytes exactly"
        );
        assert_eq!(
            batch.valid_bytes,
            raw.len(),
            "no truncated trailing record allowed after drain"
        );
        // Timestamps are non-decreasing in consumer (enqueue) order.
        let stamps: Vec<u64> = batch
            .records
            .iter()
            .map(|record| record.ts_unix_ns)
            .collect();
        assert!(
            stamps.windows(2).all(|pair| pair[0] <= pair[1]),
            "timestamps must be non-decreasing: {stamps:?}"
        );
    }

    /// LOG-12: portable archive mode (desktop layout) drains all three streams (`txt`/`bin`/`rec`)
    /// losslessly on drop; the first message creates the segment directory when saving is enabled.
    /// Uses a valid archive-safe name (raw COM port names are reserved on Windows and rejected by
    /// `name_component`; the desktop layer guards them via `archive_safe_name`).
    #[test]
    fn ctx_portable_archive_drains_all_three_streams() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log12-portable")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let saving = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let quota = duthub_core::dut_quota::DutQuota::new(64 * 1024 * 1024);
        let logs = Arc::new(std::sync::Mutex::new(SessionLogWriter::portable_archive(
            &root, "dut-com8", "dut-com8", saving, quota,
        )));
        let bus = Arc::new(EventBus::new(64));
        let ctx = SessionCtx::new("dut-com8", "sess-com8", bus).with_logs(logs);
        ctx.log_keystrokes(b"k1");
        ctx.emit_raw(b"R1");
        drop(ctx);
        let logs_dir = root
            .join("dut-com8")
            .join(duthub_core::dut_artifacts::SERIAL_LOGS_DIR);
        let entries: Vec<String> = std::fs::read_dir(&logs_dir)
            .expect("first message must create portable segment directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != ".log-sequence")
            .collect();
        assert_eq!(entries.len(), 3, "one segment per stream: {entries:?}");
        assert!(entries.iter().all(|name| name.starts_with("dut-com8-001_")));
        assert!(entries.iter().any(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        }));
        assert!(entries.iter().any(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
        }));
        assert!(entries.iter().any(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rec"))
        }));
        std::fs::remove_dir_all(&root).ok();
    }

    /// PERF-1 audit side: each element of a merged keystroke batch logs to `.bin`
    /// independently interleaved with RX chunks, verifying exact ordering and that
    /// keystroke audit never enters `.log`.
    #[test]
    fn merged_batch_keystroke_audit_lands_per_command_in_order() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/perf1-audit")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-audit".into(),
            generation: 1,
        });
        let second = b"s\r".to_vec();
        let backspace = vec![0x7f_u8];
        push_writes(
            &tx,
            lease.as_ref(),
            &[second.as_slice(), backspace.as_slice()],
        );
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"l".to_vec(), lease.as_ref());
        assert!(deferred.is_none());
        assert_eq!(
            batch,
            vec![b"l".to_vec(), second.clone(), backspace.clone()],
            "merged batch must preserve each original command element"
        );
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-kaudit").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-ka", "sess-kaudit", bus).with_logs(logs);
        for element in &batch {
            ctx.log_keystrokes(element);
            ctx.emit_raw(b"<echo>");
        }
        ctx.flush_logs();
        let mut expected = Vec::new();
        for element in &batch {
            expected.extend_from_slice(element);
            expected.extend_from_slice(b"<echo>");
        }
        let raw = std::fs::read(dir.join("sess-kaudit.bin")).unwrap();
        assert_eq!(
            raw, expected,
            ".bin must persist each original command boundary in order"
        );
        let text = std::fs::read_to_string(dir.join("sess-kaudit.log")).unwrap();
        assert_eq!(
            text, "<echo>\n<echo>\n<echo>\n",
            "keystroke audit must go only to .bin, not .log"
        );
    }

    /// Garble detection wiring: enabled via the shared per-DUT map, raw bytes are sampled
    /// in `emit_raw`, verdicts attach to published snapshots, disabling resets to `Clean`,
    /// and `SerialChunk` text/order remain unchanged.
    #[test]
    fn garble_reaches_the_snapshot_only_when_enabled() {
        let bus = Arc::new(EventBus::new(64));
        let switch = Arc::new(Mutex::new(HashMap::new()));
        switch
            .lock()
            .unwrap()
            .insert("dut-garble".to_string(), true);
        let ctx =
            SessionCtx::new("dut-garble", "sess-garble", bus.clone()).with_garble(switch.clone());

        // 512 counted NUL bytes (>= MIN_WINDOW_BYTES) force the Garbled level.
        ctx.emit_raw(&[0x00u8; 512]);
        ctx.emit_raw(b"ok\r\n");
        ctx.emit_screen(duthub_core::ipc::ScreenSnapshot::default());

        let events = bus.read_log("sess-garble", 0, usize::MAX);
        let chunks: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            chunks,
            vec!["\0".repeat(512), "ok\r\n".to_string()],
            "garble detection must not alter SerialChunk text or order"
        );
        let screen = events
            .iter()
            .rev()
            .find_map(|e| match &e.event {
                UiEvent::SerialScreen { screen } => Some(screen.clone()),
                _ => None,
            })
            .expect("snapshot must be published to bus");
        assert_eq!(screen.garbled.level, duthub_core::ipc::GarbleLevel::Garbled);
        assert!(screen.garbled.control_permille > 0);
        assert_eq!(screen.garbled.window_bytes, 516);

        // Runtime switch flip: detection off resets the meter, snapshot Clean.
        switch
            .lock()
            .unwrap()
            .insert("dut-garble".to_string(), false);
        ctx.emit_screen(duthub_core::ipc::ScreenSnapshot::default());
        let screen = bus
            .read_log("sess-garble", 0, usize::MAX)
            .into_iter()
            .rev()
            .find_map(|e| match e.event {
                UiEvent::SerialScreen { screen } => Some(screen),
                _ => None,
            })
            .expect("snapshot must be published to bus");
        assert_eq!(screen.garbled.level, duthub_core::ipc::GarbleLevel::Clean);
        assert_eq!(screen.garbled.control_permille, 0);
        assert_eq!(screen.garbled.window_bytes, 0);
    }
}
