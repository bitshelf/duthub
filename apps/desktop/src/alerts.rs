//! Exception Popup Notification Center (Requirements ⑤+④): `AlertItem` queue +
//! deduplication/inhibition based on source.
//!
//! - Source events (in `workspace.rs` / `mcp_runtime.rs`):
//!   * [`AlertSource::Heartbeat`]: Heartbeat loss (No response for N consecutive times; Alert for
//!     `HeartbeatService`)
//!   * [`AlertSource::Serial`]: Serial session task failure/initialization failure (Driver fails to
//!     initialize `"[serial]"` module)
//!   * [`AlertSource::Control`]: Failure in `ControlService` actions (GUI relay operations / MCP
//!     `power/reset/power_key` commands)
//!   * [`AlertSource::Agent`]: Agent execution completion or errors (Status: Done/Stopped;
//!     Preliminary analysis requires manual confirmation)
//!   * [`AlertSource::Relay`]: board-level offline monitor for shared relay boards
//!     (`duts.json.relayBoards`; the alert subject is the board id, not a DUT).
//!   * [`AlertSource::AgentNotice`]: a validated `notify_user` MCP notice; its
//!     level is carried into [`AlertItem::level`] and the tray balloon icon.
//!   * [`AlertSource::SerialGarbled`]: the session's raw RX byte stream entered
//!     the `Garbled` level (baud-rate mismatch heuristic, see `garble_ui`).
//!
//! Show at most one alert per source every 30 seconds, including persistent failures. Suppress
//! sources individually and let settings reset suppression. A DUT can also disable alerts through
//! `alertsByDut.<id>.enabled` in `duts.json`. Alerts are enabled by default. Sources outside a DUT,
//! such as relay boards or notifications on the general listener, can only be suppressed by source.
//! The workbench renders the dialog, while this module owns the alert logic.
//!
//! Persistence: The lists of enabled and disabled states for each DUT are stored in the `duts.json`
//! file (`duthub_core::model::AlertSettings`); the global suppression list is stored in the
//! `settings.json` file (`alerts.suppressedSources`, `duthub_core::model::GlobalAlertSettings`).
//! `AlertCenter` is the runtime image used for these two configurations.
//!
//! # Port surface (`docs/design/TRAY_ALERT_DESIGN.md` §12)
//!
//! New callers only need this narrow API:
//!
//! - the `report_*` family
//!   ([`AlertCenter::report_for`], [`AlertCenter::report_checked_for`],
//!   [`AlertCenter::report_checked_at_for`]; the legacy `-> bool` variants
//!   delegate to the checked ones);
//! - [`AlertCenter::latest`] and [`AlertCenter::generation`] — the read-only
//!   feed the hidden-state router consumes;
//! - [`AlertCenter::enabled_for`] / [`AlertCenter::is_suppressed_for`] —
//!   subject-level switch and suppression queries;
//! - [`AlertCenter::pop_next`] — the dialog renderer's only dequeue path.
//!
//! Settings mirroring ([`AlertCenter::set_dut_settings`],
//! [`AlertCenter::suppress`], [`AlertCenter::reset_suppressed`]) and the text
//! helpers at the bottom are integration surface for `workspace.rs`,
//! `pages.rs` and `mcp_runtime.rs`; they are not a second decision layer.
//!
//! The module never sees GPUI or OS types — only strings — so it stays unit
//! testable and the tray/close machinery can stay source-agnostic. Adding a
//! source is a data-only change: one [`AlertSource`] variant, its
//! `key`/`from_key`/`label` arms, one label in both locale files and the
//! settings-page source list. Tray, close and lifecycle code switch on
//! [`crate::tray_ui::AlertPresentation`], never on a source.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Deduplication window for the same source.
pub const DEDUP_WINDOW: Duration = Duration::from_secs(30);

/// Global pending-queue cap. When it is reached, only items of the *same*
/// source are evicted (oldest first); a source that is already at its own
/// quota cannot push anything older out, so a noisy source can never displace
/// device alerts.
pub const MAX_PENDING: usize = 64;
/// Per-source pending quota (same eviction order as [`MAX_PENDING`]).
pub const MAX_PENDING_PER_SOURCE: usize = 16;

/// Source of reminder (Stable key is used for setting persistence; UI text is used to display in
/// popups).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertSource {
    /// Heartbeat loss (No response for N consecutive times)
    Heartbeat,
    /// Serial session tasks have failed during execution/initialization.
    Serial,
    /// The `ControlService` action failed (relay/PDU write operation).
    Control,
    /// The burning tool failed to complete its inspection.
    Flash,
    /// Agent execution completed or encountered an error ( preliminary analysis requires manual
    /// confirmation).
    Agent,
    /// Relay board offline monitor (`duts.json.relayBoards`), not a DUT session.
    Relay,
    /// Server-side Agent notice (`notify_user` MCP tool).
    AgentNotice,
    /// Serial RX bytes look like a baud-rate mismatch (per-DUT opt-in).
    SerialGarbled,
    /// Bound HDMI / capture device disconnected.
    Media,
    /// Bound USB debug (ADB / HDC) device disconnected.
    UsbDebug,
}

