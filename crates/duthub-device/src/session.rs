//! Session registry, port ownership, cancellation, and reconnect backoff.
//!
//! Only one active session may hold a host and port.
//! Signal the actor to close and wait for it to finish.
//! Then remove the session handle and release the port.
//! The hardware reconnect path uses a configurable maximum backoff.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, mpsc};

use duthub_core::audit::AuditSink;
use duthub_core::lease::LeaseAuthority;
use duthub_core::{DeviceError, DeviceResult};

use crate::bus::EventBus;
pub use crate::demo::SessionCommand;
use crate::demo::{SessionCtx, run_demo_session};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Demo,
    Real,
}

/// Session line ending policy for transmitted text.
///
/// The default appends LF to a line without an ending.
/// A line ending in CR or LF passes through unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEnding {
    /// Send with no added line ending.
    Off,
    /// Append LF by default.
    #[default]
    Lf,
    /// Append CRLF for devices that expect Windows-style endings.
    Crlf,
}

impl LineEnding {
    /// Short label used by the UI.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Off => "无",
            LineEnding::Lf => "\\n",
            LineEnding::Crlf => "\\r\\n",
        }
    }

    /// Append the configured ending only when the text lacks one.
    #[must_use]
    pub fn apply(self, text: &str) -> String {
        if text.ends_with('\n') || text.ends_with('\r') {
            return text.to_string();
        }
        match self {
            LineEnding::Off => text.to_string(),
            LineEnding::Lf => format!("{text}\n"),
            LineEnding::Crlf => format!("{text}\r\n"),
        }
    }
}

pub struct SessionHandle {
    pub(crate) logs: Arc<Mutex<crate::logs::SessionLogWriter>>,
    /// Log writer channel used for synchronous control boundary markers.
    /// When no writer thread exists, write directly.
    pub(crate) log_tx: Option<std::sync::mpsc::Sender<crate::demo::LogMsg>>,
    pub dut_id: String,
    pub session_id: String,
    pub kind: SessionKind,
    pub host: String,
    pub port: String,
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    pub abort: tokio::task::AbortHandle,
    /// The session actor's join handle, taken by the watcher after an initial open failure.
    pub join: Option<tokio::task::JoinHandle<()>>,
    /// Set after a successful open. If the actor exits before then, reclaim its registry entry.
    pub opened_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// A manual reconnect wakes the backoff so the actor can retry immediately.
    /// The notifier retains one permit without a waiting actor.
    /// A request during an active connection can therefore wake the next backoff once.
    pub reconnect_notify: Arc<Notify>,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("dut_id", &self.dut_id)
            .field("session_id", &self.session_id)
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// Exponential reconnect backoff with a maximum. The first attempt uses the base delay.
#[must_use]
pub fn reconnect_backoff(attempt: u32, base: Duration, max: Duration) -> Duration {
    let exp = attempt.saturating_sub(1).min(16);
    let scaled = base.saturating_mul(1u32 << exp);
    scaled.min(max)
}

/// Session registry and its exclusive port ownership map.
#[derive(Debug, Default)]
pub struct Registry {
    sessions: HashMap<String, SessionHandle>,
    locked_ports: HashSet<(String, String)>,
    closing: HashMap<String, tokio::task::Id>,
}

impl Registry {
    #[must_use]
    pub fn get(&self, dut_id: &str) -> Option<&SessionHandle> {
        self.sessions.get(dut_id)
    }

    pub(crate) fn is_reserved(&self, dut_id: &str) -> bool {
        self.sessions.contains_key(dut_id) || self.closing.contains_key(dut_id)
    }

    pub(crate) fn begin_close(&mut self, dut_id: &str) -> Option<SessionHandle> {
        let handle = self.sessions.remove(dut_id)?;
        self.closing.insert(dut_id.to_owned(), handle.abort.id());
        Some(handle)
    }

    pub fn lock_port(&mut self, host: &str, port: &str) -> DeviceResult<()> {
        let key = (host.to_string(), port.to_string());
        if self.locked_ports.contains(&key) {
            return Err(DeviceError::Busy);
        }
        self.locked_ports.insert(key);
        Ok(())
    }

    pub fn unlock_port(&mut self, host: &str, port: &str) {
        self.locked_ports
            .remove(&(host.to_string(), port.to_string()));
    }

    /// Complete teardown only for the matching actor generation.
    pub(crate) fn finish(&mut self, dut_id: &str, actor: tokio::task::Id) -> Option<bool> {
        if self.closing.get(dut_id) == Some(&actor) {
            self.closing.remove(dut_id);
            return Some(true);
        }
        if self.sessions.get(dut_id)?.abort.id() != actor {
            return None;
        }
        self.sessions.remove(dut_id);
        Some(false)
    }

