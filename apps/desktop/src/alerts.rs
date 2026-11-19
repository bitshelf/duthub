//! 异常弹窗提醒中心（需求⑤+④）：AlertItem 队列 + 按 source 去重/抑制。
//!
//! - 来源事件（接入点在 workspace.rs / mcp_runtime.rs）：
//!   * [`AlertSource::Heartbeat`]：心跳丢联（连续 N 次无响应，HeartbeatService 告警）
//!   * [`AlertSource::Serial`]：serial 会话 task 死亡 / 首开失败（驱动"[serial] 打开 … 失败"行）
//!   * [`AlertSource::Control`]：ControlService 动作失败（GUI 继电器往返 / MCP power/reset/power_key）
//!   * [`AlertSource::Agent`]：Agent 运行完成或异常（Done / Stopped，初步分析待人工确认）
//! - 去重：同一 source 30 秒内只弹一次（对持续失败保底 30s 提醒一次，不刷屏）。
//! - 抑制：弹窗「忽略此类」按 source 抑制；设置页可重置。总开关关闭时全部静默。
//! - 弹窗本体（kit Dialog，标题「需要人工干预」）由 Workbench 渲染层调度：
//!   本模块只做纯逻辑，方便单测。
//!
//! 持久化：enabled 与抑制列表存 settings.json `alerts` 段
//! （`duthub_core::model::AlertSettings`），AlertCenter 是其运行时镜像。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::agent::RunStatus;

/// 同一 source 的去重窗口。
pub const DEDUP_WINDOW: Duration = Duration::from_secs(30);

/// 提醒来源（稳定 key 用于设置持久化，UI 文案用于弹窗展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertSource {
    /// 心跳丢联（连续 N 次无响应）
    Heartbeat,
    /// serial 会话 task 死亡 / 首开失败
    Serial,
    /// ControlService 动作失败（继电器/PDU 写动作）
    Control,
    /// 烧录工具检查失败。
    Flash,
    /// Agent 运行完成或异常（初步分析待人工确认）
    Agent,
}

impl AlertSource {
    /// settings.json 抑制列表中的稳定 key。
    pub fn key(self) -> &'static str {
        match self {
            AlertSource::Heartbeat => "heartbeat",
            AlertSource::Serial => "serial",
            AlertSource::Control => "control",
            AlertSource::Flash => "flash",
            AlertSource::Agent => "agent",
        }
    }

    /// 从 settings 抑制列表 key 还原来源（未知 key 忽略，向前兼容）。
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "heartbeat" => Some(AlertSource::Heartbeat),
            "serial" => Some(AlertSource::Serial),
            "control" => Some(AlertSource::Control),
            "flash" => Some(AlertSource::Flash),
            "agent" => Some(AlertSource::Agent),
            _ => None,
        }
    }

    /// 弹窗「来源」行的中文标签。
    pub fn label(self) -> &'static str {
        match self {
            AlertSource::Heartbeat => crate::i18n::label("heartbeatSource"),
            AlertSource::Serial => crate::i18n::label("serialSource"),
            AlertSource::Control => crate::i18n::label("controlSource"),
            AlertSource::Flash => crate::i18n::label("flashTitle"),
            AlertSource::Agent => crate::i18n::label("agentAnalysis"),
        }
    }
}

/// 一条待人工干预提醒：`id` 单调递增，`at` 为 unix 毫秒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertItem {
    pub id: u64,
    pub title: String,
    pub detail: String,
    pub source: AlertSource,
    pub at: u64,
    pub dut_id: String,
}

/// 提醒中心：待弹窗队列 + 去重窗口 + 抑制列表 + 总开关。
///
/// GUI（Workbench）与 MCP HTTP 线程共用（`Arc<Mutex<AlertCenter>>`）：
/// MCP 侧动作失败在此入队，GUI 渲染层出队弹窗。
#[derive(Default)]
pub struct AlertCenter {
    next_id: u64,
    items: Vec<AlertItem>,
    /// 各来源最近一次**被接受**的告警时刻（unix ms）；30s 内再次上报丢弃。
    last_accepted: HashMap<(String, AlertSource), u64>,
    by_dut: BTreeMap<String, duthub_core::model::AlertSettings>,
    suppressed: HashSet<AlertSource>,
    enabled: bool,
}