impl AlertSource {
    /// All alert sources selectable in the per-DUT suppressed sources list.
    pub const fn all() -> &'static [Self] {
        &[
            Self::UsbDebug,
            Self::Media,
            Self::Relay,
            Self::Heartbeat,
            Self::Serial,
            Self::Control,
            Self::Flash,
            Self::Agent,
            Self::SerialGarbled,
            Self::AgentNotice,
        ]
    }

    /// The settings.json file contains a list of stable keys that can be suppressed.
    pub fn key(self) -> &'static str {
        match self {
            AlertSource::Heartbeat => "heartbeat",
            AlertSource::Serial => "serial",
            AlertSource::Control => "control",
            AlertSource::Flash => "flash",
            AlertSource::Agent => "agent",
            AlertSource::Relay => "relay",
            AlertSource::AgentNotice => "agentNotice",
            AlertSource::SerialGarbled => "serialGarbled",
            AlertSource::Media => "media",
            AlertSource::UsbDebug => "usbDebug",
        }
    }

    /// Restore the source from the settings suppression list (Unknown keys will be ignored,
    /// maintaining backward compatibility).
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "heartbeat" => Some(AlertSource::Heartbeat),
            "serial" => Some(AlertSource::Serial),
            "control" => Some(AlertSource::Control),
            "flash" => Some(AlertSource::Flash),
            "agent" => Some(AlertSource::Agent),
            "relay" => Some(AlertSource::Relay),
            "agentNotice" => Some(AlertSource::AgentNotice),
            "serialGarbled" => Some(AlertSource::SerialGarbled),
            "media" => Some(AlertSource::Media),
            "usbDebug" => Some(AlertSource::UsbDebug),
            _ => None,
        }
    }

    /// The Chinese label in the "Source" column of the pop-up window.
    pub fn label(self) -> &'static str {
        match self {
            AlertSource::Heartbeat => crate::i18n::label("heartbeatSource"),
            AlertSource::Serial => crate::i18n::label("serialSource"),
            AlertSource::Control => crate::i18n::label("controlSource"),
            AlertSource::Flash => crate::i18n::label("flashTitle"),
            AlertSource::Agent => crate::i18n::label("agentAnalysis"),
            AlertSource::Relay => crate::i18n::label("relaySource"),
            AlertSource::AgentNotice => crate::i18n::label("agentNoticeSource"),
            AlertSource::SerialGarbled => crate::i18n::label("serialGarbledSource"),
            AlertSource::Media => crate::i18n::label("mediaSource"),
            AlertSource::UsbDebug => crate::i18n::label("usbDebugSource"),
        }
    }
}

/// Severity of one alert. Only the tray balloon icon uses it today; it is not
/// persisted. Legacy report paths default to [`AlertLevel::Warning`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlertLevel {
    Info,
    #[default]
    Warning,
    Error,
}

/// Read-only view of the newest presentable alert (tray balloon text/level).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertSummary {
    pub source: AlertSource,
    pub level: AlertLevel,
    pub title: String,
    pub detail: String,
}

/// Why [`AlertCenter::report_checked_for`] refused a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportRejection {
    /// The per-DUT switch is off (`duts.json` `alertsByDut.<id>.enabled =
    /// false`). Subjects without an entry default to enabled, so only a DUT
    /// the user explicitly silenced can be rejected here.
    Disabled,
    /// The source is suppressed for this DUT (or globally when `dut_id` is "").
    Suppressed,
    /// Same `(dut_id, source)` reported inside [`DEDUP_WINDOW`].
    Duplicate,
    /// The pending queue is full for this source.
    Capacity,
}

/// There is one reminder that requires human intervention: `id` increases monotonically, and `at`
/// represents the time in Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertItem {
    pub id: u64,
    pub title: String,
    pub detail: String,
    pub source: AlertSource,
    pub level: AlertLevel,
    pub at: u64,
    pub dut_id: String,
    /// How many reports were folded into this item while hidden (0 = none).
    pub repeat: u32,
}

/// Reminder Center: Pending pop-up queue, deduplication window, switch per DUT, and suppression
/// list.
///
/// The GUI (Workbench) and MCP HTTP threads share resources via `Arc<Mutex<AlertCenter>>`: Any
/// failures on the MCP side are enqueued here, while pop-up windows are enqueued at the GUI
/// rendering layer.
#[derive(Default)]
pub struct AlertCenter {
    next_id: u64,
    items: Vec<AlertItem>,
    /// The last time alarms from various sources were **accepted** (in units of unix milliseconds);
    /// alarms that were reported again within 30 seconds will be discarded.
    last_accepted: HashMap<(String, AlertSource), u64>,
    by_dut: BTreeMap<String, duthub_core::model::AlertSettings>,
    suppressed: HashSet<AlertSource>,
    /// Bumped on every accepted (or merged) report: the hidden-state router
    /// uses it to notice new alerts without polling the queue.
    generation: u64,
    /// Fold repeated reports of one `(dut_id, source)` into the pending item.
    /// Only the hidden-to-tray state enables it, so the visible dedup tests
    /// keep their exact 30s semantics.
    merge_duplicates: bool,
}

fn now_unix_ms() -> u64 {
    now_ms()
}

