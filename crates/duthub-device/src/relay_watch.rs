//! Board-level offline monitoring for shared relay boards.
//!
//! A relay board is a shared device: several DUTs may bind the same
//! `duts.json.relayBoards` entry, and the board is reachable without any DUT
//! serial session. This module therefore monitors **boards**, not sessions:
//!
//! - the only traffic it generates is a read-only single-channel STATUS probe
//!   ([`RelayBoardProbe`]);
//! - it never takes a control lease, never writes `[mark]` lines and never
//!   moves a DUT serial session into `NeedsInspection` (AGENTS.md pit #40);
//! - the probe implementation ([`crate::control::ControlRelayProbe`]) reuses
//!   the existing relay write gate, so a press-and-hold test is simply skipped
//!   (`Gated`) instead of queueing behind it. That gate is one
//!   `ControlService`-wide lock, so a hold on any board gates every board's
//!   probe; a gated probe therefore changes state silently and never produces
//!   a per-board notice (see [`apply_sample`]).
//!
//! Fault classification is the point of the whole module: only *the port
//! disappeared* or *the port answered but the board stayed silent* count as
//! `Offline`. A busy port, a held write gate or an unsupported backend is
//! `Unobservable` — a state we cannot judge, not a fault.
//!
//! Lock discipline: [`RelayWatchService`] never holds its mutex across I/O.
//! [`RelayWatchService::tick_at`] snapshots the boards that are due, releases
//! the lock, probes, and then re-acquires the lock to fold the samples back in.
//! That keeps [`RelayWatchService::sync_boards`] instant, so saving a DUT
//! configuration on the UI thread never waits for a hung board (worst case one
//! probe blocks for the backend reply timeout).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use duthub_core::DeviceError;
use duthub_core::model::{DutConfig, RelayBoard, RelayConfig};

/// Board state reported by the monitor.
///
/// [`RelayBoardState::Recovered`] is an edge, not a steady state: it only
/// appears on [`RelayBoardEvent::state`] when an `Offline` board answers again.
/// [`RelayWatchService::states`] therefore only reports `Online`, `Offline` and
/// `Unobservable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayBoardState {
    Online,
    Offline,
    Recovered,
    Unobservable,
}

/// One monitor event for the UI layer to turn into an alert or a toast.
///
/// `at_unix_ms` is whatever clock the caller injected into
/// [`RelayWatchService::tick_at`] (production passes unix milliseconds, tests
/// pass a synthetic clock so assertions stay deterministic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayBoardEvent {
    /// Relay board id (`duts.json.relayBoards[].id`) — also the alert subject.
    pub board_id: String,
    /// Board display name at the time of the event.
    pub name: String,
    pub state: RelayBoardState,
    /// Human-readable reason (shown as alert detail / toast body).
    pub detail: String,
    pub at_unix_ms: u64,
}

/// Raw result of one board probe.
///
/// The distinction that matters is *why* the probe failed: [`Self::PortMissing`]
/// and [`Self::Silent`] are faults, everything else is "we could not look".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayProbeSample {
    /// The board answered STATUS with the channel state.
    Answered(bool),
    /// The port opened but the board did not answer STATUS.
    Silent,
    /// The port itself is gone (not found / disconnected / timeout).
    PortMissing(String),
    /// The port exists but another process (or the OS) holds it.
    PortBusy(String),
    /// The probe was not allowed to run: the relay write gate is held by a
    /// press-and-hold test. That gate is one `ControlService`-wide lock, not a
    /// per-board one, so a hold on any board gates *every* board's probe — see
    /// [`apply_sample`] for why this never turns into a per-board notice.
    Gated(String),
    /// No usable probe path: unconfigured board, PDU backend, invalid channel…
    Unsupported(String),
}

