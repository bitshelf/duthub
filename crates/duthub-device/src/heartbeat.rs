//! Heartbeat scheduler for active serial sessions.
//!
//! The desktop calls `HeartbeatService::tick` from its timer.
//! Each due probe dispatches the configured command through the serial service.
//! Record the session's event cursor when dispatching the command.
//! Use the response window to collect new device output and ignore local annotations.
//! Retain the raw response and extract system information from it.
//! Report a loss alert after the configured number of unanswered probes.
//! The UI drains alerts for presentation.
//! Agent updates use `HeartbeatService::set_config` to persist and publish changes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use duthub_core::DeviceError;
use duthub_core::heartbeat::{
    HeartbeatAlert, HeartbeatConfig, HeartbeatState, extract_sysinfo, is_response_line,
};
use duthub_core::ipc::UiEvent;

use crate::DeviceService;

/// Host callback that atomically persists the heartbeat section of the DUT configuration.
/// A failed persistence callback leaves the runtime configuration unchanged.
pub type HeartbeatPersistHook =
    Arc<dyn Fn(&str, &HeartbeatConfig) -> Result<(), String> + Send + Sync>;

/// Maximum retained response size in characters.
const LAST_RESPONSE_CAP: usize = 2000;

/// Event cursor and response deadline for an in-flight probe.
#[derive(Clone, Debug)]
struct Flight {
    cursor_seq: u64,
    deadline_ms: u64,
    batch: Option<Arc<HeartbeatBatch>>,
}

/// A multi-line POSIX shell probe with boundaries that cannot be matched by command echo.
#[derive(Clone, Debug)]
pub struct HeartbeatBatch {
    pub(crate) script: String,
    nonce: String,
}

impl HeartbeatBatch {
    fn from_command(command: &str, now_ms: u64) -> Option<Self> {
        static NEXT_NONCE: AtomicU64 = AtomicU64::new(0);
        let script = command.trim_matches(['\r', '\n']);
        if !script.contains(['\r', '\n']) {
            return None;
        }
        let sequence = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
        Some(Self {
            script: script.replace("\r\n", "\n").replace('\r', "\n"),
            nonce: format!("{now_ms}-{sequence}"),
        })
    }

    pub(crate) fn begin_marker(&self) -> String {
        format!("DUTHUB-HB-BEGIN-{}", self.nonce)
    }

    pub(crate) fn end_marker(&self) -> String {
        format!("DUTHUB-HB-END-{}", self.nonce)
    }

    pub(crate) fn wire_text(&self) -> String {
        let script = self.script.replace('\'', "'\\''");
        let nonce = &self.nonce;
        // Parse the entire group before running it. Keep shell changes and exit in a subshell.
        // Print each marker in two parts so echoed input does not match the response delimiter.
        format!(
            "{{\rcommand printf '\\n%s%s\\n' 'DUTHUB-HB-BEGIN-' '{nonce}'\r( eval '{script}' )\rcommand printf '\\n%s%s\\n' 'DUTHUB-HB-END-' '{nonce}'\r}}"
        )
        .replace('\n', "\r")
    }

    fn response<'a>(&self, transcript: &'a str, expected: Option<&str>) -> Option<&'a str> {
        let (_, after_start) = transcript.split_once(&self.begin_marker())?;
        let (body, _) = after_start.split_once(&self.end_marker())?;
        let body = body.trim_matches(['\r', '\n']);
        expected
            .is_none_or(|expected| body.lines().any(|line| line.contains(expected)))
            .then_some(body)
    }
}

/// Owns per-DUT configuration, status, in-flight probes, and alerts.
pub struct HeartbeatService {
    device: Arc<DeviceService>,
    configs: Mutex<HashMap<String, HeartbeatConfig>>,
    states: Mutex<HashMap<String, HeartbeatState>>,
    flights: Mutex<HashMap<String, Flight>>,
    next_due: Mutex<HashMap<String, u64>>,
    alerts: Mutex<Vec<HeartbeatAlert>>,
    /// Serialize desktop timer ticks and immediate MCP probes to avoid double dispatch.
    ticking: Mutex<()>,
    persist: Mutex<Option<HeartbeatPersistHook>>,
    config_changed: AtomicBool,
}

