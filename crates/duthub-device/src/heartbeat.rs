//! 会话心跳引擎（需求⑦）。
//!
//! - desktop 侧定时器（Workbench 50ms tick）调用 [`HeartbeatService::tick`]；
//!   到期经 [`DeviceService::write_line`] 下发自定义心跳命令。
//! - 响应收集：发送后记总线游标，超时窗口内（间隔 1s/5s 夹取）从会话缓冲读
//!   新行，排除本机回显/驱动信令/分段标记后视为设备响应（原文保存 + 系统信息
//!   启发式提取，见 `duthub_core::heartbeat::extract_sysinfo`）。
//! - 丢联判定：连续 N 次（`loss_threshold`，默认 3）无响应 → 产出
//!   [`duthub_core::heartbeat::HeartbeatAlert`]（队列，UI drain 后供弹窗阶段消费）。
//! - Agent 改写命令/间隔：[`HeartbeatService::set_config`]（内存 + 持久化钩子）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use duthub_core::DeviceError;
use duthub_core::heartbeat::{
    HeartbeatAlert, HeartbeatConfig, HeartbeatState, extract_sysinfo, is_response_line,
};
use duthub_core::ipc::UiEvent;

use crate::DeviceService;

/// duts.json 持久化钩子（desktop 注入：load → 改 heartbeat 段 → 原子保存）。
/// Err 时 [`HeartbeatService::set_config`] 不改内存配置（Agent 可感知失败）。
pub type HeartbeatPersistHook =
    Arc<dyn Fn(&str, &HeartbeatConfig) -> Result<(), String> + Send + Sync>;

/// 响应原文保存上限（字符），防止 dmesg 类响应撑爆内存/快照。
const LAST_RESPONSE_CAP: usize = 2000;

/// 一次在途探测：发送时刻的总线游标 + 超时窗口截止时刻。
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

/// 心跳引擎：配置 / 状态 / 在途探测 / 告警队列的单一归属。
pub struct HeartbeatService {
    device: Arc<DeviceService>,
    configs: Mutex<HashMap<String, HeartbeatConfig>>,
    states: Mutex<HashMap<String, HeartbeatState>>,
    flights: Mutex<HashMap<String, Flight>>,
    next_due: Mutex<HashMap<String, u64>>,
    alerts: Mutex<Vec<HeartbeatAlert>>,
    /// tick 互斥（UI tick 与 MCP 线程的即时探测可能并发，串行化避免双发）
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

/// 响应捕获窗（P2-029）：间隔 1s/5s 夹取；多段命令按段数扩展（每段 ≥2s、
/// 总窗 ≤30s），避免 `a; b; c` 链的第 3 段输出被截断。
/// 段数 = 按 `;` 与换行切分的非空段数（至少 1）。
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

    /// 挂持久化钩子（builder）。
    #[must_use]
    pub fn with_persist(self, hook: HeartbeatPersistHook) -> Self {
        *duthub_core::util::lock_unpoisoned(&self.persist) = Some(hook);
        self
    }

    /// 同步运行时配置（UI 保存路径 / 启动装载；不触发持久化）。
    pub fn register(&self, dut_id: &str, config: HeartbeatConfig) {
        self.configs
            .lock()
            .unwrap()
            .insert(dut_id.to_string(), config);
    }