/// Read-only board probe. Implementations perform blocking I/O, so they are
/// only ever called from the monitor thread — never from the UI or the 50 ms
/// tick.
pub trait RelayBoardProbe: Send + Sync {
    fn probe_board(&self, board: &RelayBoard) -> RelayProbeSample;
}

/// Monitoring cadence and thresholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayWatchConfig {
    /// Probe interval per board.
    pub interval_ms: u64,
    /// Consecutive fault probes before a board is reported `Offline`.
    pub confirmations: u32,
    /// While offline, repeat the alert at most this often.
    pub remind_interval_ms: u64,
}

impl Default for RelayWatchConfig {
    fn default() -> Self {
        Self {
            interval_ms: 30_000,
            confirmations: 2,
            remind_interval_ms: 300_000,
        }
    }
}

impl RelayWatchConfig {
    /// Clamp degenerate values so a misconfigured entry cannot turn the
    /// monitor into a busy loop or flood the reminder queue.
    fn sanitized(mut self) -> Self {
        self.interval_ms = self.interval_ms.max(1_000);
        self.confirmations = self.confirmations.max(1);
        self.remind_interval_ms = self.remind_interval_ms.max(self.interval_ms);
        self
    }
}

/// Fault classification of a probe sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayProbeVerdict {
    Online,
    Offline,
    Unobservable,
}

/// Map a probe sample to the verdict the state machine acts on.
///
/// Only "the port vanished" and "the port answered, the board did not" are
/// faults. A busy port or a held gate must never be reported as a dead board.
pub fn classify_probe(sample: &RelayProbeSample) -> RelayProbeVerdict {
    match sample {
        RelayProbeSample::Answered(_) => RelayProbeVerdict::Online,
        RelayProbeSample::Silent | RelayProbeSample::PortMissing(_) => RelayProbeVerdict::Offline,
        RelayProbeSample::PortBusy(_)
        | RelayProbeSample::Gated(_)
        | RelayProbeSample::Unsupported(_) => RelayProbeVerdict::Unobservable,
    }
}

/// Classify a backend failure raised by a probe.
///
/// `NotFound` / `Disconnected` / `Timeout` mean the port is unusable → missing.
/// `PermissionDenied` / `Busy` mean somebody else owns the port → busy.
/// `Invalid` (driver-side argument or channel rejection) and `Unsupported`
/// mean the probe path itself is unusable → unsupported.
/// `Lease` is a defensive mapping only: the shipped probe takes no control
/// lease (`ControlService::probe_board_status`), so `Lease` cannot come out of
/// the production path, but a custom `ControlBackend` must not be able to turn
/// "not allowed" into "offline".
///
/// All four of the latter groups are `Unobservable`: none of them proves the
/// board is down.
pub fn classify_probe_error(error: &DeviceError) -> RelayProbeSample {
    match error {
        DeviceError::NotFound => {
            RelayProbeSample::PortMissing("继电器串口未找到（端口已消失）".into())
        }
        DeviceError::Disconnected => RelayProbeSample::PortMissing("继电器串口已断开".into()),
        DeviceError::Timeout => RelayProbeSample::PortMissing("继电器板在超时时间内无应答".into()),
        DeviceError::PermissionDenied => {
            RelayProbeSample::PortBusy("继电器串口被其他程序占用".into())
        }
        DeviceError::Busy => RelayProbeSample::PortBusy("继电器串口忙".into()),
        DeviceError::Lease(reason) => RelayProbeSample::Gated(format!("控制门拒绝探测：{reason}")),
        DeviceError::Unsupported => RelayProbeSample::Unsupported("控制后端不支持状态探测".into()),
        DeviceError::Invalid(what) => {
            RelayProbeSample::Unsupported(format!("探测参数无效：{what}"))
        }
    }
}