/// Current Unix milliseconds (used for UI elements such as pop-up displays).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The time displayed in the pop-up window indicates the relative timing compared to the current
/// moment: <1 minute – "just now", <1 hour – "N minutes ago", and longer periods – "N hours ago".
/// This information does not depend on the "chrono" library; it can be injected into the "now" test
/// case.
#[cfg(test)]
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

    /// Per-DUT switch: a saved DUT follows its own `enabled`; every subject
    /// without an entry (draft DUTs, relay boards, the empty id of a generic
    /// Agent notice) defaults to enabled. There is no global switch any more,
    /// so non-DUT subjects can only be silenced per source
    /// ([`AlertCenter::suppress`]).
    pub fn enabled_for(&self, dut_id: &str) -> bool {
        self.by_dut
            .get(dut_id)
            .is_none_or(|settings| settings.enabled)
    }

    /// Suppression for this subject (`dut_id == ""` naturally falls back to
    /// the global suppression set).
    pub fn is_suppressed_for(&self, dut_id: &str, source: AlertSource) -> bool {
        match self.by_dut.get(dut_id) {
            Some(settings) => settings
                .suppressed_sources
                .iter()
                .any(|key| key == source.key()),
            None => self.suppressed.contains(&source),
        }
    }

    fn allows(&self, dut_id: &str, source: AlertSource) -> bool {
        self.enabled_for(dut_id) && !self.is_suppressed_for(dut_id, source)
    }

    /// Report with the default [`AlertLevel::Warning`]; `true` when accepted.
    pub fn report_for(
        &mut self,
        dut_id: &str,
        source: AlertSource,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> bool {
        self.report_at_for(now_unix_ms(), dut_id, source, title, detail)
    }

    /// Checked report (default warning level). The notifier uses the checked
    /// variants to get the rejection reason the MCP response must carry.
    pub fn report_checked_for(
        &mut self,
        dut_id: &str,
        source: AlertSource,
        level: AlertLevel,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Result<(), ReportRejection> {
        self.report_checked_at_for(now_unix_ms(), dut_id, source, level, title, detail)
    }

    /// Clear the dedup window for `(dut_id, source)` after a device recovers so
    /// a subsequent disconnect alerts immediately instead of being dropped inside
    /// [`DEDUP_WINDOW`].
    pub fn clear_dedup_for(&mut self, dut_id: &str, source: AlertSource) {
        self.last_accepted.remove(&(dut_id.to_owned(), source));
    }

    /// Drop pending queued alerts matching `(dut_id, source)` (used when a device
    /// recovers or the user suppresses a source for a DUT).
    pub fn drop_source_for(&mut self, dut_id: &str, source: AlertSource) {
        self.items
            .retain(|item| !(item.dut_id == dut_id && item.source == source));
    }

    /// Non-mutating pre-flight in the fixed rejection order
    /// (`enabled → suppressed → dedup → capacity`). Callers that must consult
    /// another gate (rate limiting) use it so a rejected report consumes
    /// nothing and a gate rejection never masks a dedup/capacity result.
    pub fn check_report(
        &self,
        dut_id: &str,
        source: AlertSource,
        now_ms: u64,
    ) -> Result<(), ReportRejection> {
        self.rejection(now_ms, dut_id, source)
    }

    fn rejection(
        &self,
        now_ms: u64,
        dut_id: &str,
        source: AlertSource,
    ) -> Result<(), ReportRejection> {
        if !self.enabled_for(dut_id) {
            return Err(ReportRejection::Disabled);
        }
        if self.is_suppressed_for(dut_id, source) {
            return Err(ReportRejection::Suppressed);
        }
        if let Some(last) = self.last_accepted.get(&(dut_id.to_owned(), source))
            && now_ms.saturating_sub(*last)
                < u64::try_from(DEDUP_WINDOW.as_millis()).unwrap_or(u64::MAX)
        {
            return Err(ReportRejection::Duplicate);
        }
        if self.merge_duplicates
            && self
                .items
                .iter()
                .any(|item| item.dut_id == dut_id && item.source == source)
        {
            return Ok(());
        }
        let source_count = self
            .items
            .iter()
            .filter(|item| item.source == source)
            .count();
        if source_count >= MAX_PENDING_PER_SOURCE {
            return Err(ReportRejection::Capacity);
        }
        if self.items.len() >= MAX_PENDING && source_count == 0 {
            return Err(ReportRejection::Capacity);
        }
        Ok(())
    }

    /// Checked report with an injectable clock. The only writer of
    /// [`AlertItem::level`]; the legacy bool variants delegate here.
    pub fn report_checked_at_for(
        &mut self,
        now_ms: u64,
        dut_id: &str,
        source: AlertSource,
        level: AlertLevel,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Result<(), ReportRejection> {
        self.rejection(now_ms, dut_id, source)?;
        let key = (dut_id.to_owned(), source);
        self.last_accepted.insert(key, now_ms);
        self.generation = self.generation.wrapping_add(1);
        if self.merge_duplicates
            && let Some(item) = self
                .items
                .iter_mut()
                .find(|item| item.dut_id == dut_id && item.source == source)
        {
            item.detail = detail.into();
            item.level = level;
            item.repeat = item.repeat.saturating_add(1);
            return Ok(());
        }
        while self.items.len() >= MAX_PENDING {
            let Some(position) = self.items.iter().position(|item| item.source == source) else {
                return Err(ReportRejection::Capacity);
            };
            self.items.remove(position);
        }
        self.next_id += 1;
        self.items.push(AlertItem {
            id: self.next_id,
            title: title.into(),
            detail: detail.into(),
            source,
            level,
            at: now_ms,
            dut_id: dut_id.to_owned(),
            repeat: 0,
        });
        Ok(())
    }

    /// Only the hidden-to-tray state merges repeats: while visible the queue
    /// keeps one item per accepted report (the dialog shows each event).
    pub fn set_merge_duplicates(&mut self, merge: bool) {
        self.merge_duplicates = merge;
    }

    /// Monotonic counter bumped whenever an alert is accepted or merged.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Newest presentable alert without popping it (tray balloon text/level).
    pub fn latest(&self) -> Option<AlertSummary> {
        self.items
            .iter()
            .rev()
            .find(|item| self.allows(&item.dut_id, item.source))
            .map(|item| AlertSummary {
                source: item.source,
                level: item.level,
                title: item.title.clone(),
                detail: item.detail.clone(),
            })
    }

    /// Build the runtime mirror from the global suppression list
    /// (`settings.alerts.suppressed_sources`; unknown keys are ignored for
    /// forward compatibility). Per-DUT switches arrive later through
    /// [`AlertCenter::set_dut_settings`].
    pub fn from_settings(suppressed_keys: &[String]) -> Self {
        Self {
            suppressed: suppressed_keys
                .iter()
                .filter_map(|k| AlertSource::from_key(k))
                .collect(),
            ..Self::default()
        }
    }

    /// Replace the global suppression mirror with `keys` (a settings import or
    /// a full-list write). Entries added *and* removed by the new list take
    /// effect at once, so the settings page and the runtime agree without a
    /// restart; unknown keys are ignored for forward compatibility.
    pub fn set_suppressed_sources(&mut self, keys: &[String]) {
        self.suppressed = keys
            .iter()
            .filter_map(|key| AlertSource::from_key(key))
            .collect();
    }

    /// Suppressing a certain type of source (checkbox labeled "Ignore This Type"). Returns whether
    /// any changes have occurred.
    pub fn suppress(&mut self, source: AlertSource) -> bool {
        self.suppressed.insert(source)
    }

    /// Disables restrictions from certain sources. On the settings page, only overall resets are
    /// available at present; individual items can be restored later.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "public alert suppression control retained for tests and future UI"
        )
    )]
    pub fn resume(&mut self, source: AlertSource) -> bool {
        self.suppressed.remove(&source)
    }

    /// The stable key of the current suppression list (used to update settings).
    /// Kept as the runtime mirror of the global list: the settings page renders
    /// `settings.alerts.suppressed_sources` directly, so no product caller
    /// reads this yet (it is part of the `AlertCenter` API contract).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "public alert suppression query retained as AlertCenter API contract"
        )
    )]
    pub fn suppressed_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .suppressed
            .iter()
            .map(|s| s.key().to_string())
            .collect();
        // Sequential stability (UI display is consistent with persistent settings).
        keys.sort();
        keys
    }

    /// Clear the suppression list (go to the "Reset" tab on the settings page). Check if there are
    /// any changes.
    #[cfg(test)]
    pub fn reset_suppressed(&mut self) -> bool {
        let had = !self.suppressed.is_empty();
        self.suppressed.clear();
        had
    }

    pub fn is_suppressed(&self, source: AlertSource) -> bool {
        self.suppressed.contains(&source)
    }

    /// Reporting a reminder (actual clock time). Returns whether a new entry was created (false for
    /// deduplication/suppression/closure cases).
    /// Reporting (clock time can be injected for testing purposes). Deduplication for the same
    /// source within 30 seconds.
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

    /// Legacy bool report with default warning level (kept verbatim so the
    /// existing `assert!` call sites and tests stay valid).
    fn report_at_for(
        &mut self,
        now_ms: u64,
        dut_id: &str,
        source: AlertSource,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> bool {
        self.report_checked_at_for(now_ms, dut_id, source, AlertLevel::Warning, title, detail)
            .is_ok()
    }

    /// Number of pop-up windows to be displayed (including those that are suppressed—they will be
    /// silently discarded when dequeued; used for diagnostics/testing purposes).
    pub fn pending_len(&self) -> usize {
        self.items.len()
    }

    /// Discard all pending items from a certain source (clear the queue after "ignore this type" to
    /// prevent repeated display in the next frame).
    pub fn drop_source(&mut self, source: AlertSource) -> usize {
        let before = self.items.len();
        self.items.retain(|item| item.source != source);
        before - self.items.len()
    }

    /// Exit the team and wait for a pop-up notification (skip and discard the entries that were
    /// suppressed).
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
// Source Access Point: Pure mapping of driver/engine output → (title, detail) (For single test
// coverage purposes.)
// ---------------------------------------------------------------------------

/// Access Point ①: Heartbeat Disconnection Alarm (`HeartbeatService::drain_alerts`) → Reminder
/// Message.
pub fn heartbeat_alert_texts(dut_label: &str, message: &str) -> (String, String) {
    (
        crate::i18n::label("heartbeatLostAlert").into(),
        crate::i18n::message(
            "heartbeatAlertDetail",
            &[("dut", dut_label), ("detail", message)],
        ),
    )
}

/// Access point ②: Failed to initiate the process. The error message is `[serial] Failed to open
/// {port} @ {baud}: {err}}`. After this error message is reported, the corresponding session task
/// terminates immediately. This constitutes the only observable signal indicating a "failure to
/// initiate the process = task termination". Reconnect/TX operations should not lead to false
/// positives.
pub fn parse_serial_open_failure(text: &str) -> Option<String> {
    let rest = text.strip_prefix("[serial] 打开 ")?;
    let at = rest.find(" 失败: ")?;
    Some(rest[..at].trim().to_string())
}

/// Access Point ③: Failure in writing operations for `ControlService` → Reminder message.
pub fn control_action_alert_texts(dut_label: &str, action: &str, err: &str) -> (String, String) {
    (
        crate::i18n::label("controlFailedAlert").into(),
        crate::i18n::message(
            "controlFailedDetail",
            &[("dut", dut_label), ("action", action), ("error", err)],
        ),
    )
}

/// Access point ⑤: Relay board offline monitoring (`duts.json.relayBoards`) → Reminder message.
/// The subject is the board (it is a shared device, not a DUT session).
pub fn relay_board_alert_texts(name: &str, detail: &str) -> (String, String) {
    (
        crate::i18n::label("relayBoardOffline").into(),
        crate::i18n::message(
            "relayBoardOfflineDetail",
            &[("board", name), ("detail", detail)],
        ),
    )
}

/// The dialog and native Windows notification share a single event headline.
pub fn alert_headline(source: AlertSource, title: &str, detail: &str) -> String {
    let text = if source == AlertSource::AgentNotice
        && title.trim() == crate::i18n::label("agentNoticeTitle")
    {
        detail
    } else {
        title
    };
    let text = text.trim().lines().next().unwrap_or_default();
    let mut words = text
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '（' | '）'))
        .filter(|word| !word.is_empty());
    let first = words.next().unwrap_or_default();
    let mut chars = first
        .chars()
        .chain(words.flat_map(|word| std::iter::once(' ').chain(word.chars())));
    let mut preview: String = chars.by_ref().take(96).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    if preview.is_empty() {
        source.label().to_owned()
    } else {
        preview
    }
}