fn now_unix_ms() -> u64 {
    now_ms()
}

/// 当前 unix 毫秒（弹窗时间显示等 UI 侧共用）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 弹窗内的时间显示：相对当下（<1 分钟「刚刚」、<1 小时「N 分钟前」、
/// 更久「N 小时前」）。无 chrono 依赖，可注入 now 单测。
pub fn format_alert_time(at_ms: u64, now_ms: u64) -> String {
    let elapsed_ms = now_ms.saturating_sub(at_ms);
    let mins = elapsed_ms / 60_000;
    if elapsed_ms < 60_000 {
        crate::i18n::label("justNow").to_string()
    } else if mins < 60 {
        crate::i18n::message("minutesAgo", &[("count", &mins.to_string())])
    } else {
        crate::i18n::message("hoursAgo", &[("count", &(mins / 60).to_string())])
    }
}

impl AlertCenter {
    pub fn set_dut_settings(
        &mut self,
        settings: &BTreeMap<String, duthub_core::model::AlertSettings>,
    ) {
        self.by_dut.clone_from(settings);
    }

    fn allows(&self, dut_id: &str, source: AlertSource) -> bool {
        match self.by_dut.get(dut_id) {
            Some(settings) => {
                settings.enabled
                    && !settings
                        .suppressed_sources
                        .iter()
                        .any(|key| key == source.key())
            }
            None => self.enabled && !self.suppressed.contains(&source),
        }
    }

