//! Deterministic tests for the board-level relay offline monitor.
//!
//! Two layers are covered:
//! 1. `RelayWatchService` with a scripted [`RelayBoardProbe`] and an injected
//!    clock — the full state machine (confirmations, reminders, recovery,
//!    blind spots) without any hardware or timer.
//! 2. `ControlService::probe_board_status` / `ControlRelayProbe` over a mock
//!    `ControlBackend` — error classification, write-gate gating and the
//!    guarantee that a probe never touches approvals, leases or `[mark]` lines.
//!
//! Nothing here opens a real serial port (this file runs under the default
//! feature set).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use duthub_core::backend::ChannelTestReport;
use duthub_core::model::{
    DutConfig, FlashConfig, McpConfig, RelayBoard, RelayChannels, RelayConfig, SerialConfig,
};
use duthub_core::{
    ApprovalManager, ControlBackend, ControlLeaseManager, ControlTarget, DeviceError, DeviceResult,
    LeasePhase,
};
use duthub_device::control::{ControlRelayProbe, ControlService};
use duthub_device::relay_watch::{
    RelayBoardProbe, RelayBoardState, RelayProbeSample, RelayWatchConfig, RelayWatchService,
};

// ── helpers: boards, scripted probe, clock ─────────────────────────────────

fn board(id: &str, node: &str) -> RelayBoard {
    RelayBoard {
        id: id.into(),
        name: format!("继电器板 {id}"),
        host: "local".into(),
        node: node.into(),
        driver: "CH340".into(),
        channel_count: 8,
    }
}

fn config() -> RelayWatchConfig {
    RelayWatchConfig {
        interval_ms: 1_000,
        confirmations: 2,
        remind_interval_ms: 60_000,
    }
}

/// Shared script for the fake board: the current sample per board id plus the
/// probe call log. Unscripted boards answer `Answered(true)` (healthy).
#[derive(Default)]
struct Script {
    samples: HashMap<String, RelayProbeSample>,
    calls: Vec<String>,
}

impl Script {
    fn set(&mut self, board_id: &str, sample: RelayProbeSample) {
        self.samples.insert(board_id.to_string(), sample);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.clone()
    }

    fn call_count(&self) -> usize {
        self.calls.len()
    }
}

struct ScriptedProbe {
    script: Arc<Mutex<Script>>,
    /// Simulated probe latency, used to prove `sync_boards` never waits.
    delay: Duration,
}

impl RelayBoardProbe for ScriptedProbe {
    fn probe_board(&self, board: &RelayBoard) -> RelayProbeSample {
        {
            let mut script = self.script.lock().unwrap();
            script.calls.push(board.id.clone());
        }
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.script
            .lock()
            .unwrap()
            .samples
            .get(&board.id)
            .cloned()
            .unwrap_or(RelayProbeSample::Answered(false))
    }
}

fn scripted() -> (Arc<Mutex<Script>>, Arc<ScriptedProbe>) {
    scripted_with_delay(Duration::ZERO)
}

fn scripted_with_delay(delay: Duration) -> (Arc<Mutex<Script>>, Arc<ScriptedProbe>) {
    let script = Arc::new(Mutex::new(Script::default()));
    let probe = Arc::new(ScriptedProbe {
        script: script.clone(),
        delay,
    });
    (script, probe)
}

fn watch(probe: Arc<ScriptedProbe>) -> RelayWatchService {
    RelayWatchService::new(probe).with_config(config())
}

// ── state machine ──────────────────────────────────────────────────────────

/// A board that keeps answering STATUS never produces an event.
#[test]
fn healthy_board_never_alerts() {
    let (script, probe) = scripted();
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    for tick in 0..10 {
        assert!(watch.tick_at(tick * 1_000));
        assert!(watch.drain_events().is_empty(), "tick {tick}");
    }
    assert_eq!(
        script.lock().unwrap().call_count(),
        10,
        "one probe per tick"
    );
    assert_eq!(
        watch.states(),
        vec![("board-1".to_string(), RelayBoardState::Online)]
    );
}