/// Access Point ⑥: Server-side Agent notification (`notify_user`) → Reminder message.
pub fn agent_notice_alert_texts(title: Option<&str>, message: &str) -> (String, String) {
    let title = title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| crate::i18n::label("agentNoticeTitle"));
    (title.to_string(), message.to_string())
}

/// Alert texts for a disconnected HDMI / capture device.
pub fn media_disconnect_alert_texts(
    dut_name: &str,
    source_label: &str,
    device_name: &str,
) -> (String, String) {
    (
        crate::i18n::label("mediaDisconnectedAlert").into(),
        crate::i18n::message(
            "mediaDisconnectedDetail",
            &[
                ("dut", dut_name),
                ("source", source_label),
                ("device", device_name),
            ],
        ),
    )
}

/// Alert texts for a disconnected USB debug (ADB / HDC) device.
pub fn usb_debug_disconnect_alert_texts(
    dut_name: &str,
    tool: &str,
    serial: &str,
) -> (String, String) {
    (
        crate::i18n::label("usbDebugDisconnectedAlert").into(),
        crate::i18n::message(
            "usbDebugDisconnectedDetail",
            &[("dut", dut_name), ("tool", tool), ("serial", serial)],
        ),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn alert_headline_is_one_line_without_parentheses() {
        assert_eq!(
            alert_headline(
                AlertSource::Serial,
                "  Serial failure (COM8) （USB）\nmore",
                ""
            ),
            "Serial failure COM8 USB"
        );
        assert_eq!(
            alert_headline(
                AlertSource::AgentNotice,
                crate::i18n::label("agentNoticeTitle"),
                "Build complete\nMore detail"
            ),
            "Build complete"
        );
        let long_title = "继".repeat(100);
        let headline = alert_headline(AlertSource::Relay, &long_title, "");
        assert_eq!(headline.chars().count(), 97);
        assert!(headline.ends_with('…'));
    }

    use super::*;

    // Testing assistance: The default configuration is "all entities are enabled by default", which
    // corresponds to the product's startup path.
    fn new_center() -> AlertCenter {
        AlertCenter::default()
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
            "The first item must be enqueued"
        );
        // Redundant data within 30 seconds, based on the source (no duplicate entries allowed)
        assert!(!center.report_at(10_000, AlertSource::Control, "t2", "d2"));
        assert!(!center.report_at(30_999, AlertSource::Control, "t2", "d2"));
        assert_eq!(center.pending_len(), 1);
        // Reminders will be sent again after the window closes (continues to send reminders every
        // 30 seconds if there are continuous failures).
        assert!(center.report_at(31_000, AlertSource::Control, "t3", "d3"));
        assert_eq!(center.pending_len(), 2);
        // Different sources do not affect the deduplication window at all.
        assert!(center.report_at(31_500, AlertSource::Serial, "s", "d"));
        // Id is monotonic; the order of dequeuing equals the order of enqueueing.
        let first = center.pop_next().unwrap();
        let second = center.pop_next().unwrap();
        let third = center.pop_next().unwrap();
        assert!(second.id > first.id && third.id > second.id);
        assert_eq!(first.source, AlertSource::Control);
        assert_eq!(first.at, 1_000);
        assert_eq!(third.source, AlertSource::Serial);
        assert!(center.pop_next().is_none(), "The queue is empty.");
    }

    #[test]
    fn clear_dedup_for_allows_immediate_re_alert_after_recovery() {
        let mut center = new_center();
        assert!(center.report_at_for(1_000, "dut-1", AlertSource::Relay, "offline", "d1"));
        // Still within the 30s dedup window before recovery.
        assert!(!center.report_at_for(5_000, "dut-1", AlertSource::Relay, "offline", "d2"));
        // Recovery clears dedup only for the recovered (dut_id, source) pair.
        assert!(center.report_at_for(5_500, "dut-2", AlertSource::Relay, "offline", "d3"));
        center.clear_dedup_for("dut-1", AlertSource::Relay);
        assert!(center.report_at_for(6_000, "dut-1", AlertSource::Relay, "offline", "d4"));
        assert!(!center.report_at_for(6_500, "dut-2", AlertSource::Relay, "offline", "d5"));
    }

    #[test]
    fn suppress_blocks_reports_and_pending_items() {
        let mut center = new_center();
        assert!(center.report_at(0, AlertSource::Serial, "t", "d"));
        assert!(center.report_at(0, AlertSource::Agent, "t2", "d2"));
        center.suppress(AlertSource::Serial);
        // After suppression, it will no longer be enqueued with the same source
        assert!(
            !center.report_at(60_000, AlertSource::Serial, "t3", "d3"),
            "The suppressed source should not be enqueued."
        );
        // Source entries that have been enqueued are silently discarded when they are dequeued
        let next = center.pop_next().unwrap();
        assert_eq!(
            next.source,
            AlertSource::Agent,
            "Serial entries should be skipped."
        );
        assert!(center.pop_next().is_none());
        // Restoration after derestriction
        assert!(center.resume(AlertSource::Serial));
        assert!(center.report_at(61_000, AlertSource::Serial, "t4", "d4"));
        assert!(center.pop_next().is_some());
    }

    #[test]
    fn suppress_ignores_this_class_drops_queue_and_persists_keys() {
        let mut center = new_center();
        assert!(center.report_at(0, AlertSource::Control, "t", "d"));
        // After removing duplicates in the 30s window, the second element from the same source is
        // enqueued.
        assert!(center.report_at(31_000, AlertSource::Control, "t2", "d2"));
        // "Ignore such cases": Disables and clears the notifications related to that source.
        assert!(center.suppress(AlertSource::Control));
        assert_eq!(center.drop_source(AlertSource::Control), 2);
        assert!(center.pop_next().is_none());
        // Suppressing the list to stabilize persistence of keys (order-based stabilization). After
        // resetting, the list will be cleared.
        assert_eq!(center.suppressed_keys(), vec!["control".to_string()]);
        assert!(center.reset_suppressed());
        assert!(!center.reset_suppressed(), "Empty list, no changes made.");
        assert!(!center.is_suppressed(AlertSource::Control));
    }

    #[test]
    fn per_dut_switch_silences_every_source() {
        let mut center = new_center();
        let mut settings = BTreeMap::new();
        settings.insert(
            "dut-a".into(),
            duthub_core::model::AlertSettings {
                enabled: false,
                suppressed_sources: Vec::new(),
            },
        );
        center.set_dut_settings(&settings);
        for source in [
            AlertSource::Heartbeat,
            AlertSource::Serial,
            AlertSource::Control,
            AlertSource::Flash,
            AlertSource::Agent,
            AlertSource::Relay,
            AlertSource::AgentNotice,
            AlertSource::SerialGarbled,
            AlertSource::Media,
            AlertSource::UsbDebug,
        ] {
            assert!(
                !center.report_at_for(0, "dut-a", source, "t", "d"),
                "After the reminder is disabled, all sources of this DUT must be turned off."
            );
            assert!(
                center.report_at_for(0, "dut-b", source, "t", "d"),
                "DUTs without the \"alertsByDut\" entry are set to enabled by default."
            );
        }
        assert_eq!(center.pending_len(), 10);
        // Re-enabling the DUT lets it report again.
        let mut enabled = BTreeMap::new();
        enabled.insert(
            "dut-a".into(),
            duthub_core::model::AlertSettings {
                enabled: true,
                suppressed_sources: Vec::new(),
            },
        );
        center.set_dut_settings(&enabled);
        assert!(center.report_at_for(0, "dut-a", AlertSource::Heartbeat, "t", "d"));
    }

    #[test]
    fn from_settings_restores_suppressed_keys() {
        let mut center = AlertCenter::from_settings(&[
            "serial".to_string(),
            "agent".to_string(),
            "未知key".to_string(),
        ]);
        assert!(
            center.enabled_for("any-dut"),
            "By default, the switch for each DUT is set to \"on\". Thus, the global suppression mechanism no longer involves the overall switch."
        );
        assert!(center.is_suppressed(AlertSource::Serial));
        assert!(center.is_suppressed(AlertSource::Agent));
        assert!(
            !center.report_at(0, AlertSource::Serial, "t", "d"),
            "Persistence inhibition must continue to be effective after the system starts up."
        );
        assert!(center.report_at(0, AlertSource::Control, "t", "d"));
        // Unknown keys were ignored (to maintain backward compatibility). This did not lead to any
        // contamination of the rewrite results.
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
            AlertSource::Flash,
            AlertSource::Agent,
            AlertSource::Relay,
            AlertSource::AgentNotice,
            AlertSource::SerialGarbled,
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
        // Reconnection failed / Reconnection pending / TX / Normal output, no false alarms.
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
        assert_eq!(title, "心跳丢联");
        assert!(detail.contains("RK3576"));
        assert!(detail.contains("连续 3 次无响应"));
    }

    #[test]
    fn control_action_alert_texts_carry_action_and_error() {
        let (title, detail) = control_action_alert_texts("RK3576", "继电器 power ON", "无应答");
        assert_eq!(title, "控制动作失败");
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
        // If the clock is set back to the previous time (at > now), don't panic. Just treat it as
        // "just now".
        assert_eq!(format_alert_time(9_999_999, 1_000), "刚刚");
    }

    // ---- tray alert plumbing (level, checked reports, queue bounds) ----

    #[test]
    fn checked_reports_name_the_rejection_reason() {
        // A DUT with its own switch off wins over suppression/duplication.
        let mut off = new_center();
        let mut settings = BTreeMap::new();
        settings.insert(
            "dut-off".into(),
            duthub_core::model::AlertSettings {
                enabled: false,
                suppressed_sources: Vec::new(),
            },
        );
        off.set_dut_settings(&settings);
        assert_eq!(
            off.report_checked_for(
                "dut-off",
                AlertSource::Heartbeat,
                AlertLevel::Info,
                "t",
                "d"
            ),
            Err(ReportRejection::Disabled)
        );
        let mut center = new_center();
        center.suppress(AlertSource::Serial);
        assert_eq!(
            center.report_checked_for("", AlertSource::Serial, AlertLevel::Info, "t", "d"),
            Err(ReportRejection::Suppressed)
        );
        assert_eq!(
            center.report_checked_for("", AlertSource::Control, AlertLevel::Info, "t", "d"),
            Ok(())
        );
        assert_eq!(
            center.report_checked_for("", AlertSource::Control, AlertLevel::Info, "t", "d"),
            Err(ReportRejection::Duplicate)
        );
        // Capacity: one source at its quota refuses more, others still queue.
        for index in 0..MAX_PENDING_PER_SOURCE {
            assert_eq!(
                center.report_checked_for(
                    &format!("dut-{index}"),
                    AlertSource::AgentNotice,
                    AlertLevel::Info,
                    "t",
                    "d",
                ),
                Ok(())
            );
        }
        assert_eq!(
            center.report_checked_for(
                "dut-overflow",
                AlertSource::AgentNotice,
                AlertLevel::Info,
                "t",
                "d"
            ),
            Err(ReportRejection::Capacity)
        );
        assert_eq!(
            center.report_checked_for(
                "dut-other",
                AlertSource::Heartbeat,
                AlertLevel::Info,
                "t",
                "d"
            ),
            Ok(())
        );
    }

    #[test]
    fn enabled_for_defaults_on_and_follows_per_dut_switch() {
        let mut center = new_center();
        assert!(
            center.enabled_for("dut-a"),
            "DUT is saved, and the feature is enabled by default."
        );
        let mut settings = BTreeMap::new();
        settings.insert(
            "dut-a".into(),
            duthub_core::model::AlertSettings {
                enabled: false,
                suppressed_sources: Vec::new(),
            },
        );
        center.set_dut_settings(&settings);
        assert!(!center.enabled_for("dut-a"));
        assert!(center.enabled_for("dut-b"));
        assert!(center.enabled_for(""));
        assert!(!center.is_suppressed_for("unknown-board", AlertSource::Relay));
        center.suppress(AlertSource::Relay);
        assert!(center.is_suppressed_for("unknown-board", AlertSource::Relay));
        assert!(!center.is_suppressed_for("dut-a", AlertSource::Relay));
    }

    #[test]
    fn non_dut_subjects_stay_enabled_and_only_suppressible() {
        let mut center = new_center();
        // Relay boards and generic notices have no per-DUT settings row: with
        // the global switch gone they are always on and can only be muted
        // per source (settings page -> global suppression list).
        assert!(center.enabled_for("board-1"));
        assert!(center.enabled_for(""));
        center.suppress(AlertSource::Relay);
        center.suppress(AlertSource::AgentNotice);
        assert_eq!(
            center.report_checked_for("board-1", AlertSource::Relay, AlertLevel::Warning, "t", "d"),
            Err(ReportRejection::Suppressed)
        );
        assert_eq!(
            center.report_checked_for("", AlertSource::AgentNotice, AlertLevel::Info, "t", "d"),
            Err(ReportRejection::Suppressed)
        );
        // Other sources on the same subject are unaffected.
        assert!(
            center
                .report_checked_for(
                    "board-1",
                    AlertSource::Heartbeat,
                    AlertLevel::Warning,
                    "t",
                    "d"
                )
                .is_ok()
        );
    }

    #[test]
    fn latest_is_read_only_and_carries_the_level() {
        let mut center = new_center();
        assert!(center.latest().is_none());
        assert!(
            center
                .report_checked_for("", AlertSource::AgentNotice, AlertLevel::Error, "T", "D")
                .is_ok()
        );
        let pending = center.pending_len();
        let summary = center.latest().expect("queued notice");
        assert_eq!(summary.source, AlertSource::AgentNotice);
        assert_eq!(summary.level, AlertLevel::Error);
        assert_eq!(summary.title, "T");
        assert_eq!(summary.detail, "D");
        assert_eq!(center.pending_len(), pending, "latest() must not pop");
        assert!(center.pop_next().is_some());
        assert!(center.latest().is_none());
    }

    #[test]
    fn per_source_capacity_never_evicts_another_source() {
        let mut center = new_center();
        // Fill Heartbeat up to one below its quota.
        for index in 0..MAX_PENDING_PER_SOURCE - 1 {
            assert!(
                center
                    .report_checked_at_for(
                        0,
                        &format!("hb-{index}"),
                        AlertSource::Heartbeat,
                        AlertLevel::Warning,
                        "t",
                        "d",
                    )
                    .is_ok()
            );
        }
        // AgentNotice fills its own quota; it must not push Heartbeat out.
        for index in 0..MAX_PENDING_PER_SOURCE {
            assert!(
                center
                    .report_checked_at_for(
                        0,
                        &format!("notice-{index}"),
                        AlertSource::AgentNotice,
                        AlertLevel::Info,
                        "t",
                        "d",
                    )
                    .is_ok()
            );
        }
        assert_eq!(
            center.report_checked_at_for(
                0,
                "notice-overflow",
                AlertSource::AgentNotice,
                AlertLevel::Info,
                "t",
                "d",
            ),
            Err(ReportRejection::Capacity),
            "a full source refuses instead of evicting another source"
        );
        // Heartbeat still has one slot of its own.
        assert!(
            center
                .report_checked_at_for(
                    0,
                    "hb-late",
                    AlertSource::Heartbeat,
                    AlertLevel::Warning,
                    "t",
                    "d",
                )
                .is_ok()
        );
        let heartbeat_items = center
            .items
            .iter()
            .filter(|item| item.source == AlertSource::Heartbeat)
            .count();
        assert_eq!(heartbeat_items, MAX_PENDING_PER_SOURCE);
        let notice_items = center
            .items
            .iter()
            .filter(|item| item.source == AlertSource::AgentNotice)
            .count();
        assert_eq!(notice_items, MAX_PENDING_PER_SOURCE);
    }

    #[test]
    fn global_capacity_evicts_only_within_the_same_source() {
        let mut center = new_center();
        let filler = [
            (AlertSource::Serial, 16),
            (AlertSource::Control, 16),
            (AlertSource::Flash, 16),
            (AlertSource::Agent, 15),
        ];
        for (source, count) in filler {
            for index in 0..count {
                assert!(
                    center
                        .report_checked_at_for(
                            0,
                            &format!("{}-{index}", source.key()),
                            source,
                            AlertLevel::Warning,
                            "t",
                            "d",
                        )
                        .is_ok()
                );
            }
        }
        // Heartbeat (1 item) + filler = MAX_PENDING; another Heartbeat evicts
        // the oldest Heartbeat only.
        assert!(
            center
                .report_checked_at_for(
                    0,
                    "hb-old",
                    AlertSource::Heartbeat,
                    AlertLevel::Warning,
                    "t",
                    "d"
                )
                .is_ok()
        );
        assert_eq!(center.pending_len(), MAX_PENDING);
        assert!(
            center
                .report_checked_at_for(
                    0,
                    "hb-new",
                    AlertSource::Heartbeat,
                    AlertLevel::Warning,
                    "t",
                    "d"
                )
                .is_ok()
        );
        assert_eq!(center.pending_len(), MAX_PENDING);
        assert!(
            !center
                .items
                .iter()
                .any(|item| item.dut_id == "hb-old" && item.source == AlertSource::Heartbeat)
        );
        assert!(
            center
                .items
                .iter()
                .any(|item| item.dut_id == "hb-new" && item.source == AlertSource::Heartbeat)
        );
        // A source with zero pending items cannot enter a full queue.
        assert_eq!(
            center.report_checked_at_for(
                0,
                "relay-1",
                AlertSource::Relay,
                AlertLevel::Warning,
                "t",
                "d",
            ),
            Err(ReportRejection::Capacity)
        );
    }

    #[test]
    fn hidden_merge_folds_repeats_into_one_item() {
        let mut center = new_center();
        center.set_merge_duplicates(true);
        assert!(
            center
                .report_checked_at_for(
                    0,
                    "dut-a",
                    AlertSource::Heartbeat,
                    AlertLevel::Warning,
                    "lost",
                    "first",
                )
                .is_ok()
        );
        let generation = center.generation();
        assert!(
            center
                .report_checked_at_for(
                    31_000,
                    "dut-a",
                    AlertSource::Heartbeat,
                    AlertLevel::Error,
                    "lost",
                    "second",
                )
                .is_ok()
        );
        assert_eq!(center.pending_len(), 1);
        let item = center.pop_next().expect("merged item");
        assert_eq!(item.at, 0, "earliest timestamp is kept");
        assert_eq!(item.detail, "second", "detail is refreshed");
        assert_eq!(item.repeat, 1);
        assert_eq!(item.level, AlertLevel::Error);
        assert_eq!(center.generation(), generation + 1);
    }

    #[test]
    fn visible_state_does_not_merge() {
        let mut center = new_center();
        assert!(center.report_at(0, AlertSource::Control, "t", "first"));
        assert!(center.report_at(31_000, AlertSource::Control, "t", "second"));
        assert_eq!(center.pending_len(), 2);
        assert_eq!(center.pop_next().unwrap().repeat, 0);
    }

    #[test]
    fn global_suppression_roundtrip() {
        let mut center = AlertCenter::from_settings(&["relay".to_string()]);
        assert!(center.is_suppressed(AlertSource::Relay));
        assert_eq!(
            center.report_checked_for("", AlertSource::Relay, AlertLevel::Warning, "t", "d"),
            Err(ReportRejection::Suppressed)
        );
        assert!(center.reset_suppressed());
        assert!(!center.is_suppressed(AlertSource::Relay));
        assert!(
            center
                .report_checked_for("", AlertSource::Relay, AlertLevel::Warning, "t", "d")
                .is_ok()
        );
        assert!(center.suppress(AlertSource::AgentNotice));
        assert_eq!(center.suppressed_keys(), vec!["agentNotice".to_string()]);
    }

    #[test]
    fn replacing_the_global_list_adds_and_removes_sources() {
        let mut center = AlertCenter::from_settings(&["relay".to_string()]);
        assert!(center.is_suppressed(AlertSource::Relay));
        // A settings import that adds a source and drops the old one applies
        // both directions at once (no restart needed).
        center.set_suppressed_sources(&["agentNotice".into(), "未知key".into()]);
        assert!(!center.is_suppressed(AlertSource::Relay));
        assert!(center.is_suppressed(AlertSource::AgentNotice));
        assert_eq!(
            center.report_checked_for("", AlertSource::Relay, AlertLevel::Warning, "t", "d"),
            Ok(())
        );
        assert_eq!(
            center.report_checked_for("", AlertSource::AgentNotice, AlertLevel::Warning, "t", "d"),
            Err(ReportRejection::Suppressed)
        );
        // Unknown keys are ignored and do not leak into the mirror.
        assert_eq!(center.suppressed_keys(), vec!["agentNotice".to_string()]);
        // An empty list clears the mirror (import of a file without suppression).
        center.set_suppressed_sources(&[]);
        assert_eq!(center.suppressed_keys(), Vec::<String>::new());
    }

    #[test]
    fn relay_and_notice_texts_are_localized_pairs() {
        let (title, detail) = relay_board_alert_texts("板卡 A", "串口已消失");
        assert_ne!(title, "");
        assert!(detail.contains("板卡 A"));
        assert!(detail.contains("串口已消失"));
        let (title, detail) = agent_notice_alert_texts(None, "构建完成");
        assert_ne!(title, "");
        assert_eq!(detail, "构建完成");
        let (title, _) = agent_notice_alert_texts(Some("自定义标题"), "x");
        assert_eq!(title, "自定义标题");
    }
}