    pub fn report_for(
        &mut self,
        dut_id: &str,
        source: AlertSource,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> bool {
        self.report_at_for(now_unix_ms(), dut_id, source, title, detail)
    }
    /// 按设置初始化（enabled + 抑制列表；未知 key 忽略）。
    pub fn from_settings(enabled: bool, suppressed_keys: &[String]) -> Self {
        Self {
            enabled,
            suppressed: suppressed_keys
                .iter()
                .filter_map(|k| AlertSource::from_key(k))
                .collect(),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// 抑制一类来源（弹窗「忽略此类」）。返回是否发生了变化。
    #[cfg(test)]
    pub fn suppress(&mut self, source: AlertSource) -> bool {
        self.suppressed.insert(source)
    }

    /// 解除一类来源的抑制（设置页当前只用整体重置，逐条恢复预留给后续）。
    #[allow(dead_code)]
    pub fn resume(&mut self, source: AlertSource) -> bool {
        self.suppressed.remove(&source)
    }

    /// 当前抑制列表的稳定 key（回写 settings）。
    #[cfg(test)]
    pub fn suppressed_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .suppressed
            .iter()
            .map(|s| s.key().to_string())
            .collect();
        // 顺序稳定（UI 展示与 settings 持久化一致）
        keys.sort();
        keys
    }

    /// 清空抑制列表（设置页「重置」）。返回是否有变化。
    #[cfg(test)]
    pub fn reset_suppressed(&mut self) -> bool {
        let had = !self.suppressed.is_empty();
        self.suppressed.clear();
        had
    }

    #[cfg(test)]
    pub fn is_suppressed(&self, source: AlertSource) -> bool {
        self.suppressed.contains(&source)
    }

    /// 上报一条提醒（真实时钟）。返回是否产出新条目（去重/抑制/关闭时为 false）。
    /// 上报（可注入时钟，测试用）。同一 source 30 秒内去重。
    #[cfg(test)]
    pub fn report_at(
        &mut self,
        now_ms: u64,
        source: AlertSource,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> bool {
        self.report_at_for(now_ms, "", source, title, detail)
    }

    fn report_at_for(
        &mut self,
        now_ms: u64,
        dut_id: &str,
        source: AlertSource,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> bool {
        if !self.allows(dut_id, source) {
            return false;
        }
        let key = (dut_id.to_owned(), source);
        if let Some(last) = self.last_accepted.get(&key)
            && now_ms.saturating_sub(*last) < DEDUP_WINDOW.as_millis() as u64
        {
            return false;
        }
        self.next_id += 1;
        self.last_accepted.insert(key, now_ms);
        self.items.push(AlertItem {
            id: self.next_id,
            title: title.into(),
            detail: detail.into(),
            source,
            at: now_ms,
            dut_id: dut_id.to_owned(),
        });
        true
    }

    /// 待弹窗条数（含被抑制的——它们在出队时静默丢弃；诊断/单测用）。
    #[allow(dead_code)]
    pub fn pending_len(&self) -> usize {
        self.items.len()
    }

    /// 丢弃某来源的全部待弹窗条目（「忽略此类」后清队列，避免下一帧重复弹）。
    #[cfg(test)]
    pub fn drop_source(&mut self, source: AlertSource) -> usize {
        let before = self.items.len();
        self.items.retain(|item| item.source != source);
        before - self.items.len()
    }

    /// 出队下一条待弹窗提醒（跳过并丢弃被抑制来源的条目）。
    pub fn pop_next(&mut self) -> Option<AlertItem> {
        while !self.items.is_empty() {
            let item = self.items.remove(0);
            if self.allows(&item.dut_id, item.source) {
                return Some(item);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// 来源接入点：驱动/引擎输出 → (title, detail) 的纯映射（单测覆盖）
// ---------------------------------------------------------------------------

/// 接入点①：心跳丢联告警（HeartbeatService::drain_alerts）→ 提醒文案。
pub fn heartbeat_alert_texts(dut_label: &str, message: &str) -> (String, String) {
    (
        crate::i18n::label("heartbeatLostAlert").into(),
        crate::i18n::message(
            "heartbeatAlertDetail",
            &[("dut", dut_label), ("detail", message)],
        ),
    )
}

/// 接入点②：驱动首开失败行解析（real 驱动输出
/// `[serial] 打开 {port} @ {baud} 失败: {err}`）。该行之后会话 task 直接结束，
/// 即「首开失败 = task 死亡」的唯一 observable 信号；重连/TX 行不误报。
pub fn parse_serial_open_failure(text: &str) -> Option<String> {
    let rest = text.strip_prefix("[serial] 打开 ")?;
    let at = rest.find(" 失败: ")?;
    Some(rest[..at].trim().to_string())
}

/// 接入点③：ControlService 写动作失败 → 提醒文案。
pub fn control_action_alert_texts(dut_label: &str, action: &str, err: &str) -> (String, String) {
    (
        crate::i18n::label("controlFailedAlert").into(),
        crate::i18n::message(
            "controlFailedDetail",
            &[("dut", dut_label), ("action", action), ("error", err)],
        ),
    )
}

/// 接入点④：Agent 会话状态迁移 → 初步分析提醒。
/// 仅「运行中 → Done / Stopped」触发；Done/Stopped 自身的重复 tick 不再弹。
/// 返回 (title, detail)；findings 计数附在 detail 中。
pub fn agent_transition_alert_texts(
    prev: RunStatus,
    now: RunStatus,
    workflow_label: &str,
    findings: usize,
) -> Option<(String, String)> {
    if prev != RunStatus::Running {
        return None;
    }
    let phase = match now {
        RunStatus::Done => crate::i18n::label("agentWorkflowDone"),
        RunStatus::Stopped => crate::i18n::label("agentWorkflowStopped"),
        _ => return None,
    };
    Some((
        crate::i18n::label("agentReportAlert").into(),
        crate::i18n::message(
            "agentReportDetail",
            &[
                ("workflow", workflow_label),
                ("phase", phase),
                ("count", &findings.to_string()),
            ],
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 测试辅助：Default 派生的 enabled=false 在测试里要拉起总开关
    fn new_center() -> AlertCenter {
        AlertCenter {
            enabled: true,
            ..AlertCenter::default()
        }
    }

    #[test]
    fn dut_alert_rules_and_dedup_are_independent() {
        let mut center = new_center();
        let source = AlertSource::Heartbeat;
        assert!(center.report_at_for(100, "dut-a", source, "timeout", ""));
        assert!(center.report_at_for(100, "dut-b", source, "timeout", ""));
        assert!(!center.report_at_for(101, "dut-b", source, "duplicate", ""));
        let mut settings = BTreeMap::new();
        settings.insert(
            "dut-a".into(),
            duthub_core::model::AlertSettings {
                enabled: true,
                suppressed_sources: vec![source.key().into()],
            },
        );
        center.set_dut_settings(&settings);
        assert_eq!(center.pop_next().unwrap().dut_id, "dut-b");
        assert!(!center.report_at_for(40_000, "dut-a", source, "timeout", ""));
        assert!(center.report_at_for(40_000, "dut-b", source, "timeout", ""));
    }

    #[test]
    fn report_accepts_then_dedups_within_window() {
        let mut center = new_center();
        assert!(
            center.report_at(1_000, AlertSource::Control, "t1", "d1"),
            "首条必须入队"
        );
        // 30s 内同 source 去重（不同文案也不弹）
        assert!(!center.report_at(10_000, AlertSource::Control, "t2", "d2"));
        assert!(!center.report_at(30_999, AlertSource::Control, "t2", "d2"));
        assert_eq!(center.pending_len(), 1);
        // 窗口过后恢复提醒（持续失败保底 30s 一条）
        assert!(center.report_at(31_000, AlertSource::Control, "t3", "d3"));
        assert_eq!(center.pending_len(), 2);
        // 不同 source 互不影响去重窗口
        assert!(center.report_at(31_500, AlertSource::Serial, "s", "d"));
        // id 单调；出队顺序 = 入队顺序
        let first = center.pop_next().unwrap();
        let second = center.pop_next().unwrap();
        let third = center.pop_next().unwrap();
        assert!(second.id > first.id && third.id > second.id);
        assert_eq!(first.source, AlertSource::Control);
        assert_eq!(first.at, 1_000);
        assert_eq!(third.source, AlertSource::Serial);
        assert!(center.pop_next().is_none(), "队列取空");
    }

    #[test]
    fn suppress_blocks_reports_and_pending_items() {
        let mut center = new_center();
        assert!(center.report_at(0, AlertSource::Serial, "t", "d"));
        assert!(center.report_at(0, AlertSource::Agent, "t2", "d2"));
        center.suppress(AlertSource::Serial);
        // 抑制后同 source 不再入队
        assert!(
            !center.report_at(60_000, AlertSource::Serial, "t3", "d3"),
            "被抑制来源不得入队"
        );
        // 已入队的同 source 条目出队时被静默丢弃
        let next = center.pop_next().unwrap();
        assert_eq!(next.source, AlertSource::Agent, "serial 条目应被跳过");
        assert!(center.pop_next().is_none());
        // 解除抑制后恢复
        assert!(center.resume(AlertSource::Serial));
        assert!(center.report_at(61_000, AlertSource::Serial, "t4", "d4"));
        assert!(center.pop_next().is_some());
    }

    #[test]
    fn suppress_ignores_this_class_drops_queue_and_persists_keys() {
        let mut center = new_center();
        assert!(center.report_at(0, AlertSource::Control, "t", "d"));
        // 30s 去重窗口后第二条同来源入队
        assert!(center.report_at(31_000, AlertSource::Control, "t2", "d2"));
        // 「忽略此类」：抑制 + 清空该来源待弹窗
        assert!(center.suppress(AlertSource::Control));
        assert_eq!(center.drop_source(AlertSource::Control), 2);
        assert!(center.pop_next().is_none());
        // 抑制列表以稳定 key 持久化（顺序稳定），重置后清空
        assert_eq!(center.suppressed_keys(), vec!["control".to_string()]);
        assert!(center.reset_suppressed());
        assert!(!center.reset_suppressed(), "空列表重置无变化");
        assert!(!center.is_suppressed(AlertSource::Control));
    }

    #[test]
    fn master_switch_silences_all_sources() {
        let mut center = new_center();
        center.set_enabled(false);
        assert!(!center.enabled());
        for source in [
            AlertSource::Heartbeat,
            AlertSource::Serial,
            AlertSource::Control,
            AlertSource::Agent,
        ] {
            assert!(
                !center.report_at(0, source, "t", "d"),
                "总开关关闭必须全静默"
            );
        }
        assert_eq!(center.pending_len(), 0);
        center.set_enabled(true);
        assert!(center.report_at(0, AlertSource::Heartbeat, "t", "d"));
    }

    #[test]
    fn from_settings_restores_enabled_and_suppressed_keys() {
        let mut center = AlertCenter::from_settings(
            true,
            &[
                "serial".to_string(),
                "agent".to_string(),
                "未知key".to_string(),
            ],
        );
        assert!(center.enabled());
        assert!(center.is_suppressed(AlertSource::Serial));
        assert!(center.is_suppressed(AlertSource::Agent));
        assert!(
            !center.report_at(0, AlertSource::Serial, "t", "d"),
            "持久化抑制必须在启动后继续生效"
        );
        assert!(center.report_at(0, AlertSource::Control, "t", "d"));
        // 未知 key 被忽略（向前兼容），不污染回写
        assert_eq!(
            center.suppressed_keys(),
            vec!["agent".to_string(), "serial".to_string()]
        );
    }

    #[test]
    fn source_keys_roundtrip() {
        for source in [
            AlertSource::Heartbeat,
            AlertSource::Serial,
            AlertSource::Control,
            AlertSource::Agent,
        ] {
            assert_eq!(AlertSource::from_key(source.key()), Some(source));
        }
        assert_eq!(AlertSource::from_key("nope"), None);
    }

    #[test]
    fn parses_driver_first_open_failure_line() {
        assert_eq!(
            parse_serial_open_failure("[serial] 打开 COM25 @ 9600 失败: 拒绝访问。"),
            Some("COM25 @ 9600".to_string())
        );
        // 重连失败 / 重连排期 / TX / 普通输出不误报
        assert_eq!(
            parse_serial_open_failure("[serial] 第 2 次重连 · 600ms 后重试"),
            None
        );
        assert_eq!(
            parse_serial_open_failure("[serial] COM30 @ 115200 已打开"),
            None
        );
        assert_eq!(parse_serial_open_failure("[TX] uname -a"), None);
        assert_eq!(parse_serial_open_failure("hello"), None);
    }

    #[test]
    fn heartbeat_alert_texts_carry_dut_and_hint() {
        let (title, detail) =
            heartbeat_alert_texts("RK3576", "DUT dut-a 心跳丢联：连续 3 次无响应");
        assert_eq!(title, "心跳丢联，设备可能已失联");
        assert!(detail.contains("RK3576"));
        assert!(detail.contains("连续 3 次无响应"));
    }

    #[test]
    fn control_action_alert_texts_carry_action_and_error() {
        let (title, detail) = control_action_alert_texts("RK3576", "继电器 power ON", "无应答");
        assert!(title.contains("人工"));
        assert!(detail.contains("继电器 power ON"));
        assert!(detail.contains("无应答"));
    }

    #[test]
    fn alert_time_is_relative_and_monotonic_safe() {
        assert_eq!(format_alert_time(0, 59_999), "刚刚");
        assert_eq!(format_alert_time(0, 60_000), "1 分钟前");
        assert_eq!(format_alert_time(0, 3_599_999), "59 分钟前");
        assert_eq!(format_alert_time(0, 3_600_000), "1 小时前");
        assert_eq!(format_alert_time(0, 7_200_000), "2 小时前");
        // 时钟回拨（at > now）不 panic，按「刚刚」处理
        assert_eq!(format_alert_time(9_999_999, 1_000), "刚刚");
    }

    #[test]
    fn agent_transition_alerts_fire_only_from_running() {
        // Running → Done：标题固定 + findings 计数
        let (title, detail) =
            agent_transition_alert_texts(RunStatus::Running, RunStatus::Done, "DUT 故障诊断", 1)
                .unwrap();
        assert_eq!(title, "初步分析完成，请人工确认结论");
        assert!(detail.contains("工作流已完成"));
        assert!(detail.contains("findings 1 条"));
        // Running → Stopped：同标题（Stopped-on-error/中止），detail 注明中止
        let (title, detail) =
            agent_transition_alert_texts(RunStatus::Running, RunStatus::Stopped, "缺陷修复", 0)
                .unwrap();
        assert_eq!(title, "初步分析完成，请人工确认结论");
        assert!(detail.contains("运行中止"));
        // 非 Running 起点不触发（Done/Stopped/Paused/Idle → Done 都不算）
        assert!(agent_transition_alert_texts(RunStatus::Done, RunStatus::Done, "x", 0).is_none());
        assert!(
            agent_transition_alert_texts(RunStatus::Stopped, RunStatus::Done, "x", 0).is_none()
        );
        assert!(agent_transition_alert_texts(RunStatus::Paused, RunStatus::Done, "x", 0).is_none());
        // Running → Paused/Idle 不触发
        assert!(
            agent_transition_alert_texts(RunStatus::Running, RunStatus::Paused, "x", 0).is_none()
        );
        assert!(
            agent_transition_alert_texts(RunStatus::Running, RunStatus::Idle, "x", 0).is_none()
        );
    }
}
