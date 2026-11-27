//! Workbench shell based on the prototype serial page. It contains the tab bar, find bar, terminal,
//! command dock, Agent panel, and status bar. Menus, dialogs, settings, and toasts render as
//! overlays.
//!
//! Input system: `In` – Enumerates fields for setting up text editing using the gpui-kit component;
//! Drop-down menu: `Dd`/select id → `open_dd`;
//! Pop-up window: `ModalState`;
//! Notifications: Kit-specific notifications (at the root level).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use duthub_adb::AdbClient;
use duthub_core::ipc::{ChunkStyle, GarbleLevel, UiEvent};
use duthub_core::model::{FlashConfig, QuickCommand};
use duthub_core::store;
use duthub_core::{AdbDevice, DutAction, DutGrant, SerialPortInfo};
use duthub_desktop::agent_host;
use duthub_desktop::agent_host::BindingSource;
use duthub_desktop::tool_ui::{InstallFinishedOutcome, ToolPurpose};
use duthub_desktop::tray_lifecycle::{
    LifecycleAction, LifecycleEvent, LifecyclePhase, LifecycleState, LifecycleTransition,
};
use duthub_desktop::tray_ui::{
    AlertPresentation, CloseStep, HiddenAlertAction, HiddenAlertEnv, HiddenAlertRouter,
};
use duthub_device::{DeviceService, broadcast};
use gpui_kit::component::Disableable;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AppContext, Axis, Context, Div, DragMoveEvent, Empty, Entity, FocusHandle, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled,
    TextRun, Window, WindowControlArea, div, img, px,
};

use crate::agent::AgentState;
use crate::alerts::{AlertCenter, AlertItem, AlertSource};
use crate::boot_learning::boot_reference_path;
use crate::components::{icon_btn, typed_char};
use crate::data::DutsState;
use crate::i18n::Lang;
use crate::icons::{icon, kit_icon};
use crate::pages::{render_connection, render_flash_tools, render_settings, render_usb_debug};
use crate::theme::Palette;
use crate::{agent, i18n, theme};
use gpui_kit::component::WindowExt;
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{HandleEdge, ResizeHandle, resize_handle};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};

type FontSelectState = SelectState<SearchableVec<String>>;
type HighlightRulesCache = Option<(
    Vec<duthub_core::model::PromptRule>,
    Arc<Vec<crate::highlight::CompiledRule>>,
)>;

struct FontPicker {
    lang: Lang,
    available: Vec<String>,
    state: Entity<FontSelectState>,
    _subscription: gpui_kit::Subscription,
}

const DEFAULT_TERMINAL_FONT_SIZE: f32 = 12.5;
pub(crate) const MIN_TERMINAL_FONT_SIZE: f32 = 7.5;
pub(crate) const MAX_TERMINAL_FONT_SIZE: f32 = 48.0;

/// Toast queue bound: while hidden there is no render loop to drain it, so an
/// unbounded queue would only be discovered on restore. Oldest are dropped and
/// counted; the first restored frame reports how many.
const MAX_TOASTS: usize = 200;
/// MCP snapshot refresh cadence in the 50 ms tick. `refresh_snapshot` clones
/// each session's log window, so it must not run at tick rate.
const SNAPSHOT_INTERVAL_MS: u64 = 250;

const CENTER_PANE_MIN_WIDTH: Pixels = px(260.);

#[derive(Clone, Copy)]
enum PaneDivider {
    Connection,
    Tools,
}

impl PaneDivider {
    fn handle(self) -> ResizeHandle<Self, Empty> {
        let (id, edge) = match self {
            Self::Connection => ("connection-divider", HandleEdge::Trailing),
            Self::Tools => ("tools-divider", HandleEdge::Leading),
        };
        resize_handle(id, Axis::Horizontal)
            .inside(edge)
            .on_drag(self, |_, _, _, cx| cx.new(|_| Empty))
    }
}

fn workspace_pane_widths(
    viewport: Pixels,
    connection: Option<Pixels>,
    tools: Option<Pixels>,
) -> (Pixels, Pixels) {
    let budget = (viewport - CENTER_PANE_MIN_WIDTH).max(px(0.));
    let left_min = px(300.).min(if tools.is_some() {
        budget * 0.55
    } else {
        budget
    });
    let right_min = px(220.).min(if connection.is_some() {
        budget - left_min
    } else {
        budget
    });
    let left = connection.map_or(px(0.), |wanted| {
        wanted.clamp(
            left_min,
            budget - if tools.is_some() { right_min } else { px(0.) },
        )
    });
    let right = tools.map_or(px(0.), |wanted| wanted.clamp(right_min, budget - left));
    (left, right)
}

#[cfg(test)]
mod pane_width_tests {
    use super::{CENTER_PANE_MIN_WIDTH, px, workspace_pane_widths};

    #[test]
    fn sidebars_share_only_the_space_outside_the_center_pane() {
        let width = px(1000.);
        let (left, right) = workspace_pane_widths(width, Some(px(800.)), Some(px(500.)));
        assert_eq!(right, px(220.));
        assert_eq!(width - left - right, CENTER_PANE_MIN_WIDTH);
        let (left, right) = workspace_pane_widths(width, None, Some(px(800.)));
        assert_eq!(left, px(0.));
        assert_eq!(right, width - CENTER_PANE_MIN_WIDTH);
    }

    #[test]
    fn narrow_windows_shrink_both_sidebars_with_the_center_visible() {
        let width = px(640.);
        let (left, right) = workspace_pane_widths(width, Some(px(500.)), Some(px(440.)));
        assert!(left > px(0.) && right > px(0.));
        assert_eq!(width - left - right, CENTER_PANE_MIN_WIDTH);
    }
}

fn configured_terminal_font_size(settings: &duthub_core::model::TerminalSettings) -> f32 {
    settings
        .font_size
        .unwrap_or(DEFAULT_TERMINAL_FONT_SIZE)
        .clamp(MIN_TERMINAL_FONT_SIZE, MAX_TERMINAL_FONT_SIZE)
}

/// Convert a provider to the stable `settings.agent.provider` value accepted by
/// `Provider::from_setting`.
fn provider_setting(provider: duthub_core::Provider) -> &'static str {
    match provider {
        duthub_core::Provider::Openai => "openai",
        duthub_core::Provider::OpenaiResponses => "openai_responses",
        duthub_core::Provider::Anthropic => "anthropic",
        duthub_core::Provider::Gemini => "google",
        duthub_core::Provider::Deepseek => "deepseek",
        duthub_core::Provider::Qwen => "qwen",
        duthub_core::Provider::Compatible => "compatible",
    }
}

/// `GoosePath` Input Display Value: Draft is prioritized. **An empty string draft is also a valid
/// draft** – this represents a recorded external path. It is not allowed to restore the input after
/// the user has cleared it to the old value (otherwise, "Clear → Verify and Save" will re-verify
/// the old path).
fn goose_path_field_value(draft: Option<&str>, recorded: Option<&str>) -> String {
    draft.map_or_else(|| recorded.unwrap_or_default().to_owned(), str::to_owned)
}

#[cfg(test)]
mod goose_path_tests {
    use super::goose_path_field_value;

    #[test]
    fn empty_draft_wins_over_recorded_path_so_clearing_sticks() {
        // Reproduced the issue that occurred before the fix: The recorded paths are cleared,
        // resulting in the inability to restore the original paths.
        assert_eq!(
            goose_path_field_value(Some(""), Some("C:/old/goose.exe")),
            ""
        );
        // The recorded path is echoed only when there is no draft available (first rendering).
        assert_eq!(
            goose_path_field_value(None, Some("C:/old/goose.exe")),
            "C:/old/goose.exe"
        );
        // Drafts that are not empty always take precedence.
        assert_eq!(
            goose_path_field_value(Some("D:/new/goose.exe"), Some("C:/old/goose.exe")),
            "D:/new/goose.exe"
        );
        assert_eq!(goose_path_field_value(None, None), "");
    }
}

// ---------------------------------------------------------------------------
// Enter and select from the dropdown options.
// ---------------------------------------------------------------------------