/// Cold-start bad board: the very first probe is already silent, but the fault
/// is only reported after `confirmations` consecutive failures — and then
/// exactly once.
#[test]
fn silent_board_alerts_once_after_confirmations() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);

    // First failure: below the threshold, nothing is reported yet.
    assert!(watch.tick_at(0));
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );
    // Second consecutive failure: confirmed offline.
    assert!(watch.tick_at(1_000));
    let events = watch.drain_events();
    assert_eq!(events.len(), 1, "exactly one Offline: {events:?}");
    assert_eq!(events[0].board_id, "board-1");
    assert_eq!(events[0].name, "继电器板 board-1");
    assert_eq!(events[0].state, RelayBoardState::Offline);
    assert_eq!(events[0].at_unix_ms, 1_000);
    assert!(
        events[0].detail.contains("无应答") && events[0].detail.contains("连续 2 次"),
        "detail must carry the reason and the confirmation count: {}",
        events[0].detail
    );
    // Still offline, but the reminder interval has not elapsed: no repeat.
    assert!(watch.tick_at(2_000));
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );
    assert_eq!(
        watch.states(),
        vec![("board-1".to_string(), RelayBoardState::Offline)]
    );
}

/// A vanished port is a fault as well; it needs the same confirmations.
#[test]
fn missing_port_alerts_after_confirmations() {
    let (script, probe) = scripted();
    script.lock().unwrap().set(
        "board-1",
        RelayProbeSample::PortMissing("串口未找到".into()),
    );
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    watch.tick_at(0);
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );
    watch.tick_at(1_000);
    let events = watch.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].state, RelayBoardState::Offline);
    assert!(
        events[0].detail.contains("串口未找到"),
        "{}",
        events[0].detail
    );
}

/// A busy port (another process, another instance) says nothing about the
/// board: it is reported as `Unobservable` once and never as `Offline`.
#[test]
fn busy_port_is_unobservable_and_never_offline() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::PortBusy("被占用".into()));
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    let mut events = Vec::new();
    for tick in 0..6 {
        watch.tick_at(tick * 1_000);
        events.extend(watch.drain_events());
    }
    assert_eq!(events.len(), 1, "one transition event: {events:?}");
    assert_eq!(events[0].state, RelayBoardState::Unobservable);
    assert_eq!(
        watch.states(),
        vec![("board-1".to_string(), RelayBoardState::Unobservable)]
    );
}

/// A press-and-hold test clears the failure streak: two failures that are
/// interrupted by a gated probe must not become offline.
#[test]
fn gated_probe_clears_the_failure_streak() {
    let (script, probe) = scripted();
    {
        let mut script = script.lock().unwrap();
        script.set("board-1", RelayProbeSample::Silent);
    }
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);

    watch.tick_at(0); // failure 1 — below the threshold
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Gated("按住测试中".into()));
    watch.tick_at(1_000); // gated → streak cleared, and no notice
    assert!(
        watch.drain_events().is_empty(),
        "a gated probe must not produce a per-board notice"
    );

    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    watch.tick_at(2_000); // failure 1 again, not 3
    assert!(
        watch.drain_events().is_empty(),
        "the streak must restart after a gated probe"
    );
    watch.tick_at(3_000); // failure 2 → offline
    let events = watch.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].state, RelayBoardState::Offline);
}

/// A hold test holds the one service-wide relay write gate, so it gates the
/// probe of every watched board. None of them may be reported: the hold is a
/// normal, transient condition of a *different* board, and one "cannot
/// observe" toast per board would be pure noise (and misattributed).
#[test]
fn a_hold_test_on_one_board_never_notifies_any_board() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-a", RelayProbeSample::Gated("按住测试中".into()));
    let watch = watch(probe);
    watch.sync_boards(&[board("board-a", "COM25"), board("board-b", "CH1")]);
    // Both boards see `Gated` (board-b has no scripted sample, so emulate the
    // same condition it would observe while the gate is held).
    script.lock().unwrap().set(
        "board-b",
        RelayProbeSample::Gated("继电器按住测试进行中".into()),
    );
    for tick in 0..10 {
        watch.tick_at(tick * 1_000);
        assert!(
            watch.drain_events().is_empty(),
            "tick {tick}: a gated probe must stay silent"
        );
    }
    // Diagnostics still show the blind spot.
    assert_eq!(
        watch.states(),
        vec![
            ("board-a".to_string(), RelayBoardState::Unobservable),
            ("board-b".to_string(), RelayBoardState::Unobservable),
        ]
    );
}