    /// 当前配置（未注册 = 默认：关闭、30s、`uname -a`）。
    pub fn config(&self, dut_id: &str) -> HeartbeatConfig {
        self.configs
            .lock()
            .unwrap()
            .get(dut_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 最近一次状态（None 尚未探测 / Some(ok)）。
    pub fn state(&self, dut_id: &str) -> Option<HeartbeatState> {
        duthub_core::util::lock_unpoisoned(&self.states).get(dut_id).cloned()
    }

    pub fn state_or_default(&self, dut_id: &str) -> HeartbeatState {
        self.state(dut_id).unwrap_or_default()
    }

    /// Agent 改写心跳配置（命令/间隔/开关）：内存生效 + 持久化钩子落盘。
    /// 失败（钩子报错）时内存配置保持不变，返回错误文案。
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
        // 配置变更后取消在途探测：下一次 tick 立即按新命令发送（due 归零）
        self.reset_schedule(dut_id);
        self.config_changed.store(true, Ordering::Release);
        Ok(cfg)
    }

    /// 按 GUI 保存动作提交草稿。提交与 Agent 更新使用同一把锁，落盘后才启用编辑结果。
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

    /// 取走全部丢联告警（弹窗阶段消费；当前 UI 先转 toast）。
    pub fn drain_alerts(&self) -> Vec<HeartbeatAlert> {
        std::mem::take(&mut *duthub_core::util::lock_unpoisoned(&self.alerts))
    }

    /// 产品入口：真实时钟 tick。返回本 tick 是否有状态变化（供 UI 决定重绘）。
    pub fn tick(&self) -> bool {
        self.tick_at(now_unix_ms())
    }

    /// tick 主体（`now_ms` 可注入，测试不依赖真实时钟）。
    /// `ticking` 互斥串行化并发 tick（UI 定时 + MCP 即时探测）。
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
            let flight = duthub_core::util::lock_unpoisoned(&self.flights).get(&dut).cloned();
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
                    // 窗口到期无响应（含会话中途关闭）→ 丢联一次
                    changed |= self.note_loss(&dut, &cfg, now_ms);
                    duthub_core::util::lock_unpoisoned(&self.flights).remove(&dut);
                    self.next_due
                        .lock()
                        .unwrap()
                        .insert(dut.clone(), now_ms.saturating_add(interval_ms));
                }
                continue;
            }
            // 会话未打开：不发送、不计丢联（链路状态由断线重连徽标跟踪）
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
                        .write_heartbeat_batch(&dut, writer, batch.clone()),
                    None => self.device.write_line(&dut, writer, &cfg.command),
                };
                // P2-036：Busy/Lease 拒绝 = 探测被暂缓（不计丢联），置 per-DUT
                // 暂缓标记供状态查询区分「刻意挂起」与「心跳失效」；
                // 成功探测（Ok）与其他错误清除。
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
                        // 未排入串口时没有发起探测，不把控制权冲突或队列拥堵误报成死机。
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

    /// 记录一次成功响应（原文保存 + 系统信息提取 + 丢联清零）。
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

    /// 记录一次无响应（丢联计数 +1；达到阈值产出告警事件，不重复刷屏）。
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
        assert!(!svc.tick_at(1000), "未注册/未启用的 DUT 不产生任何探测");
    }

    #[test]
    fn roundtrip_on_demo_session_records_response_and_sysinfo() {
        let device = test_device("roundtrip");
        let svc = HeartbeatService::new(device.clone());
        device
            .open_session("dut-a", "COM3", 115_200, crate::SessionKind::Demo)
            .unwrap();
        // demo 驱动启动日志（delay=0，短暂等待落缓冲）
        std::thread::sleep(Duration::from_millis(150));
        svc.register("dut-a", cfg("uname -a", 1, 2));

        // t=1000：到期发送（游标=当前缓冲末尾，启动日志不计入响应）
        assert!(svc.tick_at(1000), "发送应有状态变化");
        // write_line 是异步投递：轮询等 TX 回显落缓冲
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
        assert!(tx_seen, "心跳命令必须经 write_line 下发");
        // 等模拟 shell 响应落缓冲，再 tick：窗口内收到响应
        std::thread::sleep(Duration::from_millis(250));
        assert!(svc.tick_at(1300));
        let state = svc.state_or_default("dut-a");
        assert_eq!(state.last_ok, Some(true));
        assert_eq!(state.loss_count, 0);
        assert_eq!(state.last_at_unix_ms, 1300);
        assert!(
            state.last_response.contains("aarch64 GNU/Linux"),
            "响应原文必须保存：{}",
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
        // 发送后立刻关闭会话（模拟设备失联）：窗口到期 → 丢联
        assert!(svc.tick_at(1000));
        device.close_session("dut-b").ok();
        std::thread::sleep(Duration::from_millis(120));
        assert!(!svc.tick_at(1500), "窗口未到期不判定");
        assert!(svc.tick_at(2500), "丢联判定应有状态变化");
        let state = svc.state_or_default("dut-b");
        assert_eq!(state.last_ok, Some(false));
        assert_eq!(state.loss_count, 1);
        assert!(svc.drain_alerts().is_empty(), "未达阈值不发告警");
        // 再丢一次 → 达阈值 2 → 告警
        svc.note_loss("dut-b", &cfg("uname -a", 1, 2), 3000);
        let state = svc.state_or_default("dut-b");
        assert_eq!(state.loss_count, 2);
        let alerts = svc.drain_alerts();
        assert_eq!(alerts.len(), 1, "恰好达阈值时发一次告警");
        assert_eq!(alerts[0].dut_id, "dut-b");
        assert_eq!(alerts[0].loss_count, 2);
        assert!(alerts[0].message.contains("2 次无响应"));
        // 持续丢联不再刷屏（避免弹窗风暴）
        svc.note_loss("dut-b", &cfg("uname -a", 1, 2), 4000);
        assert_eq!(svc.drain_alerts(), [] as [duthub_core::HeartbeatAlert; 0]);
        // 恢复响应后丢联清零
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
        // 注入本机注记行（TX / serial / mark）与设备提示符行
        device.log_mark("dut-c", "继电器 power ON").unwrap();
        std::thread::sleep(Duration::from_millis(250));
        assert!(svc.tick_at(1300));
        let state = svc.state_or_default("dut-c");
        assert_eq!(state.last_ok, Some(true));
        assert!(
            !state.last_response.contains("[mark]"),
            "mark/serial/TX 行不算设备响应：{}",
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
        assert_eq!(applied.interval_secs, 1, "间隔下限 1s 归一化");
        assert_eq!(applied.command, "cat /proc/uptime");
        assert_eq!(recorded.lock().unwrap().len(), 1, "钩子必须收到落盘配置");
        assert_eq!(svc.config("dut-p").command, "cat /proc/uptime");
        // 钩子失败：内存配置保持不变
        let failing =
            svc.with_persist(Arc::new(
                |_: &str, _: &HeartbeatConfig| Err("磁盘满".into()),
            ));
        let err = failing
            .set_config("dut-p", cfg("uname -a", 30, 3))
            .unwrap_err();
        assert!(err.contains("磁盘满"));
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
            "关闭的心跳不得下发命令"
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

    /// P2-029：多段命令的响应捕获窗按段数扩展（3 段 ≥ 6 秒，每段 2s），
    /// 单命令维持 1s/5s 夹取，总窗上限 30s。
    #[test]
    fn capture_window_extends_for_multi_segment_commands() {
        // 3 段命令窗口不小于 6 秒
        assert_eq!(
            capture_window_ms(1_000, "uname -a; cat /proc/uptime; head -n 2 /proc/meminfo"),
            6_000
        );
        // 换行分段同样计段
        assert_eq!(
            capture_window_ms(1_000, "uname -a\ncat /proc/uptime"),
            4_000
        );
        // 单命令仍按间隔夹取
        assert_eq!(capture_window_ms(1_000, "uname -a"), 1_000);
        assert_eq!(capture_window_ms(30_000, "uname -a"), 5_000);
        // 空段（连续分号/行尾空白）不计入
        assert_eq!(capture_window_ms(1_000, "a;;\n b"), 4_000);
        // 总窗上限 30s
        assert_eq!(
            capture_window_ms(1_000, "a; b; c; d; e; f; g; h; i; j; k; l; m; n; o; p"),
            30_000
        );
    }

    /// P2-036：探测因 Lease 被拒 → `paused_by_lease` `置位（不计丢联、last_ok`
    /// 保持未探测）；租约释放后探测成功 → 标记清除。
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
        // Agent 接管控制权（先释放打开会话时的 GUI 自动租约）→ System 心跳写被拒
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
        // Lease 拒绝：置暂缓标记（状态变化），但不计丢联、last_ok 保持未探测
        assert!(svc.tick_at(1000), "置暂缓标记应有状态变化");
        let state = svc.state_or_default("dut-lease");
        assert!(state.paused_by_lease, "Lease 拒绝必须置暂缓标记");
        assert_eq!(state.loss_count, 0, "被租约拦截不计丢联");
        assert_eq!(state.last_ok, None, "未发起探测不算失败");
        // Agent 释放 → 下一次到期 tick 发送成功，标记清除
        lease.release("dut-lease", &grant.lease_id).unwrap();
        assert!(svc.tick_at(2000), "发送成功应有状态变化");
        assert!(
            !svc.state_or_default("dut-lease").paused_by_lease,
            "成功探测必须清除暂缓标记"
        );
        device.close_session("dut-lease").ok();
    }
}