struct BoundEditor<T: 'static> {
    state: gpui_kit::Entity<T>,
    model_value: String,
    placeholder: String,
    scope: Option<String>,
    _subscription: gpui_kit::Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[expect(
    dead_code,
    reason = "variants include fields reserved for terminal prompt prefix"
)]
pub enum In {
    Find,
    CfgSearch,
    Target,
    Prompt,
    /// Agent Panel "Model Name" (ID of the model invoked by the agent/task)
    AgentModel,
    /// Draft of "External Goose Path" in Agent settings page (pit #`41: input_value/input_set`
    /// pairs)
    GoosePath,
    /// Agent Page Registry Editor: Name/Program/Parameters/Environment Variable Key Names
    AgentRegName,
    AgentRegProgram,
    AgentRegArgs,
    AgentRegEnvKeys,
    /// Registry Editor: env value mask input (W4b) – Values are only added to the keyring, not to
    /// agents.json.
    AgentRegEnvValue,
    Port,
    Baud,
    /// Input for hot-switching the baud rate of the session status bar (independent of active
    /// sessions)
    SessionBaud,
    AdbSerial,
    DutName,
    BootReference,
    BootSimilarity,
    RelayNode,
    RelayBoardName,
    RelayBoardNode,
    PduAddress,
    PduCredential,
    McpUrl,
    McpCommand,
    McpArgs,
    McpCwd,
    McpEnv,
    /// Serial Port Settings Page: The highlighted rules selected for modification (the rules
    /// pointed to by serial.serialRuleSelected)
    SerialRuleRegex,
    SerialRuleDescription,
    /// Serial Port Settings Page "Number of Scroll Buffer Lines" Draft (Save button submitted,
    /// Baud-style parsing)
    SerialScrollback,
    /// Capture settings page: idle-reclaim seconds draft
    /// (`tools.capture.idleSecs`; empty = default 60, committed by its save
    /// button after validation)
    CaptureIdle,
    AdbExe,
    HdcExe,
    WorkDir,
    DutStorageGib,
    /// Connection form field for `dut.heartbeat.interval_secs`.
    HeartbeatInterval,
    /// Connection page "Heartbeat Command" (dut.heartbeat.command; can be modified by heartbeat.set
    /// in Agent)
    HeartbeatCommand,
    HeartbeatExpectedResponse,
    /// Set the "Working Directory" on the settings page (input: settings.`workspace_dir`; save and
    /// restart for effective changes).
    /// Set the "Agent Integration" on the settings page: listening port / lease TTL (draft input,
    /// submit with the save button).
    AgentAccessPort,
    AgentAccessExtraPort,
    AgentAccessTtl,
    /// "Agent Access" adds the Agent name of the credentials (draft; token is generated by the
    /// server).
    AgentAccessCredName,
    /// Connection Management: Dedicated MCP Port for DUT (draft; empty = automatically assigned)
    DutMcpPort,
    /// Connection Management: Dedicated A2A Port for DUT / Agent Name (draft, submit with the save
    /// button)
    DutA2aPort,
    DutA2aName,
    /// Draft of the "Download Proxy" custom URL for the settings page (written after the save
    /// button verification is completed. See issue #41 for more details).
    ProxyUrl,
    ToolDownloadUrl(&'static str),
    Media(crate::media_page::MediaField),
    A2aName,
    A2aPort,
    CmdName,
    CmdValue,
    /// Create new shortcut commands: Icon name (draft, empty = bolt)
    CmdIcon,
    NewPresetName,
    NewPresetExe,
    /// Create new burning preset: Test parameters (separated by spaces; empty = probe fallback
    /// --version)
    NewPresetTestArgs,
    PeerName,
    PeerUrl,
    RenameName,
    // ＋ Create a new DUT form (with all configurations).
}

impl In {
    fn key(self) -> &'static str {
        match self {
            In::Find => "find",
            In::CfgSearch => "cfg-search",
            In::Target => "target",
            In::Prompt => "prompt",
            In::AgentModel => "agent-model",
            In::GoosePath => "goose-path",
            In::AgentRegName => "agent-reg-name",
            In::AgentRegProgram => "agent-reg-program",
            In::AgentRegArgs => "agent-reg-args",
            In::AgentRegEnvKeys => "agent-reg-env-keys",
            In::AgentRegEnvValue => "agent-reg-env-value",
            In::Port => "port",
            In::Baud => "baud",
            In::SessionBaud => "session-baud",
            In::AdbSerial => "adb-serial",
            In::DutName => "dut-name",
            In::BootReference => "boot-reference",
            In::BootSimilarity => "boot-similarity",
            In::RelayNode => "relay-node",
            In::RelayBoardName => "relay-board-name",
            In::RelayBoardNode => "relay-board-node",
            In::PduAddress => "pdu-address",
            In::PduCredential => "pdu-credential",
            In::McpUrl => "mcp-url",
            In::McpCommand => "mcp-command",
            In::McpArgs => "mcp-args",
            In::McpCwd => "mcp-cwd",
            In::McpEnv => "mcp-env",
            In::SerialRuleRegex => "serial-rule-regex",
            In::SerialRuleDescription => "serial-rule-description",
            In::SerialScrollback => "serial-scrollback",
            In::CaptureIdle => "capture-idle",
            In::AdbExe => "adb-exe",
            In::HdcExe => "hdc-exe",
            In::WorkDir => "work-dir",
            In::DutStorageGib => "dut-storage-gib",
            In::HeartbeatInterval => "hb-interval",
            In::HeartbeatCommand => "hb-command",
            In::HeartbeatExpectedResponse => "hb-expected-response",
            In::AgentAccessPort => "aa-port",
            In::AgentAccessExtraPort => "aa-extra-port",
            In::AgentAccessTtl => "aa-ttl",
            In::AgentAccessCredName => "aa-cred-name",
            In::DutMcpPort => "dut-mcp-port",
            In::DutA2aPort => "dut-a2a-port",
            In::DutA2aName => "dut-a2a-name",
            In::ProxyUrl => "tool-proxy-url",
            In::ToolDownloadUrl(tool) => tool,
            In::Media(field) => field.key(),
            In::A2aName => "a2a-name",
            In::A2aPort => "a2a-port",
            In::CmdName => "cmd-name",
            In::CmdValue => "cmd-value",
            In::CmdIcon => "cmd-icon",
            In::NewPresetName => "new-preset-name",
            In::NewPresetExe => "new-preset-exe",
            In::NewPresetTestArgs => "new-preset-test-args",
            In::PeerName => "peer-name",
            In::PeerUrl => "peer-url",
            In::RenameName => "rename-name",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dd {
    Port,
    RelayBoardPort,
    Baud,
    AdbSerial,
    CaptureMode(crate::media_page::MediaField),
}

impl Dd {
    pub(crate) fn key(self) -> std::borrow::Cow<'static, str> {
        match self {
            Dd::Port => "dd-port".into(),
            Dd::RelayBoardPort => "dd-relay-board-port".into(),
            Dd::Baud => "dd-baud".into(),
            Dd::AdbSerial => "dd-adb".into(),
            Dd::CaptureMode(field) => field.control_key(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPage {
    AgentModel,
    Flash,
    Adb,
    Relay,
    /// Serial Port Settings (Same level as the control device): Highlighted rules / Keyboard
    /// mapping / Buffer / Log
    Serial,
    Media,
    /// Tool Download Page (External tool installation/update functions + download proxy,
    /// independent of the settings page)
    ToolDownloads,
    Settings,
    /// Product and build information.
    About,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalKind {
    RemoveDut(String),
    NewSession,
    NewCommand,
    NewPreset,
    EditPreset(String),
    NewPeer,
    /// Confirm deleting a shared relay board by board ID.
    RemoveControlDevice(String),
}

#[derive(Clone)]
pub struct ModalState {
    pub kind: ModalKind,
    pub draft: String,
    pub draft2: String,
    /// The third modal field (shortcut command icons / burning preset test parameters).
    pub draft3: String,
}

// ---------------------------------------------------------------------------
// Terminal Session View
// ---------------------------------------------------------------------------

/// Session link status (displayed in H1.4 reconnection mode): Determined based on the [serial]
/// column of the device session and any disconnection events that occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Connected,
    Reconnecting,
}

pub struct SessionView {
    pub dut_id: String,
    pub session_id: String,
    pub title: String,
    pub source: &'static str,
    pub lines: Vec<LogLine>,
    pub last_seq: u64,
    /// Link status (only applicable for device session tracking; in the demo, it always remains
    /// "Connected").
    pub link: LinkState,
    /// Reconnection count (follows the "Nth reconnection" indicator; resets to zero after
    /// successful activation).
    pub reconnect_attempts: u32,
    /// Rate confirmed on device side (from line "[serial] … @ N is open"; 0 = not confirmed)
    pub baud: u32,
    /// Serial port mode switched to draft (`None` = displays current value)
    pub baud_draft: Option<String>,
    /// The snapshot of the terminal grid for real sessions (simulation using alacritty; rendering
    /// source for `PuTTY` behavior). In the `SerialChunk` of session devices, only annotation lines
    /// ([serial]/[mark]/errors) are recorded, and device output is recorded as grid lines here. Arc
    /// sharing: absorbs zero-depth copies of events, rendering is read-only.
    pub screen: Option<std::sync::Arc<duthub_core::ipc::ScreenSnapshot>>,
    scrollback: Option<terminal::TerminalScrollback>,
    scrollback_pending: Option<uuid::Uuid>,
    /// Monotonic counter incremented whenever new terminal output (grid snapshot or annotation
    /// line) arrives for this session, used to gate auto-scroll on output.
    output_seq: u64,
    /// Value of `output_seq` already observed by `follow_terminal_tail`.
    last_followed_seq: u64,
    /// Explicit one-shot request to scroll to the bottom on the next frame (e.g. keypress jump,
    /// terminal reset, or tab activation).
    scroll_to_bottom_pending: bool,
    /// Latest garbled-byte level from the device snapshots (tab warning dot and
    /// the alert-edge check). Only serial sessions fill it; demo stays Clean.
    pub last_garbled_level: GarbleLevel,
    crash_line_tail: String,
    /// Active or dragging mouse text selection for this session.
    pub selection: Option<crate::terminal_selection::TerminalSelection>,
}

impl SessionView {
    fn display_title(&self, duts: &[duthub_core::model::DutConfig]) -> String {
        duts.iter()
            .find(|dut| dut.id == self.dut_id)
            .filter(|dut| !dut.name.trim().is_empty())
            .map_or_else(
                || self.title.clone(),
                |dut| format!("{} · {}", dut.name.trim(), dut.serial.port),
            )
    }

    fn new(
        dut_id: String,
        session_id: String,
        title: String,
        source: &'static str,
        baud: u32,
    ) -> Self {
        Self {
            dut_id,
            session_id,
            title,
            source,
            lines: Vec::new(),
            last_seq: 0,
            link: LinkState::Connected,
            reconnect_attempts: 0,
            baud,
            baud_draft: None,
            screen: None,
            scrollback: None,
            scrollback_pending: None,
            output_seq: 0,
            last_followed_seq: 0,
            scroll_to_bottom_pending: true,
            last_garbled_level: GarbleLevel::Clean,
            crash_line_tail: String::new(),
            selection: None,
        }
    }

    /// Snapshot of the entire screen is displayed in the view (rendering source layer, issue
    /// #22/#49): The entire device session is replaced.
    /// Screen Arc (zero-copy, no lines added—device output comes only from grid snapshots).
    /// The demo session does not have a simulator, so snapshots are ignored, and full-line
    /// rendering is maintained.
    fn absorb_serial_screen(&mut self, screen: Arc<duthub_core::ipc::ScreenSnapshot>, seq: u64) {
        if self.source == "device" {
            self.last_garbled_level = screen.garbled.level;
            self.screen = Some(screen);
            self.last_seq = seq;
            self.output_seq = self.output_seq.wrapping_add(1);
        }
    }

    /// `SerialChunk` → routing determination for lines (device session `PuTTY` hierarchical
    /// structure): device output
    /// (Normal/Good/Prompt) rendered by emulator grid; should not be repeated in the annotation
    /// list;
    /// Signaling lines with "[serial] … @ N opened" are also not included in the list after being
    /// consumed in the link state;
    /// The demo session maintains full-line rendering (returning true = enqueue).
    fn should_queue_chunk_line(&self, style: ChunkStyle, open_baud_signaling: bool) -> bool {
        if self.source != "device" {
            return true;
        }
        let annotation = !matches!(
            style,
            ChunkStyle::Normal | ChunkStyle::Good | ChunkStyle::Prompt
        );
        !open_baud_signaling && annotation
    }
}

/// The baud rate confirmed by the device side was determined from the line "[serial] {port} @
/// {baud} is open".
fn parse_serial_open_baud(text: &str) -> Option<u32> {
    let rest = text.strip_prefix("[serial] ")?;
    let at = rest.find(" @ ")?;
    let tail = &rest[at + 3..];
    let end = tail.find(" 已打开")?;
    tail[..end].parse().ok()
}

/// The byte at the end of the keyboard input (serial port settings section: serial.enterEnding
/// driver; None/Cr = CR 0x0d, Lf/Crlf optional). The return value represents the complete byte
/// enqueued – no additional line endings are appended for the keyboard path. This rule only applies
/// to the keyboard path; the line ending strategy for MCP/heartbeat writes is independently
/// configured at the session level and does not go through this process.
fn serial_enter_bytes(ending: Option<duthub_core::model::SerialEnterEnding>) -> &'static [u8] {
    match ending {
        Some(duthub_core::model::SerialEnterEnding::Lf) => b"\n",
        Some(duthub_core::model::SerialEnterEnding::Crlf) => b"\r\n",
        _ => b"\r",
    }
}

/// Keyboard backspace byte (serial port setting segment: serial.backspace driver; None/Del = DEL
/// 0x7f, `CtrlH` = 0x08, required for some BSP shells/old tools).
fn serial_backspace_byte(mode: Option<duthub_core::model::SerialBackspace>) -> u8 {
    match mode {
        Some(duthub_core::model::SerialBackspace::CtrlH) => 0x08,
        _ => 0x7f,
    }
}

/// Escape prefix for direction keys/home/end: Application cursor mode (DECSET ?1, snapshot mode
/// bit) uses SS3 'O', while in normal mode it uses CSI '['. This mode is followed by terminals that
/// switch between ANSI modes; it is not hard-coded to override.
fn arrow_escape_prefix(application_cursor: bool) -> u8 {
    if application_cursor { b'O' } else { b'[' }
}

/// Terminalized text pasting (`PuTTY` behavior): The end of each line is uniformly terminated with CR
/// (multiple lines are pasted by pressing Enter line by line). Other control characters are removed
/// (only \t and CR are retained). When bracketed-paste is enabled for the device (DECSET?2004),
/// \x1b[200~/201~ wrapping is used, allowing the shell to receive the text as it is, rather than
/// interpreting each character individually. After cleaning, if there is nothing to return, `None`
/// is returned (no content is sent).
pub(crate) fn prepare_paste_bytes(raw: &str, bracketed_paste: bool) -> Option<Vec<u8>> {
    let mut text = raw.replace("\r\n", "\r").replace('\n', "\r");
    text.retain(|ch| !ch.is_control() || matches!(ch, '\r' | '\t'));
    if text.is_empty() {
        return None;
    }
    if bracketed_paste {
        text = format!("\x1b[200~{text}\x1b[201~");
    }
    Some(text.into_bytes())
}

/// Returns the terminal text selection highlight background for the active theme.
pub(crate) fn terminal_selection_bg(p: &Palette, mode: theme::ThemeMode) -> gpui_kit::Rgba {
    let alpha = match mode {
        theme::ThemeMode::Dark => 0.36,
        theme::ThemeMode::Light => 0.24,
    };
    gpui_kit::Rgba {
        r: p.blue.r,
        g: p.blue.g,
        b: p.blue.b,
        a: alpha,
    }
}

/// Maps a horizontal pixel offset inside a rendered terminal row to a character
/// offset using GPUI's line shaper so ASCII, CJK, and emoji align accurately.
pub(crate) fn terminal_col_for_x(
    row_text: &str,
    rel_x: Pixels,
    font_family: &SharedString,
    font_size: Pixels,
    closest: bool,
    window: &Window,
) -> usize {
    if row_text.is_empty() || rel_x <= px(0.) {
        return 0;
    }
    let clean: std::borrow::Cow<'_, str> = if row_text.contains('\n') {
        std::borrow::Cow::Owned(row_text.replace('\n', " "))
    } else {
        std::borrow::Cow::Borrowed(row_text)
    };
    let shaped = window.text_system().shape_line(
        SharedString::from(clean.as_ref().to_owned()),
        font_size,
        &[TextRun {
            len: clean.len(),
            font: gpui_kit::font(font_family.to_string()),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let byte_idx = if closest {
        shaped.closest_index_for_x(rel_x)
    } else {
        shaped.index_for_x(rel_x).unwrap_or(clean.len())
    };
    crate::terminal_selection::byte_to_char_index(clean.as_ref(), byte_idx)
}

/// Estimate the available viewport and divide by the active font's cell size.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Viewport cells are clamped to positive grid bounds and rounded down"
)]
fn terminal_grid_for(
    window: &Window,
    side_width: Pixels,
    cell_width: Pixels,
    row_height: Pixels,
) -> (usize, usize) {
    let size = window.viewport_size();
    let (w, h) = (f32::from(size.width), f32::from(size.height));
    let cols =
        ((w - f32::from(side_width) - 46.0) / f32::from(cell_width)).clamp(20.0, 500.0) as usize;
    let rows = ((h - 190.0) / f32::from(row_height)).clamp(8.0, 200.0) as usize;
    (cols, rows)
}

#[cfg(test)]
mod session_status_tests {
    use super::{configured_terminal_font_size, parse_serial_open_baud};
    use duthub_core::model::TerminalSettings;

    #[test]
    fn terminal_font_size_uses_default_and_clamps_explicit_size() {
        let mut settings: TerminalSettings = serde_json::from_str(r"{}").unwrap();
        assert_eq!(configured_terminal_font_size(&settings), 12.5);
        settings.font_size = Some(18.);
        assert_eq!(configured_terminal_font_size(&settings), 18.);
        settings.font_size = Some(90.);
        assert_eq!(configured_terminal_font_size(&settings), 48.);
        let serialized = serde_json::to_value(TerminalSettings::default()).unwrap();
        assert!(serialized.get("fontFamily").is_none());
        assert!(serialized.get("fontSize").is_none());
    }

    #[test]
    fn parses_open_line_baud() {
        assert_eq!(
            parse_serial_open_baud("[serial] COM30 @ 115200 已打开"),
            Some(115_200)
        );
        assert_eq!(
            parse_serial_open_baud("[serial] COM30 @ 9600 已打开"),
            Some(9600)
        );
        // Failed to open / Reconnect / TX echo lines without false alarms
        assert_eq!(
            parse_serial_open_baud("[serial] 打开 COM30 @ 115200 失败: x"),
            None
        );
        assert_eq!(
            parse_serial_open_baud("[serial] 第 2 次重连 · 600ms 后重试"),
            None
        );
        assert_eq!(parse_serial_open_baud("[TX] uname -a"), None);
        assert_eq!(parse_serial_open_baud("hello"), None);
    }
}

// ---------------------------------------------------------------------------
// Serial port terminal keyboard / Rendering layers (holes in AGENTS.md #22, #46, #49, #50)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod serial_terminal_tests {
    use super::{
        LineKind, LogLine, SessionView, arrow_escape_prefix, prepare_paste_bytes,
        serial_backspace_byte, serial_enter_bytes,
    };
    use duthub_core::ipc::{ChunkStyle, ScreenRow, ScreenSnapshot};
    use duthub_core::model::{SerialBackspace, SerialEnterEnding, SerialSettings};

    /// Construct a snapshot (device session rendering source; mode bits are set to off by default).
    fn screen(rows: &[&str], application_cursor: bool, bracketed_paste: bool) -> ScreenSnapshot {
        ScreenSnapshot {
            rows: rows
                .iter()
                .map(|text| ScreenRow {
                    text: (*text).into(),
                    fg: None,
                    bg: None,
                })
                .collect(),
            cursor_row: None,
            cursor_char: None,
            application_cursor,
            bracketed_paste,
            // Mechanical default from the new snapshot contract; this helper
            // exercises the terminal text path, not garbled-byte detection.
            garbled: duthub_core::ipc::GarbledStats::default(),
        }
    }

    /// IO-3. Serial settings select CR by default, with LF and CRLF available.
    /// The result contains every byte to enqueue. Keyboard input appends nothing else.
    /// This test covers byte selection. Use a real DUT to verify delivery through the serial write path.
    #[test]
    fn enter_bytes_follow_serial_settings_with_cr_default() {
        assert_eq!(serial_enter_bytes(None), b"\r");
        assert_eq!(serial_enter_bytes(Some(SerialEnterEnding::Cr)), b"\r");
        assert_eq!(serial_enter_bytes(Some(SerialEnterEnding::Lf)), b"\n");
        assert_eq!(serial_enter_bytes(Some(SerialEnterEnding::Crlf)), b"\r\n");
        // The default is exactly one CR byte.
        assert_eq!(serial_enter_bytes(None).len(), 1);
        assert_eq!(serial_enter_bytes(None)[0], 0x0d);
    }

    /// IO-3. Serial settings select DEL by default or Ctrl-H as the alternative.
    #[test]
    fn backspace_byte_follows_serial_settings_with_del_default() {
        assert_eq!(serial_backspace_byte(None), 0x7f);
        assert_eq!(serial_backspace_byte(Some(SerialBackspace::Del)), 0x7f);
        assert_eq!(serial_backspace_byte(Some(SerialBackspace::CtrlH)), 0x08);
    }

    /// IO-3. Keyboard input uses the serial section of the settings file.
    /// Test the actual camel-case keys, since Serde ignores unknown snake-case keys
    /// and would silently substitute the defaults.
    #[test]
    fn keyboard_bytes_follow_camel_case_serial_settings() {
        let settings: SerialSettings =
            serde_json::from_str(r#"{"enterEnding": "lf", "backspace": "ctrlH"}"#)
                .expect("The serial port settings have been named in camelCase format.");
        assert_eq!(serial_enter_bytes(settings.enter_ending), b"\n");
        assert_eq!(serial_backspace_byte(settings.backspace), 0x08);
    }

    #[test]
    fn mouse_actions_default_to_menu_and_manual_copy() {
        let defaults: SerialSettings = serde_json::from_str("{}").unwrap();
        assert!(!defaults.paste_on_right_click);
        assert!(!defaults.copy_on_select);
        assert_eq!(
            serde_json::to_value(&defaults).unwrap(),
            serde_json::json!({})
        );

        let selected = serde_json::json!({"pasteOnRightClick": true, "copyOnSelect": true});
        let settings: SerialSettings = serde_json::from_value(selected.clone()).unwrap();
        assert!(settings.paste_on_right_click);
        assert!(settings.copy_on_select);
        assert_eq!(serde_json::to_value(settings).unwrap(), selected);
    }

    /// IO-4. Derive the arrow, Home, and End prefixes from the snapshot's application cursor mode.
    /// Use CSI in normal mode and SS3 in application mode. The device can switch with DECSET 1.
    /// This test covers prefix selection. Verify delivery on real hardware.
    /// Terminal emulator tests cover updates to the mode bit.
    #[test]
    fn arrow_prefix_follows_snapshot_application_cursor_bit() {
        let snapshot = screen(&["root:~# "], false, false);
        assert_eq!(arrow_escape_prefix(snapshot.application_cursor), b'[');
        // Compose the same escape, prefix, and key bytes as the keyboard handler.
        let normal_seq = [0x1b, arrow_escape_prefix(snapshot.application_cursor), b'A'];
        assert_eq!(normal_seq.as_slice(), &b"\x1b[A"[..]);

        let snapshot = screen(&["root:~# "], true, false);
        assert_eq!(arrow_escape_prefix(snapshot.application_cursor), b'O');
        let app_seq = [0x1b, arrow_escape_prefix(snapshot.application_cursor), b'A'];
        assert_eq!(app_seq.as_slice(), &b"\x1bOA"[..]);
    }

    /// IO-4. Wrap paste text only if the snapshot enables bracketed paste.
    /// With the mode off, send the text without the brackets.
    #[test]
    fn paste_wrapping_follows_snapshot_bracketed_paste_bit() {
        let off = screen(&["root:~# "], false, false);
        assert_eq!(
            prepare_paste_bytes("ls -l", off.bracketed_paste).as_deref(),
            Some(&b"ls -l"[..])
        );
        let on = screen(&["root:~# "], false, true);
        assert_eq!(
            prepare_paste_bytes("ls -l", on.bracketed_paste).as_deref(),
            Some(&b"\x1b[200~ls -l\x1b[201~"[..])
        );
    }

    /// IO-4. Normalize pasted line endings to CR, preserving tab and CR only among control characters.
    /// Drop the paste entirely if sanitization leaves nothing to send.
    #[test]
    fn paste_sanitizes_line_endings_and_control_chars() {
        // \r\n / \n Convert all lines to \r format.
        assert_eq!(
            prepare_paste_bytes("a\r\nb\nc", true).as_deref(),
            Some(&b"\x1b[200~a\rb\rc\x1b[201~"[..])
        );
        // Eliminate control characters such as BEL/FF. Keep using \t.
        assert_eq!(
            prepare_paste_bytes("a\x07b\x0c", false).as_deref(),
            Some(&b"ab"[..])
        );
        assert_eq!(
            prepare_paste_bytes("a\tb", false).as_deref(),
            Some(&b"a\tb"[..])
        );
        // After cleaning, the result is empty. There is no content to be sent.
        assert_eq!(prepare_paste_bytes("\x07\x1b", true), None);
    }

    /// IO-6. A serial screen event replaces the device session's full snapshot.
    /// The annotation list stays separate from the terminal grid.
    #[test]
    fn serial_screen_snapshot_replaces_screen_without_touching_lines() {
        let mut view = SessionView::new(
            "dut-a".into(),
            "s1".into(),
            "旧名 · COM8".into(),
            "device",
            115_200,
        );
        view.lines.push(LogLine {
            text: "[serial] COM8 @ 115200 已打开".into(),
            kind: LineKind::Dim,
        });

        view.absorb_serial_screen(screen(&["root:~# ls"], false, false).into(), 7);
        let rendered = view
            .screen
            .as_ref()
            .expect("The device session must hold a snapshot.");
        assert_eq!(rendered.rows.len(), 1);
        assert_eq!(rendered.rows[0].text, "root:~# ls");
        assert_eq!(view.last_seq, 7);
        assert_eq!(
            view.lines.len(),
            1,
            "The snapshot row does not allow entry into the annotation list."
        );

        // Second snapshot: Full replacement (rows increased to two lines), the annotation list
        // remains unchanged.
        view.absorb_serial_screen(screen(&["root:~# ls", "file-a"], false, false).into(), 8);
        assert_eq!(view.screen.as_ref().unwrap().rows.len(), 2);
        assert_eq!(view.screen.as_ref().unwrap().rows[1].text, "file-a");
        assert_eq!(view.last_seq, 8);
        assert_eq!(
            view.lines.len(),
            1,
            "Device outputs should not be repeated in the annotation list."
        );
    }

    /// IO-6: The demo session does not have a simulator. The `SerialScreen` snapshot is ignored (the
    /// screen remains unchanged, and the sequence does not progress). Full row rendering is
    /// maintained.
    #[test]
    fn demo_session_ignores_screen_snapshots_and_keeps_full_line_rendering() {
        let mut view =
            SessionView::new("dut-d".into(), "d1".into(), "演示 · COM1".into(), "demo", 0);
        view.absorb_serial_screen(screen(&["demo"], true, true).into(), 3);
        assert!(
            view.screen.is_none(),
            "The demo session does not utilize a simulated grid."
        );
        assert_eq!(
            view.last_seq, 0,
            "Ignored snapshots cannot be promoted to the next sequence number."
        );
    }

    /// IO-6: `SerialChunk` → lines routing – device session only includes annotation rows
    /// (Dim/Mark/Error) and [serial] signaling. Device output (Normal/Good/Prompt) is rendered by
    /// the emulator grid. Do not duplicate entries in the annotation list. The "[serial] … @ N
    /// opened" signaling row is also not included in the list after being consumed for link status
    /// information. Demo sessions maintain full-row rendering.
    #[test]
    fn serial_chunk_routes_annotations_only_into_device_session_lines() {
        let device = SessionView::new("dut-a".into(), "s1".into(), "t".into(), "device", 0);
        // Device outputs three types of snapshots: only grid snapshot rendering
        for style in [ChunkStyle::Normal, ChunkStyle::Good, ChunkStyle::Prompt] {
            assert!(
                !device.should_queue_chunk_line(style, false),
                "{style:?} is an output from the device; it should not be included in the annotation list."
            );
        }
        // There are three types of annotations: entries in the annotation list
        for style in [ChunkStyle::Dim, ChunkStyle::Mark, ChunkStyle::Error] {
            assert!(
                device.should_queue_chunk_line(style, false),
                "{style:?} are annotation lines"
            );
        }
        // Open confirmation signaling line (Dim): Consume the link status; do not add to the list.
        assert!(!device.should_queue_chunk_line(ChunkStyle::Dim, true));

        // Demo session: Full row rendering, with items of any style included.
        let demo = SessionView::new("dut-d".into(), "d1".into(), "t".into(), "demo", 0);
        for style in [
            ChunkStyle::Normal,
            ChunkStyle::Dim,
            ChunkStyle::Good,
            ChunkStyle::Prompt,
            ChunkStyle::Error,
            ChunkStyle::Mark,
        ] {
            assert!(demo.should_queue_chunk_line(style, false), "{style:?}");
        }
    }
}

#[derive(Clone)]
pub struct LogLine {
    pub text: SharedString,
    pub kind: LineKind,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LineKind {
    Normal,
    Dim,
    Good,
    Prompt,
    Error,
    /// Log segmentation: The terminal renders the segmentation using a specific delimiter style.
    Mark,
}

impl LogLine {
    fn color(&self, p: &'static Palette) -> gpui_kit::Rgba {
        match self.kind {
            LineKind::Normal => p.term_fg,
            LineKind::Dim => p.muted,
            LineKind::Good => p.accent,
            LineKind::Prompt | LineKind::Mark => p.prompt,
            LineKind::Error => p.red,
        }
    }
}
/// Task type: Results are written back to the corresponding interface based on the type of task
/// (Verified Issue P2-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Adb,
    Hdc,
}

/// `Result` of a completed background task: Includes task type, task ID, and the DUT that initiated
/// the task.
pub struct JobResult {
    pub job: u64,
    pub kind: JobKind,
    pub outcome: Result<Vec<AdbDevice>, String>,
}

type JobQueue = Arc<Mutex<Vec<JobResult>>>;

#[derive(Default)]
pub struct UsbDiscovery {
    job: u64,
    pub result: Option<Result<Vec<AdbDevice>, String>>,
}
type OnChangeFn =
    std::sync::Arc<dyn Fn(&mut Workbench, usize, &mut Window, &mut Context<Workbench>)>;

pub struct RelayPress {
    dut_id: String,
    pub binding: &'static str,
    channel: u8,
    release: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<Result<(), String>>>,
    _activation: gpui_kit::Subscription,
}

impl RelayPress {
    pub fn is_held(&self) -> bool {
        self.release.is_some()
    }
}

impl Drop for RelayPress {
    fn drop(&mut self) {
        self.release.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
// -+
// Workbench
// -+

/// View Mode: Home Page (Welcome Page when there is no session) / Serial Port Workstation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Home,
    Serial,
}

pub(crate) enum FlashPresetTest {
    Running,
    Finished(duthub_core::DeviceResult<duthub_core::ProcessResult>),
}

/// Keep one `InputState` draft per DUT. Retain `_subscription` so pressing Enter submits the prompt
/// until the draft is dropped.
pub(crate) struct AgentPromptEditor {
    pub(crate) state: Entity<InputState>,
    _subscription: gpui_kit::Subscription,
}

/// Agent Registry Integrated Editor: `id=None` is used for adding new entries, while `Some` is used
/// for editing existing entries.
pub(crate) struct AgentRegistryEdit {
    pub(crate) id: Option<String>,
}

/// W4c: The load that is written to disk and stored in the external path. The program has been
/// moved to the backend executor for processing. Only after the verification result (generation
/// matching) is obtained, is it written to agents.json.
#[derive(Clone)]
pub(crate) struct PendingGooseSave {
    pub path: String,
    pub generation: u64,
}

/// W4c: The payload stored in the registry that is to be written to disk. The draft has been
/// approved through the unified draft process, and all fields except `expected_sha256` have been
/// filled in (the hash is populated by the backend detection).
#[derive(Clone)]
pub(crate) struct PendingRegistrySave {
    pub definition: duthub_core::agent_config::AgentDefinition,
    /// Whether this entry is the default "goose". The detection results should be synchronized to
    /// `goose_probe`.
    pub goose: bool,
    pub generation: u64,
}

/// Agent host worker thread → GUI event output (F9).
///
/// `HostEvents` is called by worker threads, and cannot directly affect the UI interface. In case
/// of failures, notifications are sent to `AlertCenter` (the rendering layer). Generally, when
/// exiting the system, notifications are sent to the toast queue, and then processed by
/// `agent_host_tick` into `pending_toasts`.
struct AgentHostEvents {
    alerts: Arc<Mutex<AlertCenter>>,
    toasts: Arc<Mutex<Vec<String>>>,
    /// Track devices whose failures have already been reported so `on_exited` does not show a
    /// duplicate toast.
    failed: Mutex<std::collections::HashSet<String>>,
}

impl agent_host::HostEvents for AgentHostEvents {
    fn on_started(&self, dut: &str, _session: &str) {
        self.failed.lock().unwrap().remove(dut);
    }

    fn on_exited(&self, dut: &str, code: Option<i32>) {
        if self.failed.lock().unwrap().remove(dut) {
            // Failed events are reported as separate alarms, without duplicate notifications.
            return;
        }
        let message = match code {
            Some(code) => i18n::message("agentExitedCode", &[("code", &code.to_string())]),
            None => i18n::label("agentExitedSignal").to_owned(),
        };
        self.toasts.lock().unwrap().push(message);
    }

    fn on_failed(&self, dut: &str, message: &str) {
        self.failed.lock().unwrap().insert(dut.to_owned());
        self.alerts.lock().unwrap().report_for(
            dut,
            AlertSource::Agent,
            i18n::label("agentFailedTitle"),
            message,
        );
        self.toasts
            .lock()
            .unwrap()
            .push(i18n::message("agentFailedToast", &[("message", message)]));
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "top-level GPUI Workbench state holds independent UI flags"
)]
pub struct Workbench {
    pub(crate) agent_credential_listener: Option<String>,
    pub(crate) agent_token_input: Option<Entity<InputState>>,
    /// Embedded goose ACP host (W3): `--self-test` option results in `None` (no construction or
    /// startup of the goose process).
    pub(crate) agent_host: Option<agent_host::HostManager>,
    /// The `toast` function for the agent's host working thread is executed here; the rendering
    /// task is moved to the `pending_toasts` state.
    agent_host_toasts: Arc<Mutex<Vec<String>>>,
    /// MCP HTTP listener handle (F1/T16): Main endpoint + additional endpoints per DUT; Disables
    /// when dropped.
    pub(crate) mcp_runtime: crate::mcp_runtime::McpRuntimeHandles,
    /// States shared with the MCP runtime (constructed from `HostManager`; T16 is used to prevent
    /// only online thread closures).
    pub(crate) mcp_state: Arc<dyn duthub_mcp::state::McpState>,
    /// Live flash tool presets & ADB path mirrored into `RuntimeMcpState`.
    mcp_tools_config: Arc<Mutex<crate::mcp_runtime::RuntimeToolsConfig>>,
    /// Agent access context (authentication/lease/audit): The source of each DUT Agent session
    /// during runtime.
    pub(crate) agent_access: Option<Arc<duthub_mcp::auth_http::AccessContext>>,
    /// For each DUT, the agent inputs the draft independently (F13: Switch DUT to not concatenate
    /// text).
    pub(crate) agent_prompts: HashMap<String, AgentPromptEditor>,
    /// Virtual list scrolling handle (for use with `DUT; uniform_list`).
    pub(crate) agent_transcript_scroll: HashMap<String, gpui_kit::UniformListScrollHandle>,
    /// The transcript shows a monotonous sequence of numbers (`SessionView::total_entries()`);
    /// after reaching a maximum of 2000 entries in memory, the value of `transcript.len()` ceases
    /// to increase. Only after reaching 2000 entries does it continue to decrease further.
    pub(crate) agent_transcript_seen: HashMap<String, u64>,
    /// Track the sent text and token by DUT until the next render frame can call `set_value`. Clear
    /// the field only if its contents still match the sent text, preserving new input and drafts
    /// for other devices.
    agent_prompt_clear: HashMap<String, (String, u64)>,
    /// The monotonous token for the "send and then clear" request (using the latest semantic
    /// definition; this is used to test the matching rules).
    agent_prompt_token: u64,
    /// `agents.json` registry (ACP agents that can be selected for each DUT; goose is the default
    /// embedded entry).
    pub(crate) agents: duthub_core::agent_config::AgentsFile,
    /// Registry loading error (Failure to close the application due to corruption or
    /// incompatibility issues; The reason is displayed in the UI, no speculation is required.)
    pub(crate) agents_error: Option<String>,
    /// Agent selection for each DUT (sidecar, `<work_dir>/agent-bindings.json`).
    pub(crate) agent_bindings: duthub_core::agent_config::AgentBindingsFile,
    /// Registry Integrated Editor (name/program/args/envKeys).
    pub(crate) agent_registry_edit: Option<AgentRegistryEdit>,
    /// W4b Env Value Editor: The key being selected currently serves as the target for value
    /// writing/deletion purposes. This feature is available only when editing existing entries.
    pub(crate) agent_env_selected: Option<String>,
    /// W4b env value editor: Selects the state of the keyring's read function (`None` indicates no
    /// selection).
    pub(crate) agent_env_value_state: Option<duthub_desktop::agent_ui::EnvValueState>,
    /// The external Goose path was last detected (version/hash/unsigned signature). The detection
    /// is performed by the backend executor (W4c). The UI provides read-only results, with all
    /// results remaining in a pending state.
    pub(crate) goose_probe: Option<agent_host::registry::ProgramProbe>,
    /// The external goose detection process is still running in the background. The UI indicates
    /// that the process is ongoing, and the startup does not become blocked.
    pub(crate) goose_probe_pending: bool,
    /// Generation of backend detection/saving: Expired results are discarded based on generation,
    /// and the new state is not overwritten.
    goose_probe_generation: u64,
    /// Wait for the background detection to complete, then write the results to disk via the
    /// external goose path (W4c).
    pub(crate) pending_goose_save: Option<PendingGooseSave>,
    /// Wait for the background program to complete detection, and then write the registry
    /// information to disk (W4c).
    pub(crate) pending_registry_save: Option<PendingRegistrySave>,
    /// The generation of the env value keyring: increments when cutting keys/rereading, and old
    /// read results are discarded directly.
    agent_env_read_generation: u64,
    /// `--self-test`: The entire agent host is disabled. This mode involves a self-check only,
    /// without any unpacking or detection of external executables.
    agent_self_test: bool,
    /// Frame A2A Token cache (reads from the keyring once when a DUT is selected, to avoid
    /// accessing the key library every frame).
    a2a_tokens: HashMap<String, Result<Option<String>, String>>,
    pub(crate) provider_models: crate::provider_models::Catalog,
    pub(crate) flash_preset_tests: HashMap<String, FlashPresetTest>,
    pub relay_press: Option<RelayPress>,
    pub relay_board_selected: usize,
    pub relay_board_name: Option<(
        String,
        gpui_kit::Entity<gpui_kit::component::input::InputState>,
    )>,
    /// Control the device (relay board) to edit the draft. The semantics of this field are the same
    /// as those in the "Connection Management" form: only edits that are committed to disk.
    /// The draft (`Some` represents "unsaved modifications"). Only when the "Saved Devices" are
    /// written back to the board card list and stored to disk, will the draft be discarded when the
    /// "Cancel Modification" is performed, or when the board card is switched or the page is left.
    /// The ID must match the currently selected saved board card; otherwise, it will be considered
    /// an expired draft.
    pub(crate) relay_board_draft: Option<duthub_core::model::RelayBoard>,
    /// View Mode: Home / Serial Port Workbench (Automatically return to the home page after closing
    /// all sessions)
    pub view: ViewMode,
    pub lang: Lang,
    pub theme_mode: theme::ThemeMode,
    pub p: &'static Palette,

    pub device: Arc<DeviceService>,
    pub(crate) media: crate::media_page::MediaView,
    /// HDMI monitor frame pump (§6.3): the only `InlinePreview::poll` consumer,
    /// driven by its own 100ms timer so a minimized main window keeps frames.
    pub(crate) frame_pump: crate::monitor_window::FramePump,
    /// The at-most-one HDMI float window (§6.2 invariant/§6.4).
    pub(crate) float_window: Option<crate::monitor_window::FloatWindow>,
    /// Main window handle (observed at first render; `FocusMainWindow` target).
    pub(crate) main_window: Option<gpui_kit::AnyWindowHandle>,
    /// `on_window_should_close` is installed once, on the first render.
    main_close_hook_installed: bool,
    /// Tray icon host. Installed on the first render (the only place with a
    /// `&mut Window`); `None` before install, after a failed install, and
    /// after `begin_quit` removed it.
    tray: Option<duthub_desktop::tray::TrayHandle>,
    /// Tray installation failed: a close must quit for real instead of
    /// pretending to hide with no way back.
    tray_failed: bool,
    /// Tooltip text currently applied to the tray icon. The tray menu labels
    /// are frozen in `TrayOptions`, but the tooltip can follow a language
    /// switch without re-installing the icon.
    tray_tooltip: String,
    /// Main window is hidden to the notification area (tick keeps running).
    hidden: bool,
    /// Latched by the tray "Quit" item: `close_disposition` always returns
    /// `Quit` from then on.
    quitting: bool,
    /// `render_root` writes `window.has_active_dialog(cx)` every frame; the
    /// 50 ms tick reads it (one frame late by design — the tick never nests a
    /// window update).
    dialog_open: bool,
    /// Hidden-state alert router (pure state machine). Its only owner is the
    /// 50 ms tick's `alert_present_tick`.
    alert_router: HiddenAlertRouter,
    /// Last `(generation, hidden, pending)` evaluated by the alert router.
    /// System notification state is only queried when this changes.
    alert_eval_key: Option<(u64, bool, u64)>,
    /// Relay board offline monitor: the probe thread owns the blocking I/O,
    /// the tick only aligns the watch set and drains events.
    relay_watch: Arc<duthub_device::relay_watch::RelayWatchService>,
    relay_probe: Arc<duthub_device::control::ControlRelayProbe>,
    relay_events: std::sync::mpsc::Receiver<duthub_device::relay_watch::RelayBoardEvent>,
    relay_watch_quit: Arc<std::sync::atomic::AtomicBool>,
    /// Change key of the watched board set (`id`, `node`, `channel`, `driver`).
    relay_watch_signature: Vec<(String, String, u8, String)>,
    /// HDMI / capture and USB debug (ADB / HDC) disconnect monitor.
    device_watch: Arc<crate::device_watch::DeviceWatchService>,
    device_watch_events: std::sync::mpsc::Receiver<crate::device_watch::DeviceWatchEvent>,
    /// Next allowed MCP snapshot refresh (250 ms throttle while hidden).
    snapshot_due_ms: u64,
    /// Toasts dropped because `pending_toasts` hit `MAX_TOASTS`; reported once
    /// when rendering resumes.
    toasts_omitted: usize,
    /// Keeps the App-level float `on_window_closed` observer alive.
    _float_closed_subscription: Option<gpui_kit::Subscription>,
    adb_client: Arc<AdbClient>,
    /// The actual external address of the master MCP listener (in LAN mode, it refers to the
    /// detected LAN IP address).
    mcp_bound_endpoint: Arc<OnceLock<SocketAddr>>,
    /// Backend task result queue (contains task type, task ID, DUT ID; updated based on the request
    /// origin)
    job_queue: JobQueue,
    next_job: u64,
    /// Tool download center (settings → Tools download): one background
    /// install at a time; progress/result slots are drained by the 50ms tick
    /// (same shape as the model-probe slot).
    pub(crate) tool_installs: duthub_desktop::tool_ui::ToolInstallCenter,
    /// Tool row snapshot cache (on-disk facts: manifest / effective path;
    /// refreshed on page open and when an install finishes).
    pub(crate) tool_rows: Vec<duthub_desktop::tool_ui::ToolRowSnapshot>,
    /// Latest install result per tool (row-level "failure / cancelled"
    /// display until the next action).
    pub(crate) tool_last_results: HashMap<&'static str, duthub_desktop::tool_ui::InstallFinished>,
    /// Progress snapshot synced by the tick, per tool (redraw only on change,
    /// so running downloads do not repaint the whole frame per chunk).
    pub(crate) tool_progress_view: HashMap<&'static str, duthub_desktop::tool_ui::InstallProgress>,
    /// Draft for the download proxy mode: the Select stages here and the save
    /// button commits it, so mode and URL share one submit semantic — a
    /// download never runs against a half-applied policy (custom mode without
    /// its saved URL would fail closed on every run).
    pub(crate) tool_proxy_mode_draft: Option<duthub_core::model::DownloadProxyMode>,
    /// Burn the test results based on DUT `ownership: dut_id` → `Result` View
    usb_discovery: [UsbDiscovery; 2],
    usb_debug_view: Option<bool>,

    pub settings: duthub_core::SettingsFile,
    /// Share the Agent authentication runtime across MCP HTTP middleware, lease management, and
    /// settings. `update()` replaces the active configuration after settings are saved.
    pub access_runtime: Arc<duthub_core::AgentAccessRuntime>,
    pub duts: DutsState,
    /// Configuration directories: bootstrap. The settings.json and duts.json files are located in
    /// the same directory, at the path .duthub/.
    data_dir: PathBuf,
    /// Working directory (valid values for resolution): logs / captures / exports / runtime output
    /// root.
    /// For detailed resolution rules, refer to `duthub_core::store::resolve_work_dir_in` (settings
    /// > --workdir > default setting).
    pub work_dir: PathBuf,
    pub artifact_root: PathBuf,
    pub(crate) artifact_quota: Arc<duthub_core::dut_quota::DutQuota>,
    pub(crate) boot_learning_dut: Option<String>,
    boot_reference_edits: HashSet<String>,
    boot_similarity_drafts: HashMap<String, String>,

    pub sessions: Vec<SessionView>,
    pub active_session: usize,
    pub serial_ports: Vec<SerialPortInfo>,

    /// MCP `Snapshot (frame-by-frame refresh, mcp_runtime) Write to disk`
    snapshot_ref: Arc<Mutex<duthub_mcp::snapshot::StateSnapshot>>,
    /// Heartbeat Engine (Requirement ⑦): Desktop timer driver; Agent modified via heartbeat.set
    /// command.
    pub heartbeat: Arc<duthub_device::heartbeat::HeartbeatService>,
    heartbeat_drafts: HashMap<String, duthub_core::heartbeat::HeartbeatConfig>,
    /// Shared Device Operation Interface (Control Action Authorization/Binding/Capability
    /// Verification Interface; DUT Configures Registration per Frame)
    pub control: Arc<duthub_device::control::ControlService>,
    /// Approval Token Manager (shared with the MCP runtime; GUI operations generate tokens through
    /// this interface)
    /// MCP Control Plane: Triggers actions via Power/Reset/PowerKey commands; details regarding
    /// backend connections can be found in section M3.3.
    pub overlay: Option<ConfigPage>,
    connection_open: bool,
    connection_dut_scope: Option<String>,
    pub(crate) expanded_mcp_config: Option<String>,
    connection_pane_width: Option<Pixels>,
    tools_pane_width: Option<Pixels>,
    pub menu_open: bool,
    pub side_panel: crate::side_panel::SidePanel,
    pub agent: AgentState,
    agent_model_provider: String,
    agent_model_name: String,
    pub find_open: bool,
    pub find_query: String,
    pub rec_on: bool,
    pub rec_buffer: Vec<String>,
    /// System notifications to be displayed (Kit notifications, displayed through the Root layer
    /// during rendering)
    pub pending_toasts: Vec<String>,
    /// Exception Popup Notification Center (Requirement ⑤): The global suppression list is
    /// retrieved from settings.alerts. The switch for each DUT is determined by duts.json
    /// alertsByDut (default setting is enabled). This instance is shared with the MCP HTTP thread
    /// (GUI popups are displayed when control actions on the MCP side fail).
    pub alerts: Arc<Mutex<AlertCenter>>,
    /// "Requires manual intervention" - Is the Dialog being opened? (Self-healing: Resets to
    /// default state in the next frame after it is closed externally.)
    alert_dialog_open: bool,
    /// Identity `(dut_id, source)` of the currently displayed alert dialog, used
    /// to auto-dismiss disconnect dialogs when the device reconnects.
    active_alert_dialog: Option<(String, crate::alerts::AlertSource)>,
    /// Pending request to close the currently open alert dialog on the next render frame.
    dismiss_active_alert_dialog: bool,
    pub modal: Option<ModalState>,
    /// Terminal Right-click Menu: `Some`((x, y)) = Menu Display Position (Window Coordinates, px)
    pub terminal_ctx: Option<(f32, f32)>,
    pub open_dd: Option<String>,
    /// During each DUT A2A operation (W4a): status checks, token hot rotation, disabling/deletion
    /// coordination. All operations related to exit and shutdown are handled here. When set to
    /// `None`, it indicates a pure self-test mode; no listeners are activated. In this mode, status
    /// checks are simply categorized as "Disabled".
    pub a2a: Option<crate::a2a_runtime::A2aRuntime>,
    pub cfg_search: String,
    // Draft of digital fields (can be cleared and re-entered; synchronized with the model when
    // valid)
    drafts: std::collections::HashMap<In, String>,

    input: String,
    input_focus: FocusHandle,
    terminal_focus_pending: bool,
    text_editors: HashMap<In, BoundEditor<InputState>>,
    textareas: HashMap<In, BoundEditor<TextareaState>>,
    /// Terminal viewport scroll handle: Automatically moves to the bottom when new output arrives
    /// (prototype behavior: nearBottom<65 semantics).
    /// This handle is specific for `uniform_list` operations; it can only be used with row-based
    /// construction elements.
    terminal_scroll: gpui_kit::UniformListScrollHandle,
    /// Serial Port Settings Page: The highlighted rule number currently being edited (selected by
    /// clicking on the rule list row)
    pub(crate) serial_rule_selected: usize,
    /// Cached compiled prompt highlight rules (`Arc` avoids cloning Regex + Strings every frame).
    highlight_cache: std::cell::RefCell<HashMap<String, Option<regex::Regex>>>,
    highlight_rules_cache: std::cell::RefCell<HighlightRulesCache>,
    /// Cached host MXC OS sandbox status (10s TTL) so render frames never run OS probes at 60 FPS.
    sandbox_status_cache:
        std::cell::RefCell<Option<(std::time::Instant, duthub_sandbox::SandboxStatus)>>,
    /// Shared Tokio runtime owned for the lifetime of the desktop process.
    _shared_runtime: Arc<tokio::runtime::Runtime>,
    /// Last grid size sent to the real session (`None` indicates no transmission; sent again after
    /// the session is created).
    term_grid: Option<(usize, usize)>,
    terminal_zoom_size: Option<f32>,
    terminal_font_picker: Option<FontPicker>,

    pub frames: Arc<AtomicUsize>,
    pub core_lines: Arc<AtomicUsize>,
}

/// Startup context ( injected once at main.rs → Workbench).
///
/// The settings.json must be loaded only once (`load_settings` has the side effect of renaming
/// corrupted files and creating backups; reading it twice will trigger a corruption alert). The
/// working directory is shared with the serial port during runtime after being unified in the main
/// process, ensuring that logs are within the same root as captures/exports/runtime.
pub struct StartupContext {
    /// Configuration directory (bootstrap): settings.json / duts.json. The executable files are
    /// stored in the .duthub/ directory.
    pub data_dir: PathBuf,
    /// Working directory (effective values for resolution): logs / captures / exports / runtime
    /// output root.
    pub work_dir: PathBuf,
    pub artifact_root: PathBuf,
    pub artifact_quota: Arc<duthub_core::dut_quota::DutQuota>,
    /// Loaded settings (results of main.rs parsing, including cases where startup fails and default
    /// values are used as a fallback option).
    pub settings: duthub_core::SettingsFile,
    /// Launch prompt (settings corrupted / working directory rollback). Appears alongside the first
    /// toast message.
    pub notice: Option<String>,
    /// `--self-test`: Does not create an Agent host (no installation/scan of goose). All other UI
    /// components continue to be rendered as usual.
    pub self_test: bool,
    /// Shared Tokio runtime from `main.rs` (used by `DeviceService`, `ControlService`, `A2aRuntime`).
    pub runtime: Arc<tokio::runtime::Runtime>,
}

impl Drop for Workbench {
    fn drop(&mut self) {
        // Quit paths that bypass `begin_quit` (WM_ENDSESSION, unwinding, a
        // panic in a tick) must not leave a relay channel closed: drop the
        // release sender and join the hold-test worker, whose contract is to
        // always attempt OFF after ON.
        if let Some(mut press) = self.relay_press.take() {
            press.release.take();
            if let Some(worker) = press.worker.take() {
                let _ = worker.join();
            }
        }
        // Stop the relay probe thread at its next wakeup.
        self.relay_watch_quit.store(true, Ordering::Relaxed);
        // W4a Fixed Shutdown Order: Stop Agent Sessions → Stop A2A Listeners (Release Ports) → Turn
        // Off MCP HTTP Listeners and Snapshot Threads (F1/T16, F12).
        if let Some(mut host) = self.agent_host.take() {
            host.stop_all();
        }
        if let Some(a2a) = self.a2a.as_mut() {
            a2a.shutdown();
        }
        self.mcp_runtime.shutdown();
    }
}

/// Runs the ordered close plan from a deferred callback, i.e. with no
/// Workbench lease active (design §2.1). Only this function destroys windows;
/// `prepare_main_window_close` merely schedules it.
///
/// `plan` is the frozen disposition contract **and** the dispatch source;
/// `transition` is the coordinator's executable form of the same decision and
/// is only cross-checked in debug builds. If the two ever drift, a release
/// build still follows the plan.
fn run_close_plan(
    weak: &gpui_kit::WeakEntity<Workbench>,
    plan: &'static [CloseStep],
    transition: &LifecycleTransition,
    cx: &mut gpui_kit::App,
) {
    debug_assert_eq!(
        transition.actions,
        duthub_desktop::tray_lifecycle::actions_for_close_plan(plan)
    );
    // Dispatch from the frozen plan, not from the coordinator's independent
    // copy: a future step that is added to the disposition but not to the
    // coordinator must change behaviour in release builds too, not only trip a
    // debug assertion.
    let actions = duthub_desktop::tray_lifecycle::actions_for_close_plan(plan);
    for action in actions {
        run_lifecycle_action(weak, action, cx);
    }
}

/// Executes one lifecycle action from an app-level callback.
///
/// Window destruction and relay release run outside a Workbench lease on
/// purpose: the float close observer fires from the window trail and must not
/// re-enter a held lease.
fn run_lifecycle_action(
    weak: &gpui_kit::WeakEntity<Workbench>,
    action: LifecycleAction,
    cx: &mut gpui_kit::App,
) {
    match action {
        LifecycleAction::ReleaseRelay => {
            let _ = weak.update(cx, Workbench::release_relay_for_close);
        }
        LifecycleAction::CloseFloat => {
            // Read the handle under a short lease, then remove the window
            // outside it.
            let float = weak
                .read_with(cx, |this, _| {
                    this.float_window.as_ref().map(|float| float.handle)
                })
                .ok()
                .flatten();
            if let Some(handle) = float {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
            // The programmatic close clears `float_window`, so the deferred
            // window-closed observer will not run its Floating→Docked
            // transition. Do it now or the following StopPreview is a no-op
            // (`release_monitor` refuses while the pump still says Floating).
            let _ = weak.update(cx, |this, _| this.forget_float_window());
        }
        LifecycleAction::StopPreview => {
            let _ = weak.update(cx, |this, cx| this.release_monitor(None, cx));
        }
        LifecycleAction::HideToTray => {
            let _ = weak.update(cx, Workbench::finish_hide_to_tray);
        }
        LifecycleAction::DestroyMain => {
            let main = weak
                .read_with(cx, |this, _| this.main_window)
                .ok()
                .flatten();
            if let Some(handle) = main {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
        }
        LifecycleAction::ShowMain => {
            let _ = weak.update(cx, Workbench::show_from_tray);
        }
        LifecycleAction::RecoverFromIconLoss => {
            let _ = weak.update(cx, Workbench::recover_from_icon_loss);
        }
        LifecycleAction::PresentAlert { presentation, .. } => {
            let _ = weak.update(cx, |this, _| {
                this.present_hidden_alert(presentation);
            });
        }
        LifecycleAction::StartFlash => {
            let _ = weak.update(cx, |this, _| {
                this.start_alert_flash(crate::alerts::now_ms());
            });
        }
        LifecycleAction::StopFlash => {
            let _ = weak.update(cx, |this, _| {
                this.stop_alert_flash();
            });
        }
        // Quit-path actions are executed by `begin_quit`, which owns the
        // bounded relay join and the shutdown order. A deferred close plan
        // never contains them: `close_disposition` returns `Quit` only when
        // the tray is unavailable, and that plan destroys the window instead.
        LifecycleAction::ReleaseRelayAndJoin
        | LifecycleAction::ShutdownRuntime
        | LifecycleAction::RemoveTray
        | LifecycleAction::QuitProcess => {
            debug_assert!(
                false,
                "quit action reached the deferred close executor: {action:?}"
            );
        }
    }
}

/// W4a: The DUT is responsible for deleting the Workbench implementation that implements the
/// five-step cleanup process.
/// The sequence of operations is as follows: strip → persist → `stop_and_join` → delete token →
/// delete dir.
/// This sequence is verified using mock operations, as defined in the
/// `apps/desktop/tests/agent_ui_logic.rs` file.
impl duthub_desktop::agent_ui::DutCleanupOps for Workbench {
    fn strip_mcp(&mut self, dut_id: &str) -> bool {
        // No dedicated endpoint for the DUT has been configured = no need to perform strip
        // operation (success); if the strip operation fails (including synchronization settings),
        // write to disk will fail, resulting in an error return of false. This causes the entire
        // deletion process to be aborted.
        if self
            .settings
            .agent_access
            .listener_for_dut(dut_id)
            .is_none()
        {
            return true;
        }
        self.strip_dut_mcp(dut_id)
    }

    fn persist_removal(&mut self, dut_id: &str) -> Result<(), String> {
        // First, write to disk and delete the data, then proceed with the operation (AGENTS #37):
        // This prevents the target record from being removed during a disk failure, avoiding the
        // need to restart the DUT in case of a failure.
        self.heartbeat.commit_configs(&HashMap::new(), |_| {
            let Some(mut saved) =
                store::load_duts(&self.data_dir).map_err(|error| error.to_string())?
            else {
                return Ok(());
            };
            if crate::data::remove_connection(&mut saved, dut_id) {
                store::save_duts_atomic(&self.data_dir, &saved)
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })?;
        // Every DUT Agent is selected to be deleted along with the DUT. By default, no redundant
        // entries are created for "goose". If a failure occurs, only an alarm will be generated,
        // and the process will not be interrupted (old bindings will automatically be resolved
        // during the next save operation).
        let mut bindings = self.agent_bindings.clone();
        if bindings.remove_dut(dut_id) {
            match duthub_core::agent_config::save_agent_bindings_atomic(&self.work_dir, &bindings) {
                Ok(()) => self.agent_bindings = bindings,
                Err(error) => {
                    self.notify_toast(i18n::message(
                        "dutDeletedAgentBindingFailed",
                        &[("error", &error.to_string())],
                    ));
                }
            }
        }
        Ok(())
    }

    fn stop_and_join(&mut self, dut_id: &str) {
        // Agent session: Non-blocking termination (token revocation/stopping stdin, execution
        // immediately, waiting on the host thread) – Same mechanism as "stopping" a tab/session
        // panel.
        self.stop_agent_session(dut_id);
        // The A2A listener will be stopped immediately, and the port will be released.
        if let Some(a2a) = self.a2a.as_ref() {
            a2a.stop_dut(dut_id);
        }
        // Serial port session cascading shutdown (the demo session is also registered in
        // DeviceService); repeated shutdown.
        // Returned "NotFound" was ignored.
        let _ = self.device.close_session(dut_id);
        self.heartbeat.unregister(dut_id);
    }

    fn delete_a2a_token(&mut self, dut_id: &str) -> Result<(), String> {
        duthub_core::delete_a2a_token(dut_id).map_err(|error| error.to_string())
    }

    fn delete_agent_dir(&mut self, dut_id: &str) -> Result<(), String> {
        if let Some(host) = self.agent_host.as_mut()
            && host.stop_and_join(dut_id, std::time::Duration::ZERO)
                == duthub_desktop::agent_host::StopJoin::TimedOut
        {
            return Err(i18n::label("agentStillExitingPending").into());
        }
        duthub_desktop::agent_ui::delete_agent_dir_idempotent(&self.work_dir, dut_id)
    }

    fn record_pending_cleanup(
        &mut self,
        dut_id: &str,
        step: duthub_desktop::agent_ui::CleanupStep,
        reason: &str,
    ) {
        if let Err(error) =
            duthub_desktop::agent_ui::append_pending_cleanup(&self.work_dir, dut_id, step, reason)
        {
            self.notify_toast(i18n::message(
                "recordPendingCleanupFailed",
                &[("error", &error.clone())],
            ));
        }
        // Synchronized alert windows (shared by AlertCenter and MCP threads): Unfinished tasks must
        // be visible.
        let (title, detail) = crate::alerts::control_action_alert_texts(
            dut_id,
            i18n::label("pendingCleanupAfterDeleteTitle"),
            reason,
        );
        let _ = self.alerts.lock().map(|mut center| {
            center.report_for(dut_id, AlertSource::Agent, title, detail);
        });
    }
}

impl Workbench {
    fn dut_id_for_port(duts: &DutsState, port: &str) -> String {
        if let Some(dut) = duts.file.duts.iter().find(|dut| {
            dut.serial.port == port || (cfg!(windows) && dut.serial.port.eq_ignore_ascii_case(port))
        }) {
            return dut.id.clone();
        }
        let slug: String = port
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let base = format!("dut-{slug}");
        let mut id = base.clone();
        let mut suffix = 2;
        while duts.file.duts.iter().any(|dut| dut.id == id) {
            id = format!("{base}-{suffix}");
            suffix += 1;
        }
        id
    }

    pub fn new(
        cx: &mut Context<Self>,
        device: Arc<DeviceService>,
        frames_in: Arc<AtomicUsize>,
        core_lines_in: Arc<AtomicUsize>,
        auto_open: Option<&str>,
        auto_baud: u32,
        startup: StartupContext,
    ) -> Self {
        let StartupContext {
            data_dir,
            work_dir,
            artifact_root,
            artifact_quota,
            settings,
            notice: startup_notice,
            self_test,
            runtime: shared_runtime,
        } = startup;
        let (mut duts, duts_err) = DutsState::load(&data_dir);
        // Agent Registry: The default goose is not generated when it is missing; it fails to close
        // when damaged (backups are retained) and an error message is displayed. The reason is that
        // each DUT chooses to store a sidecar, without modifying the existing schema in duts.json.
        // W4c `--self-test`: Only holds default values in memory; it never reads/writes agents.json
        // or agent-bindings sidecars (load_agents is written to disk as a default registry when it
        // is missing).
        let (agents, mut agents_error) = if self_test {
            (duthub_core::agent_config::default_agents(), None)
        } else {
            match duthub_core::agent_config::load_agents(&work_dir) {
                Ok(agents) => (agents, None),
                Err(error) => (
                    duthub_core::agent_config::default_agents(),
                    Some(error.to_string()),
                ),
            }
        };
        let agent_bindings = if self_test {
            duthub_core::agent_config::AgentBindingsFile::default()
        } else {
            match duthub_core::agent_config::load_agent_bindings(&work_dir) {
                Ok(bindings) => bindings,
                Err(error) => {
                    agents_error = Some(match agents_error {
                        Some(existing) => format!("{existing}；{error}"),
                        None => error.to_string(),
                    });
                    duthub_core::agent_config::AgentBindingsFile::default()
                }
            }
        };
        // W4c: External goose detection no longer causes blocking during startup (files are hashed,
        // along with the `--version` parameter, and these processes run entirely in the
        // background). After construction is complete, the process is initiated by spawn.
        // Self-testing is not performed during this process.
        let goose_probe = None;
        let mut toast = startup_notice.or(duts_err);
        if let Some(error) = &agents_error {
            toast = Some(match toast {
                Some(existing) => format!("{existing}；{error}"),
                None => error.clone(),
            });
        }
        let lang = Lang::from_setting(&settings.appearance.language);
        lang.activate();
        let theme_mode = theme::ThemeMode::from_setting(&settings.appearance.theme);
        theme::apply_brand(cx, theme_mode);
        let adb_client = Arc::new(AdbClient::real());
        let mut bus_rx = device.subscribe();
        let serial_ports = device.list_ports();

        // Start without a session so users can select a DUT from the new-session action.
        // An explicit --open-port opens a real serial session at startup.
        let mut sessions: Vec<SessionView> = Vec::new();
        if let Some(port) = auto_open {
            let dut_id = Self::dut_id_for_port(&duts, port);
            let matched = duts.file.duts.iter().position(|dut| dut.id == dut_id);
            let title = matched.map_or_else(
                || port.to_owned(),
                |index| {
                    duts.selected = index;
                    format!("{} · {port}", duts.file.duts[index].name)
                },
            );
            let name = matched.map_or_else(
                || archive_safe_name(port, &dut_id),
                |index| archive_safe_name(&duts.file.duts[index].name, &dut_id),
            );
            let reference = matched
                .and_then(|index| boot_reference_path(&artifact_root, &duts.file.duts[index]));
            match device.open_named_session_with_log_options(
                &dut_id,
                &name,
                port,
                auto_baud,
                duthub_device::SessionKind::Real,
                duthub_device::SerialLogOptions {
                    split: matched.is_some_and(|index| duts.file.duts[index].serial.split_logs),
                    reference: reference.as_deref(),
                },
            ) {
                Ok(session_id) => {
                    sessions.push(SessionView::new(
                        dut_id, session_id, title, "device", auto_baud,
                    ));
                }
                Err(err) => {
                    toast = Some(i18n::message(
                        "openPortFailed",
                        &[("port", port), ("error", &err.to_string())],
                    ));
                }
            }
        }
        let core_lines = core_lines_in;

        let agent_model_provider = settings
            .agent
            .active_provider_id
            .as_deref()
            .and_then(|id| settings.agent.providers.iter().find(|p| p.id == id))
            .map_or_default(|p| provider_setting(p.protocol).to_owned());
        let agent_model_name = settings.agent.active_model_id.clone().unwrap_or_default();
        let wb_work_dir = work_dir.clone();
        let frames = frames_in;
        let agent = AgentState::new();
        let snapshot_ref = Arc::new(std::sync::Mutex::new(
            duthub_mcp::snapshot::StateSnapshot::default(),
        ));
        let initial_view = if sessions.is_empty() {
            ViewMode::Home
        } else {
            ViewMode::Serial
        };
        let mut media = crate::media_page::MediaView::new(artifact_root.clone());
        // Resolve ffmpeg before the settings reach `MediaView` (explicit path >
        // downloaded install > PATH). A resolution miss keeps the configured
        // value for the existing `executable()` check to report (§3.6/§3.7).
        media.set_capture_settings(&duthub_desktop::capture_backend::resolved_capture_settings(
            &settings.tools.capture,
        ));
        // Share the app-wide artifact quota with the Agent-op bridge (§3.7):
        // screenshot/recordStart must use the same accounting as manual actions.
        media.set_artifact_quota(artifact_quota.clone());
        // Shared Device Operations Interface: The Approval Token Manager and the MCP share the same
        // instance during operation.
        // Authorization/binding/capability verification for control actions (Power/Reset/PowerKey)
        // is handled through ControlService.
        // The CH340 driver is implemented directly in the real hardware; injection is performed in
        // a selective manner (recommended approach: implement it in a way that avoids unnecessary
        // complexity).
        // In the demo mode, this parameter is set to None. If this value is set, it will result in
        // an error indicating that the method is not supported, and no tokens will be consumed.
        let approval = Arc::new(duthub_core::ApprovalManager::new());
        #[cfg(feature = "real-hardware")]
        let control_backend: Option<Arc<dyn duthub_core::ControlBackend>> =
            Some(Arc::new(duthub_control::Ch340RelayDriver::new()));
        #[cfg(not(feature = "real-hardware"))]
        let control_backend: Option<Arc<dyn duthub_core::ControlBackend>> = None;
        // The Agent access context shares its lease manager with `DeviceService` so MCP and the
        // serial writer use the same authorization state.
        let access_runtime =
            duthub_core::AgentAccessRuntime::from_settings(settings.agent_access.clone());
        let access_ctx = match (device.lease_manager(), device.audit_sink()) {
            (Some(lease), Some(audit)) => {
                lease.set_auth_enabled(access_runtime.enabled());
                lease.set_agent_access(access_runtime.clone());
                lease.set_ttl(Duration::from_secs(u64::from(
                    access_runtime.lease_ttl_secs().max(1),
                )));
                Some(Arc::new(duthub_mcp::AccessContext::new(
                    access_runtime.clone(),
                    lease.clone(),
                    audit.clone(),
                )))
            }
            _ => None,
        };
        let control = {
            let mut svc = duthub_device::control::ControlService::with_runtime(
                control_backend,
                approval.clone(),
                shared_runtime.handle().clone(),
            );
            if let (Some(lease), Some(audit)) = (device.lease_manager(), device.audit_sink()) {
                svc = svc
                    .with_lease_authority(lease.clone())
                    .with_audit_sink(audit.clone());
            }
            Arc::new(svc)
        };
        // Cut the segment before turning on/off. The action results are written to the opened
        // segment.
        let mark_device = device.clone();
        control.set_log_sink(std::sync::Arc::new(move |dut_id: &str, label: &str| {
            let _ = mark_device.log_mark(dut_id, label);
        }));
        let note_device = device.clone();
        control.set_power_outcome_sink(std::sync::Arc::new(move |dut_id: &str, label: &str| {
            let _ = note_device.log_control_boundary(dut_id, label, false);
        }));
        let power_device = device.clone();
        control.set_power_boundary_sink(std::sync::Arc::new(move |dut_id: &str, label: &str| {
            // Success or failure of writing to disk: Controlling entry interception and
            // distribution in case of log segmentation failures (LOG-5)
            power_device.log_control_boundary(dut_id, label, true)
        }));
        // Relay board monitor (design §6.3): the probe blocks on serial I/O, so
        // it lives on its own thread; the 50 ms tick only aligns the watch set
        // (`sync_boards` is idempotent and lock-only) and drains events.
        let relay_probe = Arc::new(duthub_device::control::ControlRelayProbe::new(
            control.clone(),
        ));
        let relay_watch = Arc::new(
            duthub_device::relay_watch::RelayWatchService::new(relay_probe.clone()).with_config(
                duthub_device::relay_watch::RelayWatchConfig {
                    interval_ms: 5_000,
                    confirmations: 1,
                    // Slightly above AlertCenter::DEDUP_WINDOW (30s) so tick jitter
                    // never drops a periodic reminder as a duplicate.
                    remind_interval_ms: 31_000,
                },
            ),
        );
        let (relay_tx, relay_events) = std::sync::mpsc::channel();
        let relay_watch_quit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let device_watch = Arc::new(crate::device_watch::DeviceWatchService::new(Arc::new(
            crate::device_watch::RealDeviceProbe,
        )));
        let (device_watch_tx, device_watch_events) = std::sync::mpsc::channel();
        // Heartbeat Engine (Requirement ⑦): The persistence hook writes the Agent's heartbeat.set
        // back to duts.json.
        // (Bootstrap configuration directory; Load → Modify the heartbeat segment of this DUT →
        // Atomic save.)
        let hb_data_dir = data_dir.clone();
        let heartbeat = std::sync::Arc::new(
            duthub_device::heartbeat::HeartbeatService::new(device.clone()).with_persist(
                std::sync::Arc::new(
                    move |dut_id: &str,
                          cfg: &duthub_core::heartbeat::HeartbeatConfig|
                          -> Result<(), String> {
                        let mut file = duthub_core::store::load_duts(&hb_data_dir)
                            .map_err(|e| e.to_string())?
                            .ok_or_else(|| "duts.json does not exist".to_string())?;
                        let Some(dut) = file.duts.iter_mut().find(|d| d.id == dut_id) else {
                            return Err(format!("DUT {dut_id} not found in duts.json"));
                        };
                        dut.heartbeat = cfg.clone();
                        duthub_core::store::save_duts_atomic(&hb_data_dir, &file)
                            .map_err(|e| e.to_string())
                    },
                ),
            ),
        );
        for dut in &duts.file.duts {
            heartbeat.register(&dut.id, dut.heartbeat.clone());
        }
        // Exception Popup Notification Center (Requirement ⑤): The global suppression list is
        // sourced from settings.alerts. Each DUT's switch settings are stored in duts.json
        // alertsByDut (default setting: enabled); this mechanism shares the same instance with the
        // MCP HTTP thread (GUI popups appear when attempts to control MCP actions fail).
        let alerts = Arc::new(std::sync::Mutex::new(AlertCenter::from_settings(
            &settings.alerts.suppressed_sources,
        )));
        alerts
            .lock()
            .unwrap()
            .set_dut_settings(&duts.file.alerts_by_dut);
        // MCP Port/LAN Switch (Values before the settings were removed by the wb structure; takes
        // effect after rebooting)
        let mcp_port = settings.agent_access.http_port;
        let mcp_lan_access = settings.agent_access.lan_access;
        let mcp_bound_endpoint = Arc::new(OnceLock::new());
        let mcp_tools_config = Arc::new(Mutex::new(crate::mcp_runtime::RuntimeToolsConfig {
            flash_tools: settings.flash_tools.clone(),
            adb_executable: settings.tools.adb_executable.clone(),
        }));
        // Keep a typed handle: `Arc<dyn McpState>` cannot be downcast, and the
        // MCP listeners need a `UserNotifier` for `notify_user` (design §7).
        let runtime_state = Arc::new(crate::mcp_runtime::RuntimeMcpState::new(
            device.clone(),
            snapshot_ref.clone(),
            control.clone(),
            approval,
            heartbeat.clone(),
            media.urls.clone(),
            media.media_op_submitter(),
            alerts.clone(),
            wb_work_dir.clone(),
            access_ctx.clone(),
            mcp_tools_config.clone(),
        ));
        let mcp_state: Arc<dyn duthub_mcp::state::McpState> = runtime_state.clone();
        // A2A Runtime (W4a): Enables the DUT to bind to loopback listeners; MCP ports are
        // registered as "External". If the startup fails, the issue is resolved at the per-DUT
        // level (Failed/PortConflict). In no case does the system panic—the Workbench must still be
        // able to start properly when A2A is unavailable.
        // W4c `--self-test`: Performs a simple self-test without constructing any runtime
        // components (no tokio driver, no listeners).
        let a2a = if self_test {
            None
        } else {
            Some(crate::a2a_runtime::A2aRuntime::start_with_handle(
                &duts,
                &settings,
                mcp_state.clone(),
                shared_runtime.handle().clone(),
            ))
        };
        // HDMI float window close observer (§6.4): a user close (toolbar X or
        // the OS button) hands the preview back to the docked pane without
        // stealing focus. Programmatic closes clear `float_window` first, so
        // they never match here.
        let float_closed_subscription = {
            let weak = cx.weak_entity();
            cx.on_window_closed(move |cx, window_id| {
                // `trail` runs this observer synchronously while the closing
                // window may still hold a lease; re-entering the Workbench one
                // frame later removes the double-lease panic class entirely.
                let weak = weak.clone();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if this
                            .float_window
                            .as_ref()
                            .is_some_and(|float| float.handle.window_id() == window_id)
                        {
                            this.on_float_window_closed(cx);
                        } else if this
                            .main_window
                            .is_some_and(|main| main.window_id() == window_id)
                        {
                            // Last row of the §6.2 table; `Closing` swallows it.
                            this.main_window = None;
                            this.handle_monitor_event(
                                duthub_desktop::capture_ui::MonitorEvent::MainWindowClosed,
                                cx,
                            );
                        }
                    });
                });
            })
        };
        let mut wb = Self {
            relay_board_selected: 0,
            relay_press: None,
            relay_board_name: None,
            relay_board_draft: None,
            agent_host: None,
            agent_host_toasts: Arc::new(Mutex::new(Vec::new())),
            mcp_runtime: crate::mcp_runtime::McpRuntimeHandles::default(),
            mcp_state: mcp_state.clone(),
            mcp_tools_config,
            agent_access: access_ctx.clone(),
            agent_prompts: HashMap::new(),
            agent_transcript_scroll: HashMap::new(),
            agent_transcript_seen: HashMap::new(),
            agent_prompt_clear: HashMap::new(),
            agent_prompt_token: 0,
            agents,
            agents_error,
            agent_bindings,
            agent_registry_edit: None,
            agent_env_selected: None,
            agent_env_value_state: None,
            goose_probe,
            goose_probe_pending: false,
            goose_probe_generation: 0,
            pending_goose_save: None,
            pending_registry_save: None,
            agent_env_read_generation: 0,
            agent_self_test: self_test,
            view: initial_view,
            lang,
            theme_mode,
            p: &theme::DARK,
            device,
            adb_client,
            mcp_bound_endpoint: mcp_bound_endpoint.clone(),
            agent_model_provider,
            agent_model_name,
            job_queue: Arc::new(Mutex::new(Vec::new())),
            next_job: 1,
            tool_installs: duthub_desktop::tool_ui::ToolInstallCenter::new(),
            tool_rows: Vec::new(),
            tool_last_results: HashMap::new(),
            tool_progress_view: HashMap::new(),
            tool_proxy_mode_draft: None,
            usb_discovery: Default::default(),
            usb_debug_view: None,
            settings,
            access_runtime,
            duts,
            data_dir,
            work_dir,
            artifact_root,
            artifact_quota,
            boot_learning_dut: None,
            boot_reference_edits: HashSet::new(),
            boot_similarity_drafts: HashMap::new(),
            sessions,
            active_session: 0,
            serial_ports,
            overlay: None,
            menu_open: false,
            connection_open: false,
            connection_dut_scope: None,
            expanded_mcp_config: None,
            connection_pane_width: None,
            tools_pane_width: None,
            side_panel: crate::side_panel::SidePanel::default(),
            agent,
            find_open: false,
            find_query: String::new(),
            rec_on: false,
            rec_buffer: Vec::new(),
            pending_toasts: toast.into_iter().collect(),
            alerts,
            alert_dialog_open: false,
            active_alert_dialog: None,
            dismiss_active_alert_dialog: false,
            flash_preset_tests: HashMap::new(),
            agent_credential_listener: None,
            agent_token_input: None,
            a2a_tokens: HashMap::new(),
            a2a,
            provider_models: crate::provider_models::Catalog::default(),
            terminal_ctx: None,
            modal: None,
            open_dd: None,
            cfg_search: String::new(),
            drafts: std::collections::HashMap::new(),
            input: String::new(),
            input_focus: cx.focus_handle(),
            terminal_focus_pending: initial_view == ViewMode::Serial,
            text_editors: HashMap::new(),
            textareas: HashMap::new(),
            terminal_scroll: gpui_kit::UniformListScrollHandle::new(),
            serial_rule_selected: 0,
            highlight_cache: std::cell::RefCell::new(HashMap::new()),
            highlight_rules_cache: std::cell::RefCell::new(None),
            sandbox_status_cache: std::cell::RefCell::new(None),
            _shared_runtime: shared_runtime,
            term_grid: None,
            terminal_zoom_size: None,
            terminal_font_picker: None,
            frames,
            core_lines,
            snapshot_ref: snapshot_ref.clone(),
            heartbeat: heartbeat.clone(),
            heartbeat_drafts: HashMap::new(),
            control: control.clone(),
            media,
            frame_pump: crate::monitor_window::FramePump::new(),
            float_window: None,
            main_window: None,
            main_close_hook_installed: false,
            tray: None,
            tray_failed: false,
            tray_tooltip: String::new(),
            hidden: false,
            quitting: false,
            dialog_open: false,
            alert_router: HiddenAlertRouter::new(),
            alert_eval_key: None,
            relay_watch,
            relay_probe,
            relay_events,
            relay_watch_quit,
            relay_watch_signature: Vec::new(),
            device_watch,
            device_watch_events,
            snapshot_due_ms: 0,
            toasts_omitted: 0,
            _float_closed_subscription: Some(float_closed_subscription),
        };
        // Register all saved DUTs into ControlService once at startup (mutations
        // call `sync_control_duts()` instead of re-registering every 250 ms).
        wb.sync_control_duts();
        // Per-DUT garbled-byte detection: mirror `duts.json.garbledByDut` into
        // the device layer (effective = detection && that DUT's pop-up alerts).
        wb.sync_garbled_runtime();
        // Tool downloads: sweep `.tooldown-*` staging directories left behind
        // when the previous process was killed or crashed (the kernel's
        // TempDir cleanup is best-effort against locked files). Runs silently;
        // failures are skipped and retried on the next startup. `--self-test`
        // performs no disk cleanup (kept separate from the pending-cleanup
        // block below so that block's source contract stays byte-identical).
        if !self_test && let Some(tools_root) = duthub_desktop::tool_resolver::config_tools_root() {
            let _ = duthub_desktop::tool_ui::sweep_staging_leftovers(&tools_root);
        }
        // Retry unfinished cleanup after a deletion. Remove the successful entries.
        // Show a toast for remaining failures and retry them at the next startup.
        // Self-test skips all on-disk Agent cleanup.
        if !self_test {
            let (pending_done, pending_failures) =
                duthub_desktop::agent_ui::retry_pending_cleanup(&wb_work_dir);
            if pending_done > 0 {
                wb.pending_toasts.push(i18n::message(
                    "pendingCleanupDone",
                    &[("count", &pending_done.to_string())],
                ));
            }
            for failure in pending_failures {
                wb.pending_toasts.push(i18n::message(
                    "pendingCleanupStillFailed",
                    &[("detail", &failure)],
                ));
            }
            // W4c: External goose path detection backend integration — The process is no longer
            // blocked by file hash +. The `--version` sub-process detection is blocked; results are
            // backed up by generation (discarded after expiration).
            if let Some(path) = wb
                .agents
                .goose()
                .and_then(|goose| goose.program_path())
                .map(str::to_owned)
            {
                wb.goose_probe_generation += 1;
                let generation = wb.goose_probe_generation;
                wb.goose_probe_pending = true;
                let probe = cx.background_executor().spawn(async move {
                    agent_host::registry::probe_program(std::path::Path::new(&path), true).ok()
                });
                cx.spawn(async move |this, cx| {
                    let probe = probe.await;
                    let _ = this.update(cx, |this, _cx| {
                        if this.goose_probe_generation == generation {
                            this.goose_probe = probe;
                            this.goose_probe_pending = false;
                        }
                    });
                })
                .detach();
            }
        }
        // Relay board probe thread (design §6.3). Its own block so the
        // self-test purity contract on the pending-cleanup block above stays
        // byte-identical (pit #79). The thread only probes and queues events;
        // the UI thread owns `AlertCenter`/toasts.
        if !self_test {
            let watch = wb.relay_watch.clone();
            let quit = wb.relay_watch_quit.clone();
            std::thread::Builder::new()
                .name("duthub-relay-watch".into())
                .spawn(move || {
                    while !quit.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(2));
                        if quit.load(Ordering::Relaxed) {
                            break;
                        }
                        watch.tick_at(crate::alerts::now_ms());
                        for event in watch.drain_events() {
                            if relay_tx.send(event).is_err() {
                                return;
                            }
                        }
                    }
                })
                .ok();
            let dev_watch = wb.device_watch.clone();
            let dev_quit = wb.relay_watch_quit.clone();
            std::thread::Builder::new()
                .name("duthub-device-watch".into())
                .spawn(move || {
                    while !dev_quit.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(2));
                        if dev_quit.load(Ordering::Relaxed) {
                            break;
                        }
                        dev_watch.tick_at(crate::alerts::now_ms());
                        for event in dev_watch.drain_events() {
                            if device_watch_tx.send(event).is_err() {
                                return;
                            }
                        }
                    }
                })
                .ok();
        }
        // Wake the UI when serial events arrive and drain the events already in the queue.
        cx.spawn(async move |this, cx| {
            loop {
                let first = match bus_rx.recv().await {
                    Ok(event) => Ok(event),
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        Err(broadcast::error::TryRecvError::Lagged(count))
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if this
                    .update(cx, |view, cx| {
                        if view.absorb_device_events(first, &mut bus_rx) > 0 {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                // Yield during continuous serial output so other UI tasks can process input.
                tokio::task::yield_now().await;
            }
        })
        .detach();

        // Periodic tasks redraw the window only after state changes.
        // Keeping idle hitboxes stable prevents a redraw from interrupting a click.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let changed = this
                    .update(cx, |view, cx| {
                        view.media.tick(&view.duts)
                            | view.poll_relay_press()
                            | view.tick_side_panel()
                            | view.drain_jobs()
                            | view.drain_tool_installs()
                            | view.heartbeat_tick()
                            | view.agent_host_tick()
                            | view.sync_relay_watch()
                            | view.drain_relay_events()
                            | view.sync_device_watch()
                            | view.drain_device_watch_events()
                            | view.alert_present_tick()
                            | view.drain_tray_events(cx)
                            | view.snapshot_tick()
                    })
                    .unwrap_or(false);
                if !changed {
                    continue;
                }
                if this.update(cx, |_view, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();

        // HDMI frame pump (§6.3): a single consumer for `InlinePreview::poll`,
        // driven at 100ms while a preview is attached and backed off to 400ms
        // while idle to save wakeups.
        cx.spawn(async move |this, cx| {
            loop {
                let active = this
                    .update(cx, |view, _cx| view.frame_pump.key().is_some())
                    .unwrap_or(false);
                let interval = if active {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(400)
                };
                cx.background_executor().timer(interval).await;
                let changed = this.update(cx, Workbench::tick_frame_pump).unwrap_or(false);
                if !changed {
                    continue;
                }
                if this.update(cx, |_view, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Bind MCP HTTP to loopback by default or all interfaces in LAN mode.
        // The background worker also persists state snapshots.
        // Workbench retains the handles and stops the runtime when the application exits.
        // The self-test skips starting any HTTP listeners.
        if !self_test {
            wb.ensure_dut_mcp_endpoints();
            wb.mcp_runtime = crate::mcp_runtime::spawn_mcp_runtime(
                &mcp_state,
                Some(runtime_state as Arc<dyn duthub_mcp::notify::UserNotifier>),
                access_ctx.as_ref(),
                mcp_port,
                mcp_lan_access,
                wb.settings.agent_access.listeners.clone(),
                snapshot_ref.clone(),
                &wb_work_dir,
                &mcp_bound_endpoint,
            );
        }
        wb
    }

    #[expect(dead_code, reason = "convenience palette accessor on Workbench")]
    pub fn p(&self) -> &'static Palette {
        self.p
    }

    fn palette(&self) -> &'static Palette {
        match self.theme_mode {
            theme::ThemeMode::Dark => &theme::DARK,
            theme::ThemeMode::Light => &theme::LIGHT,
        }
    }

    pub fn notify_toast(&mut self, message: impl Into<String>) {
        self.push_toasts(std::iter::once(message.into()));
    }

    /// Bounded toast enqueue: while hidden nothing drains `pending_toasts`,
    /// so the oldest entries are dropped and counted (reported on restore).
    fn push_toasts<I: IntoIterator<Item = String>>(&mut self, toasts: I) {
        for message in toasts {
            if self.pending_toasts.len() >= MAX_TOASTS {
                self.pending_toasts.remove(0);
                self.toasts_omitted += 1;
            }
            self.pending_toasts.push(message);
        }
    }
}

mod agent_ctrl;
mod inputs;
mod render;
mod sessions;
mod settings_ctrl;
mod terminal;
mod tray_ctrl;
// ---------------------------------------------------------------------------
// ＋ Forms → DUT Consolidation (Review found issue P1-1: Editing should be based on the original
// configuration, only the modified fields need to be updated.)
// ---------------------------------------------------------------------------

/// Fields that are displayed directly on the form and are always updated upon saving.
#[cfg(test)]
pub(crate) struct DutFormFields {
    pub name: String,
    pub port: String,
    pub baud: u32,
    /// `Some` = Update the ADB serial number (Leave the field empty if there is no existing
    /// binding.)
    pub adb_serial: Option<String>,
    pub flash: FlashConfig,
}

/// Changes to the form associated with the relay:
/// – `Unchanged`: The user does not modify the chip configuration. Only the nodes of the same type
/// are updated, while the SNMP PDU/channel mappings remain unchanged.
/// – `ReplaceUsb` / `ReplaceDisabled`: The binding is only replaced if the user explicitly selects
/// the desired type during form editing.
#[cfg(test)]
pub(crate) enum RelayFormChange {
    Unchanged { node: String },
    ReplaceUsb { node: String },
    ReplaceDisabled,
}

/// Merge in edit mode. Media, unchanged relays (including those with PDU socket bindings/receiver
/// channels), and other MCP transmission formats (stdio) will remain intact.
#[cfg(test)]
pub(crate) fn merge_dut_form(
    existing: &mut duthub_core::model::DutConfig,
    form: DutFormFields,
    relay: RelayFormChange,
    mcp_url: Option<String>,
) {
    use duthub_core::model::{McpConfig, RelayConfig};
    existing.name = form.name;
    existing.serial.port = form.port;
    existing.serial.baud = form.baud;
    if let Some(serial) = form.adb_serial {
        existing.adb.serial = serial;
    }
    existing.flash = form.flash;
    match relay {
        RelayFormChange::Unchanged { node } => {
            if let RelayConfig::UsbSerial { node: current, .. } = &mut existing.relay {
                *current = node;
            }
        }
        RelayFormChange::ReplaceUsb { node } => {
            existing.relay = RelayConfig::UsbSerial {
                host: "local".into(),
                node,
                driver: String::new(),
                channels: duthub_core::model::RelayChannels::default(),
            };
        }
        RelayFormChange::ReplaceDisabled => existing.relay = RelayConfig::Disabled,
    }
    if let Some(url) = mcp_url {
        existing.mcp = McpConfig::StreamableHttp { url };
    }
}

#[cfg(test)]
mod dut_form_tests {
    use super::*;

    #[test]
    fn empty_relay_preset_only_requires_a_port_when_bound() {
        use duthub_core::model::{RelayBoard, RelayChannels, RelayConfig};

        let mut file = store::default_duts();
        file.duts.push(demo_dut_template("DUT-31", "COM31"));
        file.relay_boards.push(RelayBoard {
            id: "ch340-relay".into(),
            name: "ch340-relay".into(),
            host: "local".into(),
            node: String::new(),
            driver: "ch340".into(),
            channel_count: 4,
        });
        assert!(Workbench::validate_bindings(&mut file).is_ok());

        file.duts[0].relay = RelayConfig::UsbBoard {
            board_id: "ch340-relay".into(),
            channels: RelayChannels::default(),
        };
        assert!(Workbench::validate_bindings(&mut file).is_err());

        file.relay_boards[0].node = "COM25".into();
        assert!(Workbench::validate_bindings(&mut file).is_ok());
    }
    use duthub_core::model::{PduOutlets, RelayChannels, SnmpConfig};

    /// Portable archive name management: The format of the names must be preserved (names are
    /// stored directly in COM ports). Otherwise, using `open_dut_for_write` will result in rejected
    /// requests one by one. If write threads silently overwrite incorrect data, all logs will be
    /// lost.
    #[test]
    fn archive_safe_name_guards_windows_reserved_shapes() {
        // Windows reserved names are in the format COM1–9 / LPT1–9 (single digits); COM10 is not
        // reserved. For other cases, it's better to use the dut_id itself (program-specific IDs,
        // which usually start with "dut-" and are valid).
        assert_eq!(archive_safe_name("COM8", "dut-com8"), "dut-com8");
        assert_eq!(archive_safe_name("com1", "dut-com1"), "dut-com1");
        assert_eq!(archive_safe_name("lpt2", "dut-lpt2"), "dut-lpt2");
        assert_eq!(archive_safe_name("com10", "dut-com10"), "com10");
        // Preserve valid names verbatim so existing DUT archives keep their original directory
        // names.
        assert_eq!(archive_safe_name("RK3576", "dut-rk3576"), "RK3576");
        assert_eq!(archive_safe_name("开发板 A", "dut-dev-a"), "开发板 A");
    }

    #[test]
    fn open_tab_uses_the_current_name_of_its_bound_dut() {
        let session = SessionView::new(
            "dut-a".into(),
            "session".into(),
            "旧名 · COM8".into(),
            "device",
            115_200,
        );
        let mut bound = usb_relay_dut();
        bound.serial.port = "COM8".into();
        let mut other = bound.clone();
        other.id = "dut-b".into();
        other.name = "另一个 DUT".into();
        bound.name = "新名称".into();
        assert_eq!(
            session.display_title(&[other.clone(), bound.clone()]),
            "新名称 · COM8"
        );
        bound.name.clear();
        assert_eq!(session.display_title(&[other, bound]), "旧名 · COM8");
    }

    #[test]
    fn usb_debug_selection_recognizes_existing_and_unassigned_bindings() {
        use duthub_core::model::AdbConfig;
        assert!(!usb_debug_enabled(&AdbConfig::default()));
        for mode in ["auto", "usb"] {
            assert!(usb_debug_enabled(&AdbConfig {
                mode: mode.into(),
                ..AdbConfig::default()
            }));
        }
        assert!(usb_debug_enabled(&AdbConfig {
            serial: "saved-serial".into(),
            ..AdbConfig::default()
        }));
    }

    fn usb_relay_dut() -> duthub_core::model::DutConfig {
        duthub_core::model::DutConfig {
            id: "dut-a".into(),
            name: "旧名".into(),
            serial: duthub_core::model::SerialConfig {
                host: "local".into(),
                port: "COM3".into(),
                baud: 115_200,
                format: "8N1".into(),
                reference_log: None,
                reference_similarity: None,
                split_logs: false,
            },
            adb: duthub_core::model::AdbConfig::default(),
            relay: duthub_core::model::RelayConfig::UsbSerial {
                host: "local".into(),
                node: "COM7".into(),
                driver: "CH340".into(),
                channels: RelayChannels {
                    power: Some(4),
                    reset: Some(1),
                    recovery: Some(3),
                    maskrom: Some(2),
                    power_key: Some(5),
                },
            },
            mcp: duthub_core::model::McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8787/mcp".into(),
            },
            a2a: duthub_core::model::DutA2aConfig::default(),
            flash: FlashConfig {
                preset_id: Some("preset-upgrade_tool".into()),
                executable: "upgrade_tool".into(),
                test_args: vec!["--help".into()],
            },
            media: duthub_core::model::DutMediaConfig::default(),
            heartbeat: duthub_core::HeartbeatConfig::default(),
        }
    }

    fn rename_form() -> DutFormFields {
        DutFormFields {
            name: "新名字".into(),
            port: "COM3".into(),
            baud: 115_200,
            adb_serial: None,
            flash: FlashConfig {
                preset_id: Some("preset-upgrade_tool".into()),
                executable: "upgrade_tool".into(),
                test_args: vec!["--help".into()],
            },
        }
    }

    #[test]
    fn rename_only_keeps_usb_relay_channels() {
        let mut dut = usb_relay_dut();
        merge_dut_form(
            &mut dut,
            rename_form(),
            RelayFormChange::Unchanged {
                node: "COM7".into(),
            },
            None,
        );
        assert_eq!(dut.name, "新名字");
        let duthub_core::model::RelayConfig::UsbSerial { node, channels, .. } = &dut.relay else {
            panic!("Preserve the USB relay binding");
        };
        assert_eq!(node, "COM7");
        assert_eq!(channels.power, Some(4));
        assert_eq!(channels.reset, Some(1));
        assert_eq!(channels.power_key, Some(5));
        // The media item is not displayed in the form → Keep it intact
        assert_eq!(dut.media, duthub_core::model::DutMediaConfig::default());
    }

    #[test]
    fn rename_only_keeps_snmp_pdu_binding() {
        let mut dut = usb_relay_dut();
        dut.relay = duthub_core::model::RelayConfig::SnmpPdu {
            snmp: SnmpConfig {
                address: "192.168.1.80".into(),
                version: "3".into(),
                credential_ref: "keyring:pdu-1".into(),
            },
            outlets: PduOutlets { power: 3 },
        };
        merge_dut_form(
            &mut dut,
            rename_form(),
            // The form cannot present the PDU: When the chip is in the unmodified state, it is
            // treated as "Unchanged".
            RelayFormChange::Unchanged {
                node: String::new(),
            },
            None,
        );
        let duthub_core::model::RelayConfig::SnmpPdu { snmp, outlets } = &dut.relay else {
            panic!("Preserve the SNMP PDU binding");
        };
        assert_eq!(snmp.address, "192.168.1.80");
        assert_eq!(snmp.credential_ref, "keyring:pdu-1");
        assert_eq!(outlets.power, 3);
    }

    #[test]
    fn explicit_mode_change_replaces_binding() {
        let mut dut = usb_relay_dut();
        merge_dut_form(
            &mut dut,
            rename_form(),
            RelayFormChange::ReplaceDisabled,
            None,
        );
        assert!(matches!(
            dut.relay,
            duthub_core::model::RelayConfig::Disabled
        ));

        let mut dut = usb_relay_dut();
        dut.relay = duthub_core::model::RelayConfig::Disabled;
        merge_dut_form(
            &mut dut,
            rename_form(),
            RelayFormChange::ReplaceUsb {
                node: "COM9".into(),
            },
            None,
        );
        let duthub_core::model::RelayConfig::UsbSerial { node, .. } = &dut.relay else {
            panic!("Switching to a USB relay must replace the binding");
        };
        assert_eq!(node, "COM9");
    }

    #[test]
    fn unchanged_usb_updates_node_only() {
        let mut dut = usb_relay_dut();
        merge_dut_form(
            &mut dut,
            rename_form(),
            RelayFormChange::Unchanged {
                node: "COM12".into(),
            },
            Some("http://127.0.0.1:9000/mcp".into()),
        );
        let duthub_core::model::RelayConfig::UsbSerial { node, channels, .. } = &dut.relay else {
            panic!("The binding type must be preserved");
        };
        assert_eq!(node, "COM12");
        assert_eq!(channels.power, Some(4));
        assert!(matches!(
            &dut.mcp,
            duthub_core::model::McpConfig::StreamableHttp { url }
                if url == "http://127.0.0.1:9000/mcp"
        ));
    }

    #[test]
    fn empty_adb_serial_keeps_existing_binding() {
        let mut dut = usb_relay_dut();
        dut.adb.serial = "RK3576USB002".into();
        merge_dut_form(
            &mut dut,
            rename_form(),
            RelayFormChange::Unchanged {
                node: String::new(),
            },
            None,
        );
        assert_eq!(dut.adb.serial, "RK3576USB002");
    }

    #[test]
    fn media_fields_edit_and_backfill_per_dut_and_source() {
        use crate::media::Source;
        type Field = crate::media_page::MediaField;
        let mut settings = duthub_core::model::CaptureSettings::default();
        let mut dut = usb_relay_dut();
        dut.media.hdmi.enabled = Some(false);
        // Write data field by field (for editing purposes), then read back the data from those
        // fields – both processes are carried out independently.
        let fields = [
            (Field::Video(Source::Hdmi), "UGREEN 35866"),
            (Field::Audio(Source::Hdmi), "数字输入 (UGREEN 35866)"),
            (Field::Size(Source::Hdmi), "1920x1080"),
            (Field::Rate(Source::Hdmi), "60"),
            (Field::Port(Source::Hdmi), "8554"),
            (Field::Video(Source::Camera), "Integrated Camera"),
            (Field::Audio(Source::Camera), ""),
            (Field::Size(Source::Camera), "1280x720"),
            (Field::Rate(Source::Camera), "30"),
            (Field::Port(Source::Camera), "0"),
        ];
        for (field, value) in fields {
            let dest = field
                .edit(&mut settings, Some(&mut dut))
                .expect("The DUT field must be editable.");
            *dest = value.into();
            assert_eq!(field.value(&settings, Some(&dut)), value, "{field:?}");
        }
        // The write operation has been performed on dut.media. Now, it waits for save_duts() to
        // persist the data.
        assert_eq!(dut.media.hdmi.video, "UGREEN 35866");
        assert!(dut.media.hdmi.has_video());
        assert_eq!(dut.media.hdmi.frame_rate, "60");
        assert_eq!(dut.media.hdmi.port, "8554");
        assert_eq!(dut.media.camera.video, "Integrated Camera");
        assert_eq!(
            dut.media.camera.audio, "",
            "Leave the audio field empty = No audio recording"
        );
        // Tool-level settings (ffmpeg/ffplay/IP listening) are processed through the same editing
        // channel, but they are not connected to the DUT.
        for (field, value) in [
            (Field::Ffmpeg, "ffmpeg"),
            (Field::Ffplay, "%FFMPEG_HOME%/bin/ffplay.exe"),
            (Field::Listen, "127.0.0.1"),
        ] {
            *field.edit(&mut settings, None).unwrap() = value.into();
            assert_eq!(field.value(&settings, None), value);
        }
        assert_eq!(settings.ffmpeg, "ffmpeg");
        assert_eq!(settings.ffplay, "%FFMPEG_HOME%/bin/ffplay.exe");
        assert_eq!(settings.listen_ip, "127.0.0.1");
    }
}

pub(crate) fn usb_debug_enabled(config: &duthub_core::model::AdbConfig) -> bool {
    matches!(config.mode.as_str(), "usb" | "auto") || !config.serial.trim().is_empty()
}

/// Use `dut_id` when `name_component` rejects a DUT name, such as the Windows reserved name COM8.
/// Otherwise `open_dut_for_write` rejects every write and the writer can lose all logs. Preserve
/// valid names verbatim so existing archives keep their original directories.
fn archive_safe_name(name: &str, dut_id: &str) -> String {
    for candidate in [name, dut_id] {
        if duthub_core::dut_artifacts::name_component(candidate).is_ok() {
            return candidate.to_string();
        }
    }
    format!("dut-{dut_id}")
}

fn demo_dut_template(name: &str, port: &str) -> duthub_core::model::DutConfig {
    duthub_core::model::DutConfig {
        id: format!("dut-{}", name.to_lowercase().replace(' ', "-")),
        name: name.to_string(),
        serial: duthub_core::model::SerialConfig {
            host: "local".into(),
            port: port.to_string(),
            baud: 115_200,
            format: "8N1".into(),
            reference_log: None,
            reference_similarity: None,
            split_logs: false,
        },
        adb: duthub_core::model::AdbConfig::default(),
        relay: duthub_core::model::RelayConfig::Disabled,
        mcp: duthub_core::model::McpConfig::StreamableHttp {
            url: "http://127.0.0.1:8787/mcp".into(),
        },
        a2a: duthub_core::model::DutA2aConfig::default(),
        flash: FlashConfig::default(),
        media: duthub_core::model::DutMediaConfig::default(),
        heartbeat: duthub_core::HeartbeatConfig::default(),
    }
}

// The unified entry point for background tasks: gpui background executor cannot be accessed
// directly through &mut Self; it must be processed as a thread instead.
fn cx_spawn_job(f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(f);
}

/// Failed to install the localization header for tools (handled through lib layer enumeration and
/// bin layer i18n handling; issue #66). The technical details string will be displayed as it is in
/// the original kernel error message; no translation required.
pub(crate) fn tool_failure_label(
    kind: duthub_desktop::tool_ui::InstallFailureKind,
) -> &'static str {
    use duthub_desktop::tool_ui::InstallFailureKind as K;
    match kind {
        K::InvalidDownloadUrl => "toolFailDownloadUrl",
        K::ProxyInvalidUrl => "toolFailProxyUrl",
        K::ClientBuild => "toolFailClientBuild",
        K::Network => "toolFailNetwork",
        K::HttpStatus => "toolFailHttpStatus",
        K::FileSystem => "toolFailFileSystem",
        K::UnsafeDestination => "toolFailUnsafeDestination",
        K::UnsafeArchiveEntry => "toolFailUnsafeArchive",
        K::ArchiveUnreadable => "toolFailArchiveUnreadable",
        K::ArchiveRead => "toolFailArchiveRead",
        K::ArchiveBomb => "toolFailArchiveBomb",
        K::MissingArchiveEntry => "toolFailMissingEntry",
        K::ChecksumMismatch => "toolFailChecksumMismatch",
        K::ManifestCorrupt => "toolFailManifest",
        K::Internal => "toolFailInternal",
    }
}

impl Render for Workbench {
    #[tracing::instrument(name = "desktop.render", skip_all)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_root(window, cx)
    }
}