/// A board that was reported offline stays recoverable across a hold test: the
/// gated probe is silent, and the next answer still emits exactly one
/// `Recovered` (the offline notice was never retracted).
#[test]
fn an_offline_board_survives_a_hold_test_and_still_reports_recovered() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    watch.tick_at(0);
    watch.tick_at(1_000);
    assert_eq!(watch.drain_events().len(), 1, "offline is reported");

    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Gated("按住测试中".into()));
    watch.tick_at(2_000);
    assert!(
        watch.drain_events().is_empty(),
        "the hold must not emit an unobservable notice"
    );

    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Answered(false));
    watch.tick_at(3_000);
    let events = watch.drain_events();
    assert_eq!(events.len(), 1, "one Recovered: {events:?}");
    assert_eq!(events[0].state, RelayBoardState::Recovered);
}

/// Re-confirming an outage that was already reported must not re-alert ahead of
/// the reminder interval, even though a hold test cleared the failure streak in
/// between.
#[test]
fn re_confirmed_outage_after_a_hold_waits_for_the_reminder_interval() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    watch.tick_at(0);
    watch.tick_at(1_000); // offline reported at t=1_000
    assert_eq!(watch.drain_events().len(), 1);

    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Gated("按住测试中".into()));
    watch.tick_at(2_000); // silent, streak cleared
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );

    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    watch.tick_at(3_000);
    watch.tick_at(4_000); // confirmed again, but the user already knows
    assert!(
        watch.drain_events().is_empty(),
        "an already reported outage must not re-alert before the reminder interval"
    );

    watch.tick_at(61_000); // 1_000 + 60_000 → reminder
    let events = watch.drain_events();
    assert_eq!(events.len(), 1, "one reminder: {events:?}");
    assert_eq!(events[0].state, RelayBoardState::Offline);
}

/// A gated probe never becomes offline, no matter how long the test runs.
#[test]
fn hold_test_never_produces_an_offline_event() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Gated("按住测试中".into()));
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    let mut offline = 0;
    for tick in 0..20 {
        watch.tick_at(tick * 1_000);
        offline += watch
            .drain_events()
            .iter()
            .filter(|event| event.state == RelayBoardState::Offline)
            .count();
    }
    assert_eq!(offline, 0);
}

/// The board answering again produces exactly one `Recovered`, and only after
/// the offline state was actually reported.
#[test]
fn answered_probe_recovers_an_offline_board_once() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    watch.tick_at(0);
    watch.tick_at(1_000);
    assert_eq!(watch.drain_events().len(), 1);

    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Answered(false));
    watch.tick_at(2_000);
    let events = watch.drain_events();
    assert_eq!(events.len(), 1, "one Recovered: {events:?}");
    assert_eq!(events[0].state, RelayBoardState::Recovered);
    assert_eq!(events[0].at_unix_ms, 2_000);
    assert_eq!(
        watch.states(),
        vec![("board-1".to_string(), RelayBoardState::Online)]
    );
    // Staying online emits nothing more.
    watch.tick_at(3_000);
    watch.tick_at(4_000);
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );
}

/// While offline, the alert is repeated at most every `remind_interval_ms`.
#[test]
fn offline_reminders_respect_the_remind_interval() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    watch.tick_at(0);
    watch.tick_at(1_000); // confirmed offline at t=1000
    assert_eq!(watch.drain_events().len(), 1);

    // Well inside the reminder window: no repeats.
    for tick in 2..30 {
        watch.tick_at(tick * 1_000);
        assert!(watch.drain_events().is_empty(), "tick {tick}");
    }
    // Reminder window elapsed (1000 + 60_000): one repeat.
    watch.tick_at(61_000);
    let events = watch.drain_events();
    assert_eq!(events.len(), 1, "one reminder: {events:?}");
    assert_eq!(events[0].state, RelayBoardState::Offline);
    // Immediately after the repeat: quiet again.
    watch.tick_at(62_000);
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );
}