/// Whether the monitor is allowed to probe this board at all.
///
/// Three conditions must hold before a board is watched (design §6.1):
/// 1. the driver is empty or CH340 — a PDU / unknown driver must not be opened
///    as a local serial relay;
/// 2. `node` is non-empty — a freshly created board has no port yet, and
///    periodically reporting it as offline would be pure noise;
/// 3. at least one saved DUT binds the board. That condition needs the DUT
///    list, so the caller filters it before calling
///    [`RelayWatchService::sync_boards`].
///
/// Boards hosted elsewhere (`host` not `local`) are also skipped: there is no
/// remote relay transport, and opening a same-named local port would be a lie.
pub fn is_watchable_board(board: &RelayBoard) -> bool {
    let driver_supported = board.driver.is_empty() || board.driver.eq_ignore_ascii_case("CH340");
    let local = board.host.is_empty() || board.host.eq_ignore_ascii_case("local");
    driver_supported && local && !board.node.trim().is_empty()
}

/// Probe channel per watchable board: the lowest channel that any saved DUT
/// bound to the board.
///
/// The monitor reads one channel to learn whether the *board* answers, so the
/// channel itself is arbitrary; the lowest one is chosen to stay stable no
/// matter in which order the DUTs are stored or which DUT wins. A board bound
/// by a DUT but with no functional channel assigned gets no entry — it has no
/// channel to read, so the probe reports `Unobservable` instead of guessing
/// (and never opens the port).
///
/// Feed the result straight into
/// [`crate::control::ControlRelayProbe::set_channels`].
pub fn board_probe_channels(boards: &[RelayBoard], duts: &[DutConfig]) -> Vec<(String, u8)> {
    let mut picked: BTreeMap<String, u8> = BTreeMap::new();
    for dut in duts {
        let RelayConfig::UsbBoard { board_id, channels } = &dut.relay else {
            continue;
        };
        if !boards.iter().any(|board| board.id == *board_id) {
            continue;
        }
        for channel in channels.assigned() {
            picked
                .entry(board_id.clone())
                .and_modify(|current| *current = (*current).min(channel))
                .or_insert(channel);
        }
    }
    picked.into_iter().collect()
}

/// Per-board monitoring state.
#[derive(Debug, Clone)]
struct BoardEntry {
    board: RelayBoard,
    state: RelayBoardState,
    /// `true` while the user has been told this board is offline; the next
    /// answer then emits `Recovered`. Kept apart from `state` because an
    /// unobservable probe in between (a hold test, a busy port) must not
    /// swallow the recovery notice.
    reported_offline: bool,
    consecutive_offline: u32,
    last_probe_ms: u64,
    last_emit_ms: u64,
    next_due_ms: u64,
}

impl BoardEntry {
    fn new(board: RelayBoard) -> Self {
        Self {
            board,
            // Nominal state before the first sample: an `Online` board emits
            // nothing, so a fresh entry stays quiet until a fault is confirmed.
            state: RelayBoardState::Online,
            reported_offline: false,
            consecutive_offline: 0,
            last_probe_ms: 0,
            last_emit_ms: 0,
            // New boards are due immediately: a board that is already broken
            // must be reported without waiting a whole interval.
            next_due_ms: 0,
        }
    }

    fn event(&self, state: RelayBoardState, detail: &str, now_ms: u64) -> RelayBoardEvent {
        RelayBoardEvent {
            board_id: self.board.id.clone(),
            name: self.board.name.clone(),
            state,
            detail: detail.to_string(),
            at_unix_ms: now_ms,
        }
    }
}

/// Change-detection key of the watched set (`id`, `name`, `node`, `driver`).
type BoardSignature = (String, String, String, String);

#[derive(Debug, Default)]
struct WatchInner {
    boards: BTreeMap<String, BoardEntry>,
    signature: Vec<BoardSignature>,
    /// Bumped whenever the watched set changes; a probe result produced under
    /// an older revision is discarded instead of being folded into a board
    /// that was reconfigured or removed meanwhile.
    revision: u64,
    events: Vec<RelayBoardEvent>,
}