    pub fn insert(&mut self, handle: SessionHandle) {
        self.sessions.insert(handle.dut_id.clone(), handle);
    }

    pub fn remove(&mut self, dut_id: &str) -> Option<SessionHandle> {
        self.sessions.remove(dut_id)
    }

    #[must_use]
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }
}

/// Parameters for opening a session actor.
pub struct SessionSpec<'a> {
    pub dut_id: &'a str,
    pub session_id: &'a str,
    pub kind: SessionKind,
    pub port: &'a str,
    pub baud: u32,
    pub demo_line_delay_ms: u64,
    /// Terminal history capacity used when constructing a real session.
    pub scrollback_rows: usize,
    /// Maximum number of history rows carried in a snapshot.
    pub snapshot_max_rows: usize,
    pub logs: Arc<Mutex<crate::logs::SessionLogWriter>>,
    /// Tracks whether a real session opened its port at least once.
    pub opened_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The actor validates any lease declared by a command before writing bytes.
    pub lease_authority: Option<Arc<dyn LeaseAuthority>>,
    /// Optional audit sink for denied writes.
    pub audit: Option<Arc<AuditSink>>,
    /// Shared per-DUT garbled-output detection settings.
    pub garble_enabled: Arc<Mutex<HashMap<String, bool>>>,
}

impl std::fmt::Debug for SessionSpec<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSpec")
            .field("dut_id", &self.dut_id)
            .field("session_id", &self.session_id)
            .field("kind", &self.kind)
            .field("port", &self.port)
            .field("baud", &self.baud)
            .finish_non_exhaustive()
    }
}