/// Boards are monitored independently: one broken board must not mask another.
#[test]
fn multiple_boards_are_monitored_independently() {
    let (script, probe) = scripted();
    {
        let mut script = script.lock().unwrap();
        script.set("board-a", RelayProbeSample::Silent);
        script.set("board-b", RelayProbeSample::Answered(true));
        script.set("board-c", RelayProbeSample::PortBusy("占用".into()));
    }
    let watch = watch(probe);
    watch.sync_boards(&[
        board("board-a", "COM25"),
        board("board-b", "COM26"),
        board("board-c", "COM27"),
    ]);
    watch.tick_at(0);
    // Only board-c produced a transition so far (unobservable).
    let events = watch.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].board_id, "board-c");
    watch.tick_at(1_000);
    let events = watch.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].board_id, "board-a");
    assert_eq!(events[0].state, RelayBoardState::Offline);
    assert_eq!(
        watch.states(),
        vec![
            ("board-a".to_string(), RelayBoardState::Offline),
            ("board-b".to_string(), RelayBoardState::Online),
            ("board-c".to_string(), RelayBoardState::Unobservable),
        ]
    );
}

/// Boards that were never passed to `sync_boards` are not probed, and a board
/// removed from the set stops being probed.
#[test]
fn boards_outside_the_watch_set_are_never_probed() {
    let (script, probe) = scripted();
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    watch.tick_at(0);
    assert_eq!(script.lock().unwrap().calls(), vec!["board-1".to_string()]);

    // A different board is not in the set: replacing the set drops board-1.
    watch.sync_boards(&[board("board-2", "COM26")]);
    watch.tick_at(1_000);
    assert_eq!(
        script.lock().unwrap().calls(),
        vec!["board-1".to_string(), "board-2".to_string()]
    );
    // board-1 is gone from the diagnostics as well.
    assert_eq!(
        watch.states(),
        vec![("board-2".to_string(), RelayBoardState::Online)]
    );
}

/// Decisions 5: a board without a configured port is not part of the watch
/// set at all — no probe, no event, no state.
#[test]
fn unconfigured_boards_are_not_watched() {
    let (script, probe) = scripted();
    let watch = watch(probe);
    watch.sync_boards(&[board("empty-node", ""), board("blank-node", "   "), {
        let mut pdu = board("pdu", "192.168.1.10");
        pdu.driver = "snmp-pdu".into();
        pdu
    }]);
    assert!(
        !watch.tick_at(0),
        "nothing is due because nothing is watched"
    );
    assert_eq!(script.lock().unwrap().calls(), [] as [String; 0]);
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );
    assert_eq!(watch.states(), [] as [(String, RelayBoardState); 0]);
}

/// A board whose port is reconfigured starts over; a rename keeps the state.
#[test]
fn resync_keeps_state_but_restarts_after_an_endpoint_change() {
    let (script, probe) = scripted();
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    let watch = watch(probe);
    watch.sync_boards(&[board("board-1", "COM25")]);
    watch.tick_at(0);
    watch.tick_at(1_000);
    assert_eq!(watch.drain_events().len(), 1);
    assert_eq!(
        watch.states(),
        vec![("board-1".to_string(), RelayBoardState::Offline)]
    );

    // Rename only: the board is the same physical endpoint, state survives.
    let mut renamed = board("board-1", "COM25");
    renamed.name = "改名后的板卡".into();
    watch.sync_boards(&[renamed]);
    assert_eq!(
        watch.states(),
        vec![("board-1".to_string(), RelayBoardState::Offline)],
        "a rename must not restart monitoring"
    );

    // New port: fresh entry, due immediately, one failure is not enough.
    watch.sync_boards(&[board("board-1", "COM26")]);
    assert_eq!(
        watch.states(),
        vec![("board-1".to_string(), RelayBoardState::Online)]
    );
    assert!(watch.tick_at(2_000));
    assert!(
        watch.drain_events().is_empty(),
        "the confirmation streak restarts with the new endpoint"
    );
}

/// `sync_boards` is called every 50 ms tick: it must stay cheap and instant
/// even while a probe blocks for a hung board.
#[test]
fn sync_boards_returns_immediately_while_a_probe_is_running() {
    let (script, probe) = scripted_with_delay(Duration::from_millis(200));
    script
        .lock()
        .unwrap()
        .set("board-1", RelayProbeSample::Silent);
    let watch = Arc::new(watch(probe));
    watch.sync_boards(&[board("board-1", "COM25")]);

    let worker_watch = watch.clone();
    let worker = std::thread::spawn(move || worker_watch.tick_at(0));
    // Give the worker time to enter the 200 ms probe.
    std::thread::sleep(Duration::from_millis(80));
    let started = Instant::now();
    watch.sync_boards(&[board("board-1", "COM25")]); // identical set
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(20),
        "sync_boards waited for the probe: {elapsed:?}"
    );
    // A changed set is a pure map swap as well (no I/O under the lock).
    let started = Instant::now();
    watch.sync_boards(&[board("board-1", "COM25"), board("board-2", "COM26")]);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(20),
        "sync_boards waited for the probe: {elapsed:?}"
    );
    assert!(worker.join().unwrap(), "the worker did probe");
    // The set changed mid-probe, so that sample was discarded; the new set is
    // due immediately and produces no stale event.
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );
    assert!(watch.tick_at(1_000));
    assert_eq!(
        script.lock().unwrap().calls(),
        vec![
            "board-1".to_string(),
            "board-1".to_string(),
            "board-2".to_string()
        ]
    );
}