/// Board-level offline monitor.
///
/// Call [`Self::sync_boards`] with the current watch set (cheap, idempotent,
/// safe to call every tick), then [`Self::tick_at`] from a dedicated thread
/// (it blocks on I/O) and [`Self::drain_events`] from the UI side.
pub struct RelayWatchService {
    probe: Arc<dyn RelayBoardProbe>,
    config: RelayWatchConfig,
    inner: Mutex<WatchInner>,
}

impl RelayWatchService {
    pub fn new(probe: Arc<dyn RelayBoardProbe>) -> Self {
        Self {
            probe,
            config: RelayWatchConfig::default().sanitized(),
            inner: Mutex::new(WatchInner::default()),
        }
    }

    /// Override cadence/thresholds (values are clamped, see
    /// [`RelayWatchConfig::sanitized`]).
    pub fn with_config(mut self, config: RelayWatchConfig) -> Self {
        self.config = config.sanitized();
        self
    }

    pub fn config(&self) -> &RelayWatchConfig {
        &self.config
    }

    /// Align the watched set with the current board configuration.
    ///
    /// Only boards that pass [`is_watchable_board`] are kept; the caller is
    /// responsible for the "bound by at least one DUT" filter. State survives
    /// a rename or a channel-count edit of the same physical endpoint
    /// (`node` + `driver` unchanged); a board whose endpoint changed, or one
    /// that was not watched before, starts fresh and is due immediately.
    ///
    /// The function only compares signatures and swaps the map under the lock,
    /// so it returns immediately even while a probe is in flight.
    pub fn sync_boards(&self, boards: &[RelayBoard]) {
        let mut watchable: Vec<&RelayBoard> = boards
            .iter()
            .filter(|board| is_watchable_board(board))
            .collect();
        watchable.sort_by(|a, b| a.id.cmp(&b.id));
        watchable.dedup_by(|a, b| a.id == b.id);
        let signature: Vec<BoardSignature> = watchable
            .iter()
            .map(|board| {
                (
                    board.id.clone(),
                    board.name.clone(),
                    board.node.clone(),
                    board.driver.clone(),
                )
            })
            .collect();
        let mut inner = self.lock();
        if inner.signature == signature {
            return;
        }
        let previous = std::mem::take(&mut inner.boards);
        let mut next = BTreeMap::new();
        for board in watchable {
            let entry = match previous.get(&board.id) {
                Some(old) if old.board.node == board.node && old.board.driver == board.driver => {
                    let mut kept = old.clone();
                    kept.board = board.clone();
                    kept
                }
                _ => BoardEntry::new(board.clone()),
            };
            next.insert(board.id.clone(), entry);
        }
        inner.boards = next;
        inner.signature = signature;
        inner.revision = inner.revision.wrapping_add(1);
    }

    /// Probe every board whose interval elapsed, fold the results into the
    /// state machine and queue the resulting events.
    ///
    /// Returns `true` when at least one board was due and probed. This call
    /// blocks on I/O (one probe per due board, worst case the backend reply
    /// timeout) — it must not run on the UI thread or from a render tick.
    pub fn tick_at(&self, now_ms: u64) -> bool {
        // 1) Snapshot the due boards, then release the lock before any I/O.
        let (revision, due) = {
            let inner = self.lock();
            let due: Vec<RelayBoard> = inner
                .boards
                .values()
                .filter(|entry| entry.next_due_ms <= now_ms)
                .map(|entry| entry.board.clone())
                .collect();
            if due.is_empty() {
                return false;
            }
            (inner.revision, due)
        };
        // 2) Probe outside the lock so `sync_boards` (UI thread) never waits.
        let samples: Vec<(String, RelayProbeSample)> = due
            .iter()
            .map(|board| (board.id.clone(), self.probe.probe_board(board)))
            .collect();
        // 3) Re-acquire and fold the samples back in.
        let mut inner = self.lock();
        if inner.revision != revision {
            // The watched set changed while probing: these samples may belong
            // to boards that were just removed or reconfigured. Drop them (the
            // new set is due immediately and is probed on the next tick) but
            // still report that probing happened.
            return true;
        }
        let mut events = Vec::new();
        for (board_id, sample) in samples {
            let Some(entry) = inner.boards.get_mut(&board_id) else {
                continue;
            };
            entry.last_probe_ms = now_ms;
            entry.next_due_ms = now_ms.saturating_add(self.config.interval_ms);
            if let Some(event) = apply_sample(entry, &sample, &self.config, now_ms) {
                events.push(event);
            }
        }
        inner.events.extend(events);
        true
    }