/// Start the appropriate actor and return its session handle and command sender.
pub fn spawn_session(
    bus: Arc<EventBus>,
    spec: SessionSpec<'_>,
) -> DeviceResult<(SessionHandle, mpsc::Sender<SessionCommand>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(32);
    let reconnect_notify = Arc::new(Notify::new());
    let ctx = SessionCtx::new(spec.dut_id, spec.session_id, bus)
        .with_logs(spec.logs.clone())
        .with_reconnect_notify(reconnect_notify.clone())
        .with_lease_authority(spec.lease_authority.clone())
        .with_audit(spec.audit.clone())
        .with_garble(spec.garble_enabled.clone());
    let log_tx = ctx.log_sender();
    let task = match spec.kind {
        SessionKind::Demo => tokio::spawn(async move {
            run_demo_session(ctx, cmd_rx, spec.demo_line_delay_ms).await;
        }),
        SessionKind::Real => {
            #[cfg(feature = "real-hardware")]
            {
                let port = spec.port.to_string();
                let baud = spec.baud;
                let scrollback_rows = spec.scrollback_rows;
                let snapshot_max_rows = spec.snapshot_max_rows;
                let flag = spec.opened_flag.clone().unwrap_or_else(|| {
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
                });
                tokio::spawn(crate::real::run_real_session(
                    ctx,
                    cmd_rx,
                    port,
                    baud,
                    flag,
                    scrollback_rows,
                    snapshot_max_rows,
                ))
            }
            #[cfg(not(feature = "real-hardware"))]
            {
                let _ = (ctx, spec.port, spec.baud);
                return Err(DeviceError::Unsupported);
            }
        }
    };
    let abort = task.abort_handle();
    let handle = SessionHandle {
        logs: spec.logs,
        log_tx,
        dut_id: spec.dut_id.to_string(),
        session_id: spec.session_id.to_string(),
        kind: spec.kind,
        host: "local".to_string(),
        port: spec.port.to_string(),
        cmd_tx: cmd_tx.clone(),
        abort,
        join: Some(task),
        opened_flag: spec
            .opened_flag
            .clone()
            .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))),
        reconnect_notify,
    };
    Ok((handle, cmd_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::ipc::UiEvent;
    use duthub_core::lease::WriteIdentity;

    #[test]
    fn line_ending_apply_adds_configured_suffix() {
        assert_eq!(LineEnding::Lf.apply("ls"), "ls\n");
        assert_eq!(LineEnding::Crlf.apply("ls"), "ls\r\n");
        assert_eq!(LineEnding::Off.apply("ls"), "ls");
        // Preserve text that already ends in CR or LF.
        assert_eq!(LineEnding::Lf.apply("ls\n"), "ls\n");
        assert_eq!(LineEnding::Crlf.apply("ls\r\n"), "ls\r\n");
        assert_eq!(LineEnding::Off.apply("ls\r"), "ls\r");
        // LF remains the default line ending.
        assert_eq!(LineEnding::default(), LineEnding::Lf);
    }

    #[test]
    fn line_ending_labels_cover_all_variants() {
        assert_eq!(LineEnding::Off.label(), "无");
        assert_eq!(LineEnding::Lf.label(), "\\n");
        assert_eq!(LineEnding::Crlf.label(), "\\r\\n");
    }

    #[test]
    fn port_lock_is_exclusive() {
        let mut registry = Registry::default();
        registry.lock_port("local", "COM3").unwrap();
        let err = registry.lock_port("local", "COM3").unwrap_err();
        assert!(
            matches!(err, DeviceError::Busy),
            "second bind on the same port must return Busy"
        );
        registry.unlock_port("local", "COM3");
        registry.lock_port("local", "COM3").unwrap();
    }

    #[test]
    fn different_ports_can_coexist() {
        let mut registry = Registry::default();
        registry.lock_port("local", "COM3").unwrap();
        registry.lock_port("local", "COM5").unwrap();
        registry.lock_port("lab-pc", "COM3").unwrap();
    }

    #[test]
    fn backoff_grows_and_caps() {
        let base = Duration::from_millis(200);
        let max = Duration::from_secs(8);
        assert_eq!(reconnect_backoff(1, base, max), Duration::from_millis(200));
        assert_eq!(reconnect_backoff(2, base, max), Duration::from_millis(400));
        assert_eq!(reconnect_backoff(3, base, max), Duration::from_millis(800));
        assert_eq!(reconnect_backoff(10, base, max), max);
        assert_eq!(reconnect_backoff(999, base, max), max);
    }

    /// Exercise all combinations of the three policies and three existing line endings.
    /// An existing CR, LF, or CRLF must pass through byte for byte.
    /// The suffix test separately covers text without an ending.
    /// Together the tests cover both paths.
    #[test]
    fn line_ending_apply_passthrough_matrix_all_policies() {
        for ending in [LineEnding::Off, LineEnding::Lf, LineEnding::Crlf] {
            for ended in ["x\n", "x\r\n", "x\r"] {
                assert_eq!(
                    ending.apply(ended),
                    ended,
                    "{ending:?} must not double-append to already terminated {ended:?}"
                );
            }
        }
    }

    /// A demo session applies its current line ending policy to each write.
    /// Expect LF until a different policy is set.
    /// Already terminated text passes through under every policy.
    /// Use a generous deadline for the asynchronous write.
    /// Drain the log writer before reading the raw output.
    #[tokio::test]
    async fn session_send_path_applies_line_ending_policy_to_wire_bytes() {
        // Use the repository target directory for temporary test data.
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device")
            .join(format!("session-line-ending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logs = Arc::new(Mutex::new(
            crate::logs::SessionLogWriter::new(&dir, "sess-lio").unwrap(),
        ));
        let bus = Arc::new(EventBus::new(256));
        let spec = SessionSpec {
            dut_id: "dut-lio",
            session_id: "sess-lio",
            kind: SessionKind::Demo,
            port: "COM3",
            baud: 115_200,
            demo_line_delay_ms: 0,
            scrollback_rows: crate::term::SCROLLBACK_HISTORY,
            snapshot_max_rows: crate::term::SNAPSHOT_MAX_ROWS,
            logs: logs.clone(),
            opened_flag: None,
            lease_authority: None,
            audit: None,
            garble_enabled: Arc::new(Mutex::new(HashMap::new())),
        };
        let (mut handle, cmd_tx) = spawn_session(bus.clone(), spec).unwrap();

        let send_write = |text: &str| {
            cmd_tx.send(SessionCommand::Write {
                text: text.to_string(),
                lease: None,
            })
        };
        // The new session starts with LF without setting a policy.
        send_write("probe-default").await.unwrap();
        cmd_tx
            .send(SessionCommand::SetEnding(LineEnding::Crlf))
            .await
            .unwrap();
        send_write("probe-crlf").await.unwrap();
        // An existing LF is preserved under the CRLF policy.
        send_write("probe-ends-lf\n").await.unwrap();
        cmd_tx
            .send(SessionCommand::SetEnding(LineEnding::Off))
            .await
            .unwrap();
        send_write("probe-off").await.unwrap();
        // An existing CRLF is preserved when suffixes are off.
        send_write("probe-ends-crlf\r\n").await.unwrap();

        // Wait for the final TX event before reading logs.
        // The actor queues bytes for logging before publishing the event.
        let mut processed = false;
        for _ in 0..250 {
            processed = bus
                .read_log("sess-lio", 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(&e.event, UiEvent::SerialChunk { text, .. } if text == "[TX] probe-ends-crlf\\r\\n")
                });
            if processed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            processed,
            "all writes must reach the demo session (final [TX] echo visible)"
        );

        // Flush the log queue before reading the file.
        if let Some(tx) = &handle.log_tx {
            let (ack, rx) = std::sync::mpsc::channel();
            if tx.send(crate::demo::LogMsg::Flush(ack)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_secs(3));
            }
        }
        // Drop the last sender and join the actor so the writer has time to finish.
        let join = handle.join.take();
        drop(handle);
        drop(cmd_tx);
        let _ = join.unwrap().await;

        let raw = std::fs::read(dir.join("sess-lio.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        // Verify the default LF suffix.
        assert!(
            raw.contains("probe-default\n"),
            "default policy must append \\n: {raw:?}"
        );
        assert!(
            !raw.contains("probe-default\r\n"),
            "default policy must not write CRLF"
        );
        // The new CRLF policy applies to subsequent writes.
        assert!(
            raw.contains("probe-crlf\r\n"),
            "Crlf policy must append \\r\\n: {raw:?}"
        );
        // Preserve lines that already have an ending.
        assert!(
            raw.contains("probe-ends-lf\n"),
            "text already ending in \\n must pass through verbatim: {raw:?}"
        );
        assert!(
            !raw.contains("probe-ends-lf\n\r\n"),
            "Crlf policy must not double-append to terminated text: {raw:?}"
        );
        // The disabled policy adds no suffix.
        assert!(
            raw.contains("probe-off"),
            "Off policy must preserve body: {raw:?}"
        );
        assert!(
            !raw.contains("probe-off\n"),
            "Off policy must not append \\n: {raw:?}"
        );
        assert!(
            !raw.contains("probe-off\r\n"),
            "Off policy must not append \\r\\n: {raw:?}"
        );
        assert!(
            raw.contains("probe-ends-crlf\r\n"),
            "Off policy must pass through existing \\r\\n verbatim: {raw:?}"
        );
    }

    /// Normalize multiline writes by collapsing CRLF first, then converting remaining LF to CR.
    /// Keep existing CR separators intact.
    /// Apply the configured line ending only after normalizing the full payload.
    /// A payload already ending in CR needs no suffix.
    #[test]
    fn write_line_normalizes_multiline_to_cr_without_crlf_mixing() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device")
            .join(format!("session-normalize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let service = crate::DeviceService::custom(64, dir.clone(), 0);
        service
            .open_session("dut-norm", "COM3", 115_200, SessionKind::Demo)
            .unwrap();
        // Use mixed input to exercise each normalization step.
        service
            .write_line("dut-norm", WriteIdentity::gui(), "M1\r\nM2\nM3\rM4")
            .unwrap();
        // A trailing CR counts as an existing line ending.
        service
            .write_line("dut-norm", WriteIdentity::gui(), "tail\r")
            .unwrap();
        // The final TX event confirms the send path has handled all writes.
        let mut processed = false;
        for _ in 0..250 {
            processed = service.read_log("sess-dut-norm", 0, usize::MAX).iter().any(
                |e| matches!(&e.event, UiEvent::SerialChunk { text, .. } if text == "[TX] tail\\r"),
            );
            if processed {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            processed,
            "all writes must reach the demo session (final [TX] echo visible)"
        );
        // Flush the writer before inspecting the binary log.
        service.flush_session_logs("dut-norm");
        service.close_session("dut-norm").ok();
        let raw = std::fs::read(dir.join("sess-dut-norm.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        // Expect CR between lines and the default LF suffix.
        assert!(
            raw.contains("M1\rM2\rM3\rM4\n"),
            "multiline payload must separate with CR and terminate with default Lf: {raw:?}"
        );
        assert!(
            !raw.contains("M1\r\n"),
            "CRLF must not pass through verbatim: {raw:?}"
        );
        assert!(
            !raw.contains("M1\r\r"),
            "folding order must not turn leftover LF into \\r\\r: {raw:?}"
        );
        assert!(
            !raw.contains("M2\n"),
            "remaining bare LF must fold to CR: {raw:?}"
        );
        // Preserve text that already ends in CR.
        assert!(
            raw.contains("tail\r"),
            "text already ending in CR must pass through verbatim: {raw:?}"
        );
        assert!(
            !raw.contains("tail\r\n"),
            "default Lf must not double-append to terminated text: {raw:?}"
        );
    }
}