// ── ControlService side: classification, gate, no side effects ─────────────

/// Mock relay backend: records probes and answers with a scripted result.
struct MockRelayBackend {
    probe_result: Mutex<DeviceResult<Option<bool>>>,
    probe_calls: Mutex<Vec<(String, u8)>>,
    power_calls: Mutex<Vec<bool>>,
}

impl MockRelayBackend {
    fn new(probe_result: DeviceResult<Option<bool>>) -> Arc<Self> {
        Arc::new(Self {
            probe_result: Mutex::new(probe_result),
            probe_calls: Mutex::new(Vec::new()),
            power_calls: Mutex::new(Vec::new()),
        })
    }

    fn set_probe_result(&self, result: DeviceResult<Option<bool>>) {
        *self.probe_result.lock().unwrap() = result;
    }

    fn probe_calls(&self) -> Vec<(String, u8)> {
        self.probe_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ControlBackend for MockRelayBackend {
    async fn power(&self, _target: &ControlTarget, on: bool) -> DeviceResult<()> {
        self.power_calls.lock().unwrap().push(on);
        Ok(())
    }

    async fn reset(&self, _target: &ControlTarget) -> DeviceResult<()> {
        Ok(())
    }

    async fn power_key(&self, _target: &ControlTarget, _long_press: bool) -> DeviceResult<()> {
        Ok(())
    }

    async fn channel_status(&self, _address: &str) -> DeviceResult<Vec<(u8, bool)>> {
        Err(DeviceError::Unsupported)
    }

    async fn channel_test(&self, _address: &str, _channel: u8) -> DeviceResult<ChannelTestReport> {
        Err(DeviceError::Unsupported)
    }

    async fn probe_channel(&self, address: &str, channel: u8) -> DeviceResult<Option<bool>> {
        self.probe_calls
            .lock()
            .unwrap()
            .push((address.to_string(), channel));
        self.probe_result.lock().unwrap().clone()
    }
}

fn dut_with_serial_relay(id: &str) -> DutConfig {
    DutConfig {
        id: id.into(),
        name: "RK3576".into(),
        serial: SerialConfig {
            host: "local".into(),
            port: "COM3".into(),
            baud: 115_200,
            format: "8N1".into(),
            reference_log: None,
            reference_similarity: None,
            split_logs: false,
        },
        adb: duthub_core::model::AdbConfig::default(),
        relay: RelayConfig::UsbSerial {
            host: "local".into(),
            node: "COM25".into(),
            driver: "CH340".into(),
            channels: RelayChannels {
                power: Some(4),
                reset: Some(1),
                recovery: None,
                maskrom: None,
                power_key: Some(2),
            },
        },
        mcp: McpConfig::StreamableHttp {
            url: "http://127.0.0.1:8787/mcp".into(),
        },
        a2a: duthub_core::model::DutA2aConfig::default(),
        flash: FlashConfig::default(),
        media: duthub_core::model::DutMediaConfig::default(),
        heartbeat: duthub_core::model::HeartbeatConfig::default(),
    }
}

fn probe_control(backend: Arc<MockRelayBackend>) -> ControlService {
    let backend: Arc<dyn ControlBackend> = backend;
    ControlService::new(Some(backend), Arc::new(ApprovalManager::new()))
}

/// An answering board is `Answered`, a silent one is `Silent`, and neither
/// needs a registered DUT: the monitor is board-level.
#[test]
fn probe_reports_answered_and_silent_without_a_registered_dut() {
    let backend = MockRelayBackend::new(Ok(Some(true)));
    let control = probe_control(backend.clone());
    assert_eq!(
        control.probe_board_status("COM25", 1),
        RelayProbeSample::Answered(true)
    );

    backend.set_probe_result(Ok(Some(false)));
    assert_eq!(
        control.probe_board_status("COM25", 3),
        RelayProbeSample::Answered(false)
    );

    backend.set_probe_result(Ok(None));
    assert_eq!(
        control.probe_board_status("COM25", 1),
        RelayProbeSample::Silent
    );

    assert_eq!(
        backend.probe_calls(),
        vec![
            ("COM25".to_string(), 1),
            ("COM25".to_string(), 3),
            ("COM25".to_string(), 1),
        ],
        "the probe must read exactly the channel it was asked for"
    );
}

/// Only a vanished port or a silent board is a fault; everything that merely
/// hides the board is `Unobservable`.
#[test]
fn probe_classifies_backend_failures_without_false_offline() {
    let backend = MockRelayBackend::new(Err(DeviceError::NotFound));
    let control = probe_control(backend.clone());
    for (error, expected) in [
        (DeviceError::NotFound, "port-missing"),
        (DeviceError::Disconnected, "port-missing"),
        (DeviceError::Timeout, "port-missing"),
        (DeviceError::PermissionDenied, "port-busy"),
        (DeviceError::Busy, "port-busy"),
        (DeviceError::Lease("人工接管".into()), "gated"),
        (DeviceError::Unsupported, "unsupported"),
        (DeviceError::Invalid("通道 0 无效".into()), "unsupported"),
    ] {
        backend.set_probe_result(Err(error.clone()));
        let sample = control.probe_board_status("COM25", 1);
        let bucket = match sample {
            RelayProbeSample::PortMissing(_) => "port-missing",
            RelayProbeSample::PortBusy(_) => "port-busy",
            RelayProbeSample::Gated(_) => "gated",
            RelayProbeSample::Unsupported(_) => "unsupported",
            other => panic!("{error:?} mapped to unexpected sample {other:?}"),
        };
        assert_eq!(bucket, expected, "{error:?} classified as {bucket}");
    }
}

/// A probe is not a control action: no approval, no lease, no `[mark]`, and no
/// DUT ends up in `NeedsInspection`.
#[test]
fn probe_has_no_control_side_effects() {
    let backend = MockRelayBackend::new(Err(DeviceError::Disconnected));
    let lease = Arc::new(ControlLeaseManager::new(None));
    let control = probe_control(backend.clone()).with_lease_authority(lease.clone());
    let marks: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = marks.clone();
    assert!(
        control.set_log_sink(Arc::new(move |dut, label| {
            sink.lock()
                .unwrap()
                .push((dut.to_string(), label.to_string()));
        })),
        "first sink registration must succeed"
    );

    // A failing probe for a DUT that is currently bound.
    control.register_dut(dut_with_serial_relay("dut-a"));
    assert!(matches!(
        control.probe_board_status("COM25", 1),
        RelayProbeSample::PortMissing(_)
    ));
    assert!(
        marks.lock().unwrap().is_empty(),
        "probe must not write [mark]"
    );
    let status = lease.status("dut-a");
    assert_eq!(
        status.phase,
        LeasePhase::Free,
        "probe must not hold a lease"
    );
    assert!(status.protected_op.is_none());
    assert!(status.holder.is_none());

    // Never inspected, and a normal write path still works afterwards.
    let token_absent = control.channel_status("dut-a");
    assert!(matches!(
        token_absent,
        Err(duthub_device::control::ControlEntryError::Device(
            DeviceError::Unsupported
        ))
    ));
    assert_eq!(lease.status("dut-a").phase, LeasePhase::Free);
    assert!(backend.power_calls.lock().unwrap().is_empty());
}

/// While a press-and-hold test owns the relay write gate the probe is `Gated`:
/// it neither waits nor touches the port, and never reports the board offline.
#[test]
fn probe_is_gated_during_a_press_and_hold_test() {
    let backend = MockRelayBackend::new(Ok(Some(false)));
    let control = Arc::new(probe_control(backend.clone()));
    let dut = dut_with_serial_relay("dut-hold");
    control.register_dut(dut.clone());
    let (release, released) = tokio::sync::oneshot::channel();
    let worker_control = control.clone();
    let worker = std::thread::spawn(move || worker_control.hold_channel(&dut, 1, released));
    let deadline = Instant::now() + Duration::from_secs(5);
    while backend.power_calls.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "hold test never closed the relay"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let sample = control.probe_board_status("COM25", 1);
    assert!(
        matches!(sample, RelayProbeSample::Gated(_)),
        "a held gate must yield Gated, got {sample:?}"
    );
    assert!(
        backend.probe_calls().is_empty(),
        "a gated probe must not touch the port"
    );

    drop(release);
    worker.join().unwrap().unwrap();
    // After the hold test the gate is free again.
    assert_eq!(
        control.probe_board_status("COM25", 1),
        RelayProbeSample::Answered(false)
    );
}

/// Unconfigured values are refused before any I/O instead of being probed.
#[test]
fn probe_refuses_unconfigured_nodes_and_missing_backends() {
    let backend = MockRelayBackend::new(Ok(Some(true)));
    let control = probe_control(backend.clone());
    for node in ["", "   "] {
        assert!(matches!(
            control.probe_board_status(node, 1),
            RelayProbeSample::Unsupported(_)
        ));
    }
    let without_backend = ControlService::new(None, Arc::new(ApprovalManager::new()));
    assert!(matches!(
        without_backend.probe_board_status("COM25", 1),
        RelayProbeSample::Unsupported(_)
    ));
    assert_eq!(backend.probe_calls(), [] as [(String, u8); 0]);
}

// ── ControlRelayProbe: board → channel mapping ─────────────────────────────

/// The adapter probes the channel the UI mapped for that board.
#[test]
fn control_relay_probe_uses_the_mapped_channel() {
    let backend = MockRelayBackend::new(Ok(Some(true)));
    let control = Arc::new(probe_control(backend.clone()));
    let probe = ControlRelayProbe::new(control);
    probe.set_channels(&[("board-1".to_string(), 3)]);
    assert_eq!(
        probe.probe_board(&board("board-1", "COM25")),
        RelayProbeSample::Answered(true)
    );
    assert_eq!(backend.probe_calls(), vec![("COM25".to_string(), 3)]);
}

/// A board bound to a DUT but without any assigned channel cannot be probed:
/// `Unobservable` (toast only), never `Offline`.
#[test]
fn control_relay_probe_without_a_channel_is_unobservable() {
    let backend = MockRelayBackend::new(Err(DeviceError::NotFound));
    let control = Arc::new(probe_control(backend.clone()));
    let probe = Arc::new(ControlRelayProbe::new(control));
    let watch = RelayWatchService::new(probe.clone()).with_config(config());
    watch.sync_boards(&[board("board-1", "COM25")]);
    for tick in 0..5 {
        watch.tick_at(tick * 1_000);
    }
    let events = watch.drain_events();
    assert_eq!(events.len(), 1, "one transition event: {events:?}");
    assert_eq!(events[0].state, RelayBoardState::Unobservable);
    assert!(events[0].detail.contains("未分配"), "{}", events[0].detail);
    assert!(
        backend.probe_calls().is_empty(),
        "a board without a channel must not touch the port"
    );

    // Once a channel is assigned the board is probed for real.
    probe.set_channels(&[("board-1".to_string(), 2)]);
    watch.tick_at(5_000);
    assert_eq!(backend.probe_calls(), vec![("COM25".to_string(), 2)]);
}

/// The end-to-end shape: a vanished port reaches `Offline` through the adapter
/// and the mock backend, and a later answer recovers it.
#[test]
fn adapter_and_mock_backend_drive_the_full_transition() {
    let backend = MockRelayBackend::new(Err(DeviceError::NotFound));
    let control = Arc::new(probe_control(backend.clone()));
    let probe = Arc::new(ControlRelayProbe::new(control));
    probe.set_channels(&[("board-1".to_string(), 1)]);
    let watch = RelayWatchService::new(probe).with_config(config());
    watch.sync_boards(&[board("board-1", "COM25")]);

    watch.tick_at(0);
    assert_eq!(
        watch.drain_events(),
        [] as [duthub_device::relay_watch::RelayBoardEvent; 0]
    );
    watch.tick_at(1_000);
    let events = watch.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].state, RelayBoardState::Offline);

    backend.set_probe_result(Ok(Some(false)));
    watch.tick_at(2_000);
    let events = watch.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].state, RelayBoardState::Recovered);
}