    /// Take the queued events (the caller owns the alert/toast mapping).
    pub fn drain_events(&self) -> Vec<RelayBoardEvent> {
        std::mem::take(&mut self.lock().events)
    }

    /// Current steady state per watched board (diagnostics / UI).
    pub fn states(&self) -> Vec<(String, RelayBoardState)> {
        self.lock()
            .boards
            .values()
            .map(|entry| (entry.board.id.clone(), entry.state))
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, WatchInner> {
        // A monitoring service must survive a poisoned lock: the state is
        // plain data and the next tick re-establishes it from fresh samples.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Fold one sample into a board's state and return the event it produces.
///
/// - `Online`: clear the failure streak; a board the user was told is offline
///   emits `Recovered` (even if an unobservable probe happened in between).
/// - `Offline`: count the failure; only the `confirmations`-th consecutive
///   failure emits, then repeated at most every `remind_interval_ms`.
/// - `Unobservable`: clear the streak and go quiet. A *per-board* blind spot
///   (busy port, no channel, unsupported backend) is reported once on entry.
///   A gated probe is reported **not at all**: the relay write gate is one
///   `ControlService`-wide lock, so a press-and-hold test on any board gates
///   every board's probe — one "cannot observe" notice per board would
///   misattribute a normal, transient condition of a *different* board.
fn apply_sample(
    entry: &mut BoardEntry,
    sample: &RelayProbeSample,
    config: &RelayWatchConfig,
    now_ms: u64,
) -> Option<RelayBoardEvent> {
    match classify_probe(sample) {
        RelayProbeVerdict::Online => {
            entry.consecutive_offline = 0;
            let recovered = std::mem::replace(&mut entry.reported_offline, false);
            entry.state = RelayBoardState::Online;
            recovered.then(|| entry.event(RelayBoardState::Recovered, "板卡恢复应答", now_ms))
        }
        RelayProbeVerdict::Offline => {
            entry.consecutive_offline = entry.consecutive_offline.saturating_add(1);
            if entry.consecutive_offline < config.confirmations {
                return None;
            }
            // The reminder gate keys off "the user was already told", not off
            // the current state: a hold test (or a busy port) in between makes
            // the board unobservable for a while without retracting the
            // offline notice, and re-confirming the same outage must not
            // produce a second alert ahead of `remind_interval_ms`.
            let reminder_due = !entry.reported_offline
                || now_ms >= entry.last_emit_ms.saturating_add(config.remind_interval_ms);
            if !reminder_due {
                return None;
            }
            entry.state = RelayBoardState::Offline;
            entry.reported_offline = true;
            entry.last_emit_ms = now_ms;
            let detail = format!(
                "{}（连续 {} 次探测失败）",
                sample_detail(sample),
                entry.consecutive_offline
            );
            Some(entry.event(RelayBoardState::Offline, &detail, now_ms))
        }
        RelayProbeVerdict::Unobservable => {
            entry.consecutive_offline = 0;
            let entered = entry.state != RelayBoardState::Unobservable;
            entry.state = RelayBoardState::Unobservable;
            if !entered || matches!(sample, RelayProbeSample::Gated(_)) {
                return None;
            }
            entry.last_emit_ms = now_ms;
            let detail = sample_detail(sample);
            Some(entry.event(RelayBoardState::Unobservable, &detail, now_ms))
        }
    }
}

/// Human-readable reason carried by an event.
fn sample_detail(sample: &RelayProbeSample) -> String {
    match sample {
        RelayProbeSample::Answered(on) => {
            format!("板卡应答（通道{}）", if *on { "吸合" } else { "释放" })
        }
        RelayProbeSample::Silent => "板卡无应答".to_string(),
        RelayProbeSample::PortMissing(detail)
        | RelayProbeSample::PortBusy(detail)
        | RelayProbeSample::Gated(detail)
        | RelayProbeSample::Unsupported(detail) => detail.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::model::{McpConfig, RelayChannels, SerialConfig};

    /// Samples are classified by "can we blame the board?" — anything that
    /// only hides the board from us must stay `Unobservable`.
    #[test]
    fn probe_classification_separates_faults_from_blind_spots() {
        assert_eq!(
            classify_probe(&RelayProbeSample::Answered(true)),
            RelayProbeVerdict::Online
        );
        assert_eq!(
            classify_probe(&RelayProbeSample::Answered(false)),
            RelayProbeVerdict::Online
        );
        assert_eq!(
            classify_probe(&RelayProbeSample::Silent),
            RelayProbeVerdict::Offline
        );
        assert_eq!(
            classify_probe(&RelayProbeSample::PortMissing("x".into())),
            RelayProbeVerdict::Offline
        );
        for sample in [
            RelayProbeSample::PortBusy("x".into()),
            RelayProbeSample::Gated("x".into()),
            RelayProbeSample::Unsupported("x".into()),
        ] {
            assert_eq!(classify_probe(&sample), RelayProbeVerdict::Unobservable);
        }
    }

    /// Backend errors map by cause: vanished port → missing, contended port →
    /// busy, refused lease → gated, unusable path → unsupported.
    #[test]
    fn backend_errors_map_to_probe_samples() {
        assert!(matches!(
            classify_probe_error(&DeviceError::NotFound),
            RelayProbeSample::PortMissing(_)
        ));
        assert!(matches!(
            classify_probe_error(&DeviceError::Disconnected),
            RelayProbeSample::PortMissing(_)
        ));
        assert!(matches!(
            classify_probe_error(&DeviceError::Timeout),
            RelayProbeSample::PortMissing(_)
        ));
        assert!(matches!(
            classify_probe_error(&DeviceError::PermissionDenied),
            RelayProbeSample::PortBusy(_)
        ));
        assert!(matches!(
            classify_probe_error(&DeviceError::Busy),
            RelayProbeSample::PortBusy(_)
        ));
        assert!(matches!(
            classify_probe_error(&DeviceError::Lease("人工接管".into())),
            RelayProbeSample::Gated(_)
        ));
        assert!(matches!(
            classify_probe_error(&DeviceError::Unsupported),
            RelayProbeSample::Unsupported(_)
        ));
        assert!(matches!(
            classify_probe_error(&DeviceError::Invalid("通道 0".into())),
            RelayProbeSample::Unsupported(_)
        ));
        // Nothing that hides the board from us may be read as a dead board.
        for error in [
            DeviceError::PermissionDenied,
            DeviceError::Busy,
            DeviceError::Lease("held".into()),
            DeviceError::Unsupported,
            DeviceError::Invalid("ch".into()),
        ] {
            assert_eq!(
                classify_probe(&classify_probe_error(&error)),
                RelayProbeVerdict::Unobservable,
                "{error:?} must never count as offline"
            );
        }
    }

    fn board(id: &str, node: &str, driver: &str) -> RelayBoard {
        RelayBoard {
            id: id.into(),
            name: format!("board {id}"),
            host: "local".into(),
            node: node.into(),
            driver: driver.into(),
            channel_count: 8,
        }
    }

    /// Decision 5: an unconfigured board (empty port) is not watched, so it is
    /// never probed and never reported. PDU / foreign drivers are skipped too.
    #[test]
    fn unconfigured_and_foreign_boards_are_not_watched() {
        assert!(!is_watchable_board(&board("b1", "", "CH340")));
        assert!(!is_watchable_board(&board("b2", "   ", "CH340")));
        assert!(!is_watchable_board(&board("b3", "COM25", "snmp-pdu")));
        assert!(is_watchable_board(&board("b4", "COM25", "CH340")));
        assert!(is_watchable_board(&board("b5", "COM25", "ch340")));
        assert!(is_watchable_board(&board("b6", "COM25", "")));
        let mut remote = board("b7", "COM25", "CH340");
        remote.host = "lab-pc".into();
        assert!(!is_watchable_board(&remote));
    }

    /// Degenerate config values are clamped, never used raw.
    #[test]
    fn config_clamps_degenerate_values() {
        let config = RelayWatchConfig {
            interval_ms: 0,
            confirmations: 0,
            remind_interval_ms: 0,
        }
        .sanitized();
        assert_eq!(config.interval_ms, 1_000);
        assert_eq!(config.confirmations, 1);
        assert_eq!(config.remind_interval_ms, config.interval_ms);
        let default = RelayWatchConfig::default();
        assert_eq!(default.interval_ms, 30_000);
        assert_eq!(default.confirmations, 2);
        assert_eq!(default.remind_interval_ms, 300_000);
    }

    fn dut(binding: RelayConfig) -> DutConfig {
        DutConfig {
            id: "dut".into(),
            name: "dut".into(),
            serial: SerialConfig {
                host: "local".into(),
                port: "COM3".into(),
                baud: 115_200,
                format: "8N1".into(),
                reference_log: None,
                reference_similarity: None,
                split_logs: false,
            },
            adb: Default::default(),
            relay: binding,
            mcp: McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8787/mcp".into(),
            },
            a2a: Default::default(),
            flash: Default::default(),
            media: Default::default(),
            heartbeat: Default::default(),
        }
    }

    fn usb_board(board_id: &str, channels: RelayChannels) -> RelayConfig {
        RelayConfig::UsbBoard {
            board_id: board_id.into(),
            channels,
        }
    }

    /// The probe channel is the lowest channel any DUT bound on the board, and
    /// a DUT-bound board without a channel gets no entry at all.
    #[test]
    fn probe_channels_follow_the_lowest_bound_channel() {
        let boards = vec![board("b1", "COM25", "CH340"), board("b2", "COM26", "CH340")];
        let duts = vec![
            dut(usb_board(
                "b1",
                RelayChannels {
                    power: Some(4),
                    reset: Some(6),
                    ..Default::default()
                },
            )),
            dut(usb_board(
                "b1",
                RelayChannels {
                    reset: Some(1),
                    ..Default::default()
                },
            )),
            // Bound but no channel: cannot be probed, no entry.
            dut(usb_board("b2", RelayChannels::default())),
        ];
        assert_eq!(
            board_probe_channels(&boards, &duts),
            vec![("b1".to_string(), 1)]
        );
        // A DUT bound to a board that is not in the watch set is ignored.
        let without_b1 = vec![board("b2", "COM26", "CH340")];
        assert!(board_probe_channels(&without_b1, &duts).is_empty());
        // Legacy per-DUT serial relay / PDU / disabled bindings never map.
        let legacy = vec![
            dut(RelayConfig::UsbSerial {
                host: "local".into(),
                node: "COM25".into(),
                driver: "CH340".into(),
                channels: RelayChannels {
                    power: Some(4),
                    ..Default::default()
                },
            }),
            dut(RelayConfig::SnmpPdu {
                snmp: Default::default(),
                outlets: Default::default(),
            }),
            dut(RelayConfig::Disabled),
        ];
        assert!(board_probe_channels(&boards, &legacy).is_empty());
    }
}