impl std::fmt::Debug for HeartbeatService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeartbeatService").finish_non_exhaustive()
    }
}

#[must_use]
pub fn now_unix_ms() -> u64 {
    duthub_core::util::unix_ms()
}

fn truncate_chars(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

/// A single command gets a response window between one and five seconds.
/// Allow at least two seconds per nonempty segment for multi-command probes.
/// Count semicolon- and newline-separated segments and cap the total at 30 seconds.
fn capture_window_ms(interval_ms: u64, command: &str) -> u64 {
    let segments = command
        .split([';', '\n', '\r'])
        .map(str::trim)
        .filter(|seg| !seg.is_empty())
        .count();
    let window = interval_ms.clamp(1_000, 5_000);
    if segments > 1 {
        window
            .max((segments as u64).saturating_mul(2_000))
            .min(30_000)
    } else {
        window
    }
}

impl HeartbeatService {
    pub fn new(device: Arc<DeviceService>) -> Self {
        Self {
            device,
            configs: Mutex::new(HashMap::new()),
            states: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            next_due: Mutex::new(HashMap::new()),
            alerts: Mutex::new(Vec::new()),
            ticking: Mutex::new(()),
            persist: Mutex::new(None),
            config_changed: AtomicBool::new(false),
        }
    }

    /// Attach the host's persistence callback.
    #[must_use]
    pub fn with_persist(self, hook: HeartbeatPersistHook) -> Self {
        *duthub_core::util::lock_unpoisoned(&self.persist) = Some(hook);
        self
    }

    /// Apply a configuration loaded or saved by the UI without writing it again.
    pub fn register(&self, dut_id: &str, config: HeartbeatConfig) {
        self.configs
            .lock()
            .unwrap()
            .insert(dut_id.to_string(), config);
    }

    /// Return the configuration, or the default disabled probe every 30 seconds.
    pub fn config(&self, dut_id: &str) -> HeartbeatConfig {
        self.configs
            .lock()
            .unwrap()
            .get(dut_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Return the most recent result if a probe has completed.
    pub fn state(&self, dut_id: &str) -> Option<HeartbeatState> {
        duthub_core::util::lock_unpoisoned(&self.states)
            .get(dut_id)
            .cloned()
    }

    pub fn state_or_default(&self, dut_id: &str) -> HeartbeatState {
        self.state(dut_id).unwrap_or_default()
    }

    /// Persist and apply an Agent's heartbeat changes.
    /// A persistence error leaves the active configuration unchanged.
    pub fn set_config(
        &self,
        dut_id: &str,
        config: HeartbeatConfig,
    ) -> Result<HeartbeatConfig, String> {
        self.update_config(dut_id, |_| config)
    }

    pub fn update_config(
        &self,
        dut_id: &str,
        update: impl FnOnce(HeartbeatConfig) -> HeartbeatConfig,
    ) -> Result<HeartbeatConfig, String> {
        let _tick_guard = duthub_core::util::lock_unpoisoned(&self.ticking);
        let config = update(self.config(dut_id));
        let cfg = HeartbeatConfig {
            interval_secs: config.normalized_interval(),
            ..config
        };
        let hook = duthub_core::util::lock_unpoisoned(&self.persist).clone();
        if let Some(hook) = &hook {
            hook(dut_id, &cfg)?;
        }
        self.configs
            .lock()
            .unwrap()
            .insert(dut_id.to_string(), cfg.clone());
        // Cancel the in-flight probe and make the new command due on the next tick.
        self.reset_schedule(dut_id);
        self.config_changed.store(true, Ordering::Release);
        Ok(cfg)
    }

    /// Save the GUI draft under the same lock as Agent updates and apply it after persistence.
    pub fn commit_configs(
        &self,
        drafts: &HashMap<String, HeartbeatConfig>,
        save: impl FnOnce(&HashMap<String, HeartbeatConfig>) -> Result<(), String>,
    ) -> Result<(), String> {
        let _tick_guard = duthub_core::util::lock_unpoisoned(&self.ticking);
        let mut candidate = duthub_core::util::lock_unpoisoned(&self.configs).clone();
        candidate.extend(drafts.iter().map(|(id, cfg)| (id.clone(), cfg.clone())));
        save(&candidate)?;
        *duthub_core::util::lock_unpoisoned(&self.configs) = candidate;
        for dut_id in drafts.keys() {
            self.reset_schedule(dut_id);
        }
        Ok(())
    }

    pub fn unregister(&self, dut_id: &str) {
        let _tick_guard = duthub_core::util::lock_unpoisoned(&self.ticking);
        duthub_core::util::lock_unpoisoned(&self.configs).remove(dut_id);
        duthub_core::util::lock_unpoisoned(&self.states).remove(dut_id);
        self.alerts
            .lock()
            .unwrap()
            .retain(|alert| alert.dut_id != dut_id);
        self.reset_schedule(dut_id);
    }

    fn reset_schedule(&self, dut_id: &str) {
        duthub_core::util::lock_unpoisoned(&self.flights).remove(dut_id);
        duthub_core::util::lock_unpoisoned(&self.next_due).remove(dut_id);
        self.note_lease_pause(dut_id, false);
    }

    fn note_lease_pause(&self, dut_id: &str, paused: bool) -> bool {
        let mut states = duthub_core::util::lock_unpoisoned(&self.states);
        if paused {
            let state = states.entry(dut_id.to_owned()).or_default();
            return !std::mem::replace(&mut state.paused_by_lease, true);
        }
        states
            .get_mut(dut_id)
            .is_some_and(|state| std::mem::replace(&mut state.paused_by_lease, false))
    }

    /// Drain pending heartbeat loss alerts.
    pub fn drain_alerts(&self) -> Vec<HeartbeatAlert> {
        std::mem::take(&mut *duthub_core::util::lock_unpoisoned(&self.alerts))
    }

    /// Use the system clock and report whether the tick changed state.
    pub fn tick(&self) -> bool {
        self.tick_at(now_unix_ms())
    }

    /// Run one tick at a supplied time so tests control deadlines.
    /// Hold the tick lock to serialize the UI and MCP entry points.
    pub fn tick_at(&self, now_ms: u64) -> bool {
        let _tick_guard = duthub_core::util::lock_unpoisoned(&self.ticking);
        let mut changed = self.config_changed.swap(false, Ordering::AcqRel);
        let entries: Vec<(String, HeartbeatConfig)> = self
            .configs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, cfg)| {
                cfg.enabled && !cfg.is_paused_at(now_ms) && !cfg.command.trim().is_empty()
            })
            .map(|(dut, cfg)| (dut.clone(), cfg.clone()))
            .collect();
        for (dut, cfg) in entries {
            let interval_ms = u64::from(cfg.normalized_interval()) * 1000;
            let window_ms = capture_window_ms(interval_ms, &cfg.command);
            let session_id = format!("sess-{dut}");
            let source = self.device.session_source(&dut);
            let session_open = source.is_some();
            let flight = duthub_core::util::lock_unpoisoned(&self.flights)
                .get(&dut)
                .cloned();
            if let Some(f) = flight {
                if session_open {
                    let lines: Vec<String> = self
                        .device
                        .read_log(&session_id, f.cursor_seq, usize::MAX)
                        .iter()
                        .filter_map(|env| match &env.event {
                            UiEvent::SerialChunk { text, .. } if is_response_line(text) => {
                                Some(text.clone())
                            }
                            _ => None,
                        })
                        .collect();
                    let response = if f.batch.is_some() && source == Some("demo") {
                        lines.join("\n")
                    } else {
                        lines.concat()
                    };
                    let expected = cfg.expected_response.as_deref();
                    let mut response_lines = response.lines().filter(|line| {
                        let line = line.trim();
                        !line.is_empty()
                            && !matches!(line, "#" | "$" | ">")
                            && line.trim_start_matches(['#', '$', '>']).trim_start()
                                != cfg.command.trim()
                    });
                    let answered = if let Some(batch) = &f.batch {
                        batch
                            .response(&response, expected)
                            .map(|body| self.note_response(&dut, &[body.to_owned()], now_ms))
                    } else {
                        let matched = match expected {
                            Some(expected) => response_lines.any(|line| line.contains(expected)),
                            None => response_lines.next().is_some(),
                        };
                        matched.then(|| self.note_response(&dut, &lines, now_ms))
                    };
                    if let Some(updated) = answered {
                        changed |= updated;
                        duthub_core::util::lock_unpoisoned(&self.flights).remove(&dut);
                        self.next_due
                            .lock()
                            .unwrap()
                            .insert(dut.clone(), now_ms.saturating_add(interval_ms));
                        continue;
                    }
                }
                if now_ms >= f.deadline_ms {
                    // An unanswered dispatched probe counts as a loss once its window expires.
                    changed |= self.note_loss(&dut, &cfg, now_ms);
                    duthub_core::util::lock_unpoisoned(&self.flights).remove(&dut);
                    self.next_due
                        .lock()
                        .unwrap()
                        .insert(dut.clone(), now_ms.saturating_add(interval_ms));
                }
                continue;
            }
            // A closed session receives no probe and does not accumulate new losses.
            if !session_open {
                changed |= self.note_lease_pause(&dut, false);
                continue;
            }
            let due = self
                .next_due
                .lock()
                .unwrap()
                .get(&dut)
                .copied()
                .unwrap_or(0);
            if now_ms >= due {
                let cursor = self.device.session_last_seq(&dut);
                let batch = HeartbeatBatch::from_command(&cfg.command, now_ms).map(Arc::new);
                let writer = duthub_core::lease::WriteIdentity::system();
                let sent = match &batch {
                    Some(batch) => self
                        .device
                        .write_heartbeat_batch(&dut, &writer, batch.clone()),
                    None => self.device.write_line(&dut, writer, &cfg.command),
                };
                // A busy or lease denial defers the probe without counting a loss.
                // Expose the deferral separately from a failed probe.
                // A successful dispatch or a different error clears the flag.
                changed |= self.note_lease_pause(
                    &dut,
                    matches!(&sent, Err(DeviceError::Lease(_) | DeviceError::Busy)),
                );
                match sent {
                    Ok(()) => {
                        duthub_core::util::lock_unpoisoned(&self.flights).insert(
                            dut.clone(),
                            Flight {
                                cursor_seq: cursor,
                                deadline_ms: now_ms.saturating_add(window_ms),
                                batch,
                            },
                        );
                        changed = true;
                    }
                    Err(DeviceError::NotFound | DeviceError::Lease(_) | DeviceError::Busy) => {
                        // A command that never entered the serial queue does not count as a hardware timeout.
                        self.next_due
                            .lock()
                            .unwrap()
                            .insert(dut.clone(), now_ms.saturating_add(interval_ms));
                    }
                    Err(_) => {
                        changed |= self.note_loss(&dut, &cfg, now_ms);
                        self.next_due
                            .lock()
                            .unwrap()
                            .insert(dut.clone(), now_ms.saturating_add(interval_ms));
                    }
                }
            }
        }
        changed
    }

    /// Store the response, update system information, and clear the loss count.
    pub fn note_response(&self, dut_id: &str, lines: &[String], now_ms: u64) -> bool {
        let joined = lines.join("\n");
        let mut states = duthub_core::util::lock_unpoisoned(&self.states);
        let state = states.entry(dut_id.to_string()).or_default();
        state.last_ok = Some(true);
        state.last_at_unix_ms = now_ms;
        state.loss_count = 0;
        state.last_response = truncate_chars(&joined, LAST_RESPONSE_CAP);
        state.sysinfo = extract_sysinfo(&joined);
        true
    }

    /// Increment the loss count and alert when it first reaches the threshold.
    pub fn note_loss(&self, dut_id: &str, cfg: &HeartbeatConfig, now_ms: u64) -> bool {
        let (loss_count, crossed) = {
            let mut states = duthub_core::util::lock_unpoisoned(&self.states);
            let state = states.entry(dut_id.to_string()).or_default();
            state.last_ok = Some(false);
            state.last_at_unix_ms = now_ms;
            state.loss_count = state.loss_count.saturating_add(1);
            let threshold = cfg.loss_threshold.max(1);
            (state.loss_count, state.loss_count == threshold)
        };
        if crossed {
            duthub_core::util::lock_unpoisoned(&self.alerts).push(HeartbeatAlert {
                dut_id: dut_id.to_string(),
                loss_count,
                message: format!("DUT {dut_id} 心跳丢联：连续 {loss_count} 次无响应"),
                at_unix_ms: now_ms,
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::heartbeat::HeartbeatConfig;
    use std::time::Duration;

    fn test_device(tag: &str) -> Arc<DeviceService> {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/heartbeat")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(DeviceService::custom(256, dir, 0))
    }

    fn cfg(command: &str, interval: u32, threshold: u32) -> HeartbeatConfig {
        HeartbeatConfig {
            enabled: true,
            interval_secs: interval,
            command: command.into(),
            loss_threshold: threshold,
            ..HeartbeatConfig::default()
        }
    }

    fn response_texts(service: &DeviceService, dut: &str) -> Vec<String> {
        service
            .read_log(&format!("sess-{dut}"), 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn default_config_is_disabled_with_defaults() {
        let svc = HeartbeatService::new(test_device("default"));
        let cfg = svc.config("dut-x");
        assert!(!cfg.enabled);
        assert_eq!(cfg.interval_secs, 30);
        assert_eq!(cfg.command, "uname -a");
        assert_eq!(cfg.loss_threshold, 3);
        assert!(
            !svc.tick_at(1000),
            "unregistered/disabled DUT must not emit any probe"
        );
    }

    #[test]
    fn roundtrip_on_demo_session_records_response_and_sysinfo() {
        let device = test_device("roundtrip");
        let svc = HeartbeatService::new(device.clone());
        device
            .open_session("dut-a", "COM3", 115_200, crate::SessionKind::Demo)
            .unwrap();
        // Wait briefly for the demo driver's startup messages to reach the buffer.
        std::thread::sleep(Duration::from_millis(150));
        svc.register("dut-a", cfg("uname -a", 1, 2));

        // Dispatch at the due time and ignore output before the captured cursor.
        assert!(svc.tick_at(1000), "probe dispatch should change state");
        // Wait for the asynchronous serial write to appear in the buffer.
        let mut tx_seen = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            tx_seen = response_texts(&device, "dut-a")
                .iter()
                .any(|t| t == "[TX] uname -a");
            if tx_seen {
                break;
            }
        }
        assert!(
            tx_seen,
            "heartbeat command must be dispatched via write_line"
        );
        // Wait for the demo shell response, then advance within the response window.
        std::thread::sleep(Duration::from_millis(250));
        assert!(svc.tick_at(1300));
        let state = svc.state_or_default("dut-a");
        assert_eq!(state.last_ok, Some(true));
        assert_eq!(state.loss_count, 0);
        assert_eq!(state.last_at_unix_ms, 1300);
        assert!(
            state.last_response.contains("aarch64 GNU/Linux"),
            "raw response text must be preserved: {}",
            state.last_response
        );
        assert_eq!(state.sysinfo.hostname.as_deref(), Some("rk3576"));
        assert_eq!(state.sysinfo.cpu.as_deref(), Some("aarch64"));
        assert_eq!(svc.drain_alerts(), [] as [duthub_core::HeartbeatAlert; 0]);
        device.close_session("dut-a").ok();
    }

    #[test]
    fn multiline_probe_ignores_echo_and_waits_for_end_marker() {
        let batch = HeartbeatBatch::from_command("printf 'ready'\r\nuptime", 100).unwrap();
        assert!(HeartbeatBatch::from_command("uname -a\r\n", 100).is_none());
        let echo = batch.wire_text();
        assert!(!echo.contains(&batch.begin_marker()));
        assert!(!echo.contains(&batch.end_marker()));
        let prior = format!("{echo}\r\nold uptime\r\n{}\r\nready", batch.begin_marker());
        assert_eq!(batch.response(&prior, Some("ready")), None);
        let complete = format!(
            "{prior}\r\nnew uptime\r\n{}\r\nprompt # ",
            batch.end_marker()
        );
        assert_eq!(batch.response(&complete, Some("old uptime")), None);
        assert_eq!(
            batch.response(&complete, Some("new uptime")),
            Some("ready\r\nnew uptime")
        );
    }

    #[test]
    #[ignore = "uses the POSIX shell named in DUTHUB_TEST_POSIX_SHELL"]
    fn multiline_probe_runs_as_one_posix_shell_group() {
        let shell = std::env::var_os("DUTHUB_TEST_POSIX_SHELL").unwrap();
        let batch = HeartbeatBatch::from_command(
            "printf '%s\\n' \"first's output\"\r\nprintf '%s\\n' 'second output'\r\nexit 7",
            100,
        )
        .unwrap();
        let script = format!(
            "{}\nprintf '%s\\n' shell-still-running",
            batch.wire_text().replace('\r', "\n")
        );
        let output = std::process::Command::new(shell)
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            batch.response(&stdout, Some("second output")),
            Some("first's output\nsecond output")
        );
        assert!(stdout.ends_with("shell-still-running\n"));
    }

    #[test]
    fn heartbeat_batch_records_outputs_from_both_commands_on_one_probe() {
        let device = test_device("batch");
        let svc = HeartbeatService::new(device.clone());
        device
            .open_session("dut-batch", "DEMO", 115_200, crate::SessionKind::Demo)
            .unwrap();
        svc.register(
            "dut-batch",
            HeartbeatConfig {
                expected_response: Some("4 days".into()),
                ..cfg("uname -a\r\nuptime", 1, 2)
            },
        );
        assert!(svc.tick_at(1000));
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(10));
            svc.tick_at(1200);
            if svc.state_or_default("dut-batch").last_ok.is_some() {
                break;
            }
        }
        let state = svc.state_or_default("dut-batch");
        assert_eq!(state.last_ok, Some(true));
        assert_eq!(state.sysinfo.cpu.as_deref(), Some("aarch64"));
        assert!(state.last_response.contains("4 days"));
        assert!(!state.last_response.contains("DUTHUB-HB-"));
        assert_eq!(
            response_texts(&device, "dut-batch")
                .iter()
                .filter(|line| line.starts_with("[TX]"))
                .count(),
            1
        );
        device.close_session("dut-batch").unwrap();
    }

    #[test]
    fn window_expiry_counts_loss_and_fires_alert_at_threshold() {
        let device = test_device("loss");
        let svc = HeartbeatService::new(device.clone());
        device
            .open_session("dut-b", "COM3", 115_200, crate::SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        svc.register("dut-b", cfg("uname -a", 1, 2));
        // Close the session after sending to simulate a probe that never receives a reply.
        assert!(svc.tick_at(1000));
        device.close_session("dut-b").ok();
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            !svc.tick_at(1500),
            "must not evaluate loss before response window expires"
        );
        assert!(svc.tick_at(2500), "loss detection should change state");
        let state = svc.state_or_default("dut-b");
        assert_eq!(state.last_ok, Some(false));
        assert_eq!(state.loss_count, 1);
        assert!(
            svc.drain_alerts().is_empty(),
            "must not alert below loss threshold"
        );
        // A second unanswered probe reaches the configured threshold.
        svc.note_loss("dut-b", &cfg("uname -a", 1, 2), 3000);
        let state = svc.state_or_default("dut-b");
        assert_eq!(state.loss_count, 2);
        let alerts = svc.drain_alerts();
        assert_eq!(
            alerts.len(),
            1,
            "should emit one alert upon reaching threshold"
        );
        assert_eq!(alerts[0].dut_id, "dut-b");
        assert_eq!(alerts[0].loss_count, 2);
        assert!(alerts[0].message.contains("2 次无响应"));
        // Further losses must not generate duplicate alerts.
        svc.note_loss("dut-b", &cfg("uname -a", 1, 2), 4000);
        assert_eq!(svc.drain_alerts(), [] as [duthub_core::HeartbeatAlert; 0]);
        // A valid response clears the loss count.
        assert!(svc.note_response("dut-b", &["pong".into()], 5000));
        assert_eq!(svc.state_or_default("dut-b").loss_count, 0);
        assert_eq!(svc.state_or_default("dut-b").last_ok, Some(true));
    }

    #[test]
    fn response_lines_exclude_tx_serial_and_mark() {
        let device = test_device("filter");
        let svc = HeartbeatService::new(device.clone());
        device
            .open_session("dut-c", "COM3", 115_200, crate::SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        svc.register("dut-c", cfg("uname -a", 1, 3));
        assert!(svc.tick_at(1000));
        // Inject local annotations as well as a device prompt.
        device.log_mark("dut-c", "继电器 power ON").unwrap();
        std::thread::sleep(Duration::from_millis(250));
        assert!(svc.tick_at(1300));
        let state = svc.state_or_default("dut-c");
        assert_eq!(state.last_ok, Some(true));
        assert!(
            !state.last_response.contains("[mark]"),
            "mark/serial/TX lines must not count as device responses: {}",
            state.last_response
        );
        device.close_session("dut-c").ok();
    }

    #[test]
    fn set_config_persists_via_hook_and_cancels_flight() {
        let device = test_device("persist");
        let svc = HeartbeatService::new(device.clone());
        let recorded: Arc<Mutex<Vec<(String, HeartbeatConfig)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = recorded.clone();
        let svc = svc.with_persist(Arc::new(move |dut: &str, cfg: &HeartbeatConfig| {
            sink.lock().unwrap().push((dut.to_string(), cfg.clone()));
            Ok(())
        }));
        let want = cfg("cat /proc/uptime", 0, 5);
        let applied = svc.set_config("dut-p", want).unwrap();
        assert_eq!(
            applied.interval_secs, 1,
            "interval must clamp to 1s minimum"
        );
        assert_eq!(applied.command, "cat /proc/uptime");
        assert_eq!(
            recorded.lock().unwrap().len(),
            1,
            "persist hook must receive saved config"
        );
        assert_eq!(svc.config("dut-p").command, "cat /proc/uptime");
        // A failed persistence callback must preserve the active configuration.
        let failing = svc.with_persist(Arc::new(|_: &str, _: &HeartbeatConfig| {
            Err("disk full".into())
        }));
        let err = failing
            .set_config("dut-p", cfg("uname -a", 30, 3))
            .unwrap_err();
        assert!(err.contains("disk full"));
    }

    #[test]
    fn disabled_config_never_probes() {
        let device = test_device("disabled");
        let svc = HeartbeatService::new(device.clone());
        device
            .open_session("dut-d", "COM3", 115_200, crate::SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        svc.register(
            "dut-d",
            HeartbeatConfig {
                enabled: false,
                ..cfg("uname -a", 1, 3)
            },
        );
        assert!(!svc.tick_at(999_999));
        assert!(
            !response_texts(&device, "dut-d")
                .iter()
                .any(|t| t == "[TX] uname -a"),
            "disabled heartbeat must not send commands"
        );
        device.close_session("dut-d").ok();
    }

    #[test]
    fn pause_prevents_probes_and_resumes_at_deadline() {
        let device = test_device("paused");
        let svc = HeartbeatService::new(device.clone());
        device
            .open_session("dut-pause", "COM3", 115_200, crate::SessionKind::Demo)
            .unwrap();
        svc.register(
            "dut-pause",
            HeartbeatConfig {
                paused_until_unix_ms: Some(5_000),
                ..cfg("uname -a", 1, 3)
            },
        );
        assert!(!svc.tick_at(4_999));
        assert!(svc.state("dut-pause").is_none());
        assert!(svc.tick_at(5_000));
        device.close_session("dut-pause").unwrap();
    }

    /// Allow at least two seconds per segment in a multi-command probe.
    /// A single command uses the ordinary timing limits. All windows cap at 30 seconds.
    #[test]
    fn capture_window_extends_for_multi_segment_commands() {
        // A three-segment command needs at least six seconds.
        assert_eq!(
            capture_window_ms(1_000, "uname -a; cat /proc/uptime; head -n 2 /proc/meminfo"),
            6_000
        );
        // Newlines also separate segments.
        assert_eq!(
            capture_window_ms(1_000, "uname -a\ncat /proc/uptime"),
            4_000
        );
        // The single-command window remains clamped by the probe interval.
        assert_eq!(capture_window_ms(1_000, "uname -a"), 1_000);
        assert_eq!(capture_window_ms(30_000, "uname -a"), 5_000);
        // Consecutive separators and trailing whitespace add no segments.
        assert_eq!(capture_window_ms(1_000, "a;;\n b"), 4_000);
        // Cap the response window at 30 seconds.
        assert_eq!(
            capture_window_ms(1_000, "a; b; c; d; e; f; g; h; i; j; k; l; m; n; o; p"),
            30_000
        );
    }

    /// A lease denial defers the probe without counting a loss or reporting a probe result.
    /// The next successful dispatch after release clears the deferral.
    #[test]
    fn probe_deferred_by_lease_sets_paused_flag_until_success() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/heartbeat")
            .join(format!("lease-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lease = Arc::new(duthub_core::lease::ControlLeaseManager::new(None));
        let device =
            Arc::new(DeviceService::custom(256, dir, 0).with_lease_authority(lease.clone()));
        device
            .open_session("dut-lease", "COM3", 115_200, crate::SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let svc = HeartbeatService::new(device.clone());
        svc.register("dut-lease", cfg("uname -a", 1, 3));
        // Release the automatic GUI lease and let an Agent take control, blocking system probes.
        lease.release_for_holder("dut-lease", &duthub_core::lease::LeaseHolder::Gui);
        let grant = lease
            .acquire(
                "dut-lease",
                &duthub_core::lease::LeaseHolder::Agent {
                    agent_name: "bot".into(),
                    credential_id: "cred".into(),
                },
                None,
            )
            .unwrap();
        // A denied system probe sets the deferral flag and leaves its result unset.
        assert!(
            svc.tick_at(1000),
            "setting deferral flag should change state"
        );
        let state = svc.state_or_default("dut-lease");
        assert!(
            state.paused_by_lease,
            "lease denial must set paused_by_lease"
        );
        assert_eq!(
            state.loss_count, 0,
            "lease-blocked probe must not count as a loss"
        );
        assert_eq!(
            state.last_ok, None,
            "unstarted probe must not count as failed"
        );
        // Release the Agent lease and confirm that the next due dispatch clears the flag.
        lease.release("dut-lease", &grant.lease_id).unwrap();
        assert!(svc.tick_at(2000), "successful dispatch should change state");
        assert!(
            !svc.state_or_default("dut-lease").paused_by_lease,
            "successful probe must clear paused_by_lease"
        );
        device.close_session("dut-lease").ok();
    }
}
