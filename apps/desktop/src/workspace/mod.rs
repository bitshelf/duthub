//! 工作台外壳 —— 1:1 移植原型 `#page-serial` + 全局件：
//! tabbar（menu/spark/divider/tabs/add/工具栏）→ find 条 → 终端（含 edit-row）→ 命令 dock
//! → workspace-split（终端 | Agent 面板）→ zed-statusbar；浮层：配置壳 / 菜单 / 弹窗 / Toast。
//!
//! 输入体系：`In` 枚举标识设置字段，gpui-kit 组件负责文本编辑；
//! 下拉：`Dd`/select id → `open_dd`；弹窗：`ModalState`；通知：kit Notification（Root 层）。

use std::collections::HashMap;
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
use crate::components::{BtnVariant, btn, icon_btn, typed_char};
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

/// Provider → `settings.agent.provider` 的稳定写法（`Provider::from_setting` 的反向）。
fn provider_setting(provider: duthub_core::Provider) -> &'static str {
    match provider {
        duthub_core::Provider::Openai => "openai",
        duthub_core::Provider::Anthropic => "anthropic",
        duthub_core::Provider::Gemini => "google",
        duthub_core::Provider::Deepseek => "deepseek",
        duthub_core::Provider::Qwen => "qwen",
        duthub_core::Provider::Compatible => "compatible",
    }
}

/// GoosePath 输入的显示值：草稿优先，**空串草稿也是有效草稿**——已记录的外部路径
/// 不得把用户清空后的输入回填成旧值（否则「清空 → 校验并保存」会重新校验旧路径）。
fn goose_path_field_value(draft: Option<&str>, recorded: Option<&str>) -> String {
    draft.map_or_else(|| recorded.unwrap_or_default().to_owned(), str::to_owned)
}

#[cfg(test)]
mod goose_path_tests {
    use super::goose_path_field_value;

    #[test]
    fn empty_draft_wins_over_recorded_path_so_clearing_sticks() {
        // 复现修复前的缺陷：已记录路径 + 清空输入 → 不能回填旧路径。
        assert_eq!(
            goose_path_field_value(Some(""), Some("C:/old/goose.exe")),
            ""
        );
        // 没有草稿时才回显已记录路径（初次渲染）。
        assert_eq!(
            goose_path_field_value(None, Some("C:/old/goose.exe")),
            "C:/old/goose.exe"
        );
        // 非空草稿始终优先。
        assert_eq!(
            goose_path_field_value(Some("D:/new/goose.exe"), Some("C:/old/goose.exe")),
            "D:/new/goose.exe"
        );
        assert_eq!(goose_path_field_value(None, None), "");
    }
}

// ---------------------------------------------------------------------------
// 输入与下拉标识
// ---------------------------------------------------------------------------

struct BoundEditor<T: 'static> {
    state: gpui_kit::Entity<T>,
    model_value: String,
    placeholder: String,
    scope: Option<String>,
    _subscription: gpui_kit::Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)] // Prompt 预留给终端 prompt 前缀
pub enum In {
    Find,
    CfgSearch,
    Target,
    Prompt,
    /// Agent 面板「模型名称」（探针/任务调用的模型 id）
    AgentModel,
    /// Agent 设置页「外部 goose 路径」草稿（坑 #41：input_value/input_set 成对）
    GoosePath,
    /// Agent 页注册表编辑器：名称 / 程序 / 参数 / 环境变量键名
    AgentRegName,
    AgentRegProgram,
    AgentRegArgs,
    AgentRegEnvKeys,
    /// 注册表编辑器 env 值掩码输入（W4b）：值只进 keyring，不进 agents.json
    AgentRegEnvValue,
    Port,
    Baud,
    /// 会话状态条的波特率热切换输入（按活动会话独立草稿）
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
    /// 串口设置页：选中的高亮规则正则（serial.serialRuleSelected 指向的规则）
    SerialRuleRegex,
    SerialRuleDescription,
    /// 串口设置页「滚动缓冲行数」草稿（保存按钮提交，Baud 式解析）
    SerialScrollback,
    /// Capture settings page: idle-reclaim seconds draft
    /// (`tools.capture.idleSecs`; empty = default 60, committed by its save
    /// button after validation)
    CaptureIdle,
    AdbExe,
    HdcExe,
    WorkDir,
    DutStorageGib,
    /// 连接页「心跳间隔（秒）」（dut.heartbeat.interval_secs）
    HeartbeatInterval,
    /// 连接页「心跳命令」（dut.heartbeat.command；Agent 可经 heartbeat.set 改写）
    HeartbeatCommand,
    HeartbeatExpectedResponse,
    /// 设置页「工作目录」输入（settings.workspace_dir；保存后重启生效）
    /// 设置页「Agent 接入」：监听端口 / 租约 TTL（草稿输入，保存按钮提交）
    AgentAccessPort,
    AgentAccessExtraPort,
    AgentAccessTtl,
    /// 「Agent 接入」新增凭据的 Agent 名称（草稿；token 由服务端生成）
    AgentAccessCredName,
    /// 连接管理：DUT 专用 MCP 端口（草稿；空 = 自动分配）
    DutMcpPort,
    /// 连接管理：DUT 专用 A2A 端口 / Agent 名称（草稿，保存按钮提交）
    DutA2aPort,
    DutA2aName,
    /// 设置页「下载代理」自定义 URL 草稿（保存按钮校验后写模型，坑 #41 成对）
    ProxyUrl,
    ToolDownloadUrl(&'static str),
    Media(crate::media_page::MediaField),
    A2aName,
    A2aPort,
    CmdName,
    CmdValue,
    /// 新建快捷命令：图标名（草稿，空 = bolt）
    CmdIcon,
    NewPresetName,
    NewPresetExe,
    /// 新建烧录预设：测试参数（单条字符串，空格分隔；空 = 探针回落 --version）
    NewPresetTestArgs,
    PeerName,
    PeerUrl,
    RenameName,
    // ＋ 新建 DUT 表单（全配置）
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
    /// 串口终端设置（与控制设备同级）：高亮规则 / 键盘映射 / 缓冲 / 日志
    Serial,
    Media,
    /// 工具下载独立页（外部工具安装/更新 + 下载代理，独立于设置页）
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
    /// 第三个模态字段（快捷命令图标 / 烧录预设测试参数）。
    pub draft3: String,
}

// ---------------------------------------------------------------------------
// 终端会话视图
// ---------------------------------------------------------------------------

/// 会话链路状态（H1.4 重连显示）：由 device 会话的 [serial] 行与断线事件推导。
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
    /// 链路状态（仅 device 会话跟踪；demo 恒为 Connected）
    pub link: LinkState,
    /// 重连计数（跟随驱动"第 N 次重连"行；成功打开后归零）
    pub reconnect_attempts: u32,
    /// 设备侧最近确认的波特率（来自"[serial] … @ N 已打开"行；0 = 未确认）
    pub baud: u32,
    /// 波特率热切换输入草稿（None = 显示当前值）
    pub baud_draft: Option<String>,
    /// real 会话的终端网格快照（alacritty 仿真；PuTTY 行为的渲染源）。
    /// device 会话的 SerialChunk 里仅注记行（[serial]/[mark]/错误）进 `lines`，
    /// 设备输出一律走这里的网格行。Arc 共享：吸收事件零深拷贝，渲染只读。
    pub screen: Option<std::sync::Arc<duthub_core::ipc::ScreenSnapshot>>,
    history: Option<duthub_device::term::TerminalHistoryPage>,
    history_pending: Option<uuid::Uuid>,
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
            history: None,
            history_pending: None,
            last_garbled_level: GarbleLevel::Clean,
            crash_line_tail: String::new(),
            selection: None,
        }
    }

    fn displayed_screen(&self) -> Option<&duthub_core::ipc::ScreenSnapshot> {
        self.history
            .as_ref()
            .map(|page| &page.screen)
            .or(self.screen.as_deref())
    }

    /// 整屏快照入视图（渲染源分层，坑 #22/#49）：device 会话整体替换
    /// screen Arc（零拷贝、不进 lines——设备输出只来自网格快照）；
    /// demo 会话没有仿真器，忽略快照、保持全量行渲染。
    fn absorb_serial_screen(&mut self, screen: Arc<duthub_core::ipc::ScreenSnapshot>, seq: u64) {
        if self.source == "device" {
            self.last_garbled_level = screen.garbled.level;
            self.screen = Some(screen);
            self.last_seq = seq;
        }
    }

    /// SerialChunk → lines 的路由判定（device 会话 PuTTY 分层）：设备输出
    /// （Normal/Good/Prompt）由仿真器网格渲染，不得重复进注记列表；
    /// "[serial] … @ N 已打开"信令行消费进链路状态后同样不进列表；
    /// demo 会话保持全量行渲染（返回 true = 入队）。
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

/// 从"[serial] {port} @ {baud} 已打开"行解析设备侧确认的波特率。
fn parse_serial_open_baud(text: &str) -> Option<u32> {
    let rest = text.strip_prefix("[serial] ")?;
    let at = rest.find(" @ ")?;
    let tail = &rest[at + 3..];
    let end = tail.find(" 已打开")?;
    tail[..end].parse().ok()
}

/// 键盘回车行尾字节（「串口」设置段 serial.enterEnding 驱动；None/Cr = CR 0x0d，
/// Lf / Crlf 可选）。返回值就是入队下发的完整字节——键盘路径不追加额外行尾。
/// 仅键盘路径生效；MCP/心跳写入的行尾策略是会话级独立配置，不经此处。
fn serial_enter_bytes(ending: Option<duthub_core::model::SerialEnterEnding>) -> &'static [u8] {
    match ending {
        Some(duthub_core::model::SerialEnterEnding::Lf) => b"\n",
        Some(duthub_core::model::SerialEnterEnding::Crlf) => b"\r\n",
        _ => b"\r",
    }
}

/// 键盘退格字节（「串口」设置段 serial.backspace 驱动；None/Del = DEL 0x7f，
/// CtrlH = 0x08，部分 BSP shell / 老工具需要）。
fn serial_backspace_byte(mode: Option<duthub_core::model::SerialBackspace>) -> u8 {
    match mode {
        Some(duthub_core::model::SerialBackspace::CtrlH) => 0x08,
        _ => 0x7f,
    }
}

/// 方向键/home/end 的转义前缀：应用光标模式（DECSET ?1，快照模式位）用 SS3 'O'，
/// 普通模式用 CSI '['。跟随设备经 ANSI 切换的终端模式，不硬编码覆盖。
fn arrow_escape_prefix(application_cursor: bool) -> u8 {
    if application_cursor { b'O' } else { b'[' }
}

/// 粘贴文本的终端化（PuTTY 行为）：行尾统一为 CR（多行粘贴 = 逐行回车），
/// 剔除其余控制字符（保留 \t 与 CR）；设备开启 bracketed-paste（DECSET ?2004，
/// 快照模式位）时按 \x1b[200~/201~ 包裹，让 shell 原样接收而非逐键解释。
/// 清洗后为空返回 None（无内容不发送）。
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
        let mut settings: TerminalSettings = serde_json::from_str(r#"{}"#).unwrap();
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
            Some(115200)
        );
        assert_eq!(
            parse_serial_open_baud("[serial] COM30 @ 9600 已打开"),
            Some(9600)
        );
        // 打开失败 / 重连 / TX 回显行不误报
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
// 串口终端键盘 / 渲染分层（AGENTS.md 坑 #22、#46、#49、#50）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod serial_terminal_tests {
    use super::{
        LineKind, LogLine, SessionView, arrow_escape_prefix, prepare_paste_bytes,
        serial_backspace_byte, serial_enter_bytes,
    };
    use duthub_core::ipc::{ChunkStyle, ScreenRow, ScreenSnapshot};
    use duthub_core::model::{SerialBackspace, SerialEnterEnding, SerialSettings};

    /// 构造快照（device 会话渲染源；模式位默认全关）。
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
            garbled: Default::default(),
        }
    }

    /// IO-3：键盘回车行尾由「串口」设置段驱动（enterEnding 默认 CR 0x0d，可选
    /// LF / CRLF），返回值就是完整入队字节——键盘路径不追加额外行尾。
    /// 字节经 send_bytes→write_bytes 原样到达设备需实机复核，字节选择规则软件可测。
    #[test]
    fn enter_bytes_follow_serial_settings_with_cr_default() {
        assert_eq!(serial_enter_bytes(None), b"\r");
        assert_eq!(serial_enter_bytes(Some(SerialEnterEnding::Cr)), b"\r");
        assert_eq!(serial_enter_bytes(Some(SerialEnterEnding::Lf)), b"\n");
        assert_eq!(serial_enter_bytes(Some(SerialEnterEnding::Crlf)), b"\r\n");
        // 默认恰好一个 0x0d：没有隐式追加的第二个字节
        assert_eq!(serial_enter_bytes(None).len(), 1);
        assert_eq!(serial_enter_bytes(None)[0], 0x0d);
    }

    /// IO-3：键盘退格字节由「串口」设置段驱动（默认 DEL 0x7f，可选 Ctrl-H 0x08）。
    #[test]
    fn backspace_byte_follows_serial_settings_with_del_default() {
        assert_eq!(serial_backspace_byte(None), 0x7f);
        assert_eq!(serial_backspace_byte(Some(SerialBackspace::Del)), 0x7f);
        assert_eq!(serial_backspace_byte(Some(SerialBackspace::CtrlH)), 0x08);
    }

    /// IO-3：键盘字节读的是同一份 settings.serial——JSON 配置键是 camelCase
    /// （坑 #15：snake_case 键会静默落默认值，断言会退化成默认值假绿，必须
    /// 用 enterEnding/backspace/ctrlH 这类真实键名解析）。
    #[test]
    fn keyboard_bytes_follow_camel_case_serial_settings() {
        let settings: SerialSettings =
            serde_json::from_str(r#"{"enterEnding": "lf", "backspace": "ctrlH"}"#)
                .expect("「串口」设置段键名为 camelCase");
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

    /// IO-4：方向键/home/end 的转义前缀从快照 application_cursor 模式位推导——
    /// 普通模式 CSI '['，应用光标模式 SS3 'O'。设备经 ANSI（DECSET ?1）切换
    /// 模式后桌面端跟随，不得硬编码覆盖；完整键→设备链路需实机复核，
    /// 前缀决策规则软件可测（模式位本身由 term.rs 既有测试锁定）。
    #[test]
    fn arrow_prefix_follows_snapshot_application_cursor_bit() {
        let snapshot = screen(&["root:~# "], false, false);
        assert_eq!(arrow_escape_prefix(snapshot.application_cursor), b'[');
        // 组合形状与 on_key_down 同构：ESC + 前缀 + 键码
        let normal_seq = [0x1b, arrow_escape_prefix(snapshot.application_cursor), b'A'];
        assert_eq!(normal_seq.as_slice(), &b"\x1b[A"[..]);

        let snapshot = screen(&["root:~# "], true, false);
        assert_eq!(arrow_escape_prefix(snapshot.application_cursor), b'O');
        let app_seq = [0x1b, arrow_escape_prefix(snapshot.application_cursor), b'A'];
        assert_eq!(app_seq.as_slice(), &b"\x1bOA"[..]);
    }

    /// IO-4：粘贴的 bracketed-paste 包裹（\x1b[200~/201~）从快照
    /// bracketed_paste 模式位推导，不硬编码；模式关闭时文本原样下发。
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

    /// IO-4（粘贴清洗，PuTTY 行为）：多行粘贴行尾统一为 CR（逐行回车），
    /// 剔除其余控制字符（保留 \t 与 CR）；纯控制字符内容不发送。
    #[test]
    fn paste_sanitizes_line_endings_and_control_chars() {
        // \r\n / \n 统一为 \r，包裹内逐行回车
        assert_eq!(
            prepare_paste_bytes("a\r\nb\nc", true).as_deref(),
            Some(&b"\x1b[200~a\rb\rc\x1b[201~"[..])
        );
        // BEL / FF 等控制字符剔除，\t 保留
        assert_eq!(
            prepare_paste_bytes("a\x07b\x0c", false).as_deref(),
            Some(&b"ab"[..])
        );
        assert_eq!(
            prepare_paste_bytes("a\tb", false).as_deref(),
            Some(&b"a\tb"[..])
        );
        // 清洗后为空 = 无内容，不下发
        assert_eq!(prepare_paste_bytes("\x07\x1b", true), None);
    }

    /// IO-6：device 会话渲染源分层——UiEvent::SerialScreen 快照整体替换进
    /// SessionView.screen（不进 lines），后续快照整屏覆盖而非叠加。
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
        let rendered = view.screen.as_ref().expect("device 会话必须持有快照");
        assert_eq!(rendered.rows.len(), 1);
        assert_eq!(rendered.rows[0].text, "root:~# ls");
        assert_eq!(view.last_seq, 7);
        assert_eq!(view.lines.len(), 1, "快照行不得进注记列表");

        // 第二份快照：整体替换（rows 变两行），注记列表仍然不动
        view.absorb_serial_screen(screen(&["root:~# ls", "file-a"], false, false).into(), 8);
        assert_eq!(view.screen.as_ref().unwrap().rows.len(), 2);
        assert_eq!(view.screen.as_ref().unwrap().rows[1].text, "file-a");
        assert_eq!(view.last_seq, 8);
        assert_eq!(view.lines.len(), 1, "设备输出不得重复进注记列表");
    }

    /// IO-6：demo 会话没有仿真器——SerialScreen 快照被忽略（screen 保持
    /// None、seq 不推进），保持全量行渲染。
    #[test]
    fn demo_session_ignores_screen_snapshots_and_keeps_full_line_rendering() {
        let mut view =
            SessionView::new("dut-d".into(), "d1".into(), "演示 · COM1".into(), "demo", 0);
        view.absorb_serial_screen(screen(&["demo"], true, true).into(), 3);
        assert!(view.screen.is_none(), "demo 会话没有仿真网格");
        assert_eq!(view.last_seq, 0, "被忽略的快照不得推进序号");
    }

    /// IO-6：SerialChunk → lines 路由——device 会话只收注记行（Dim/Mark/Error
    /// 及 [serial] 信令），设备输出（Normal/Good/Prompt）由仿真器网格渲染、
    /// 不得重复进注记列表；"[serial] … @ N 已打开"信令行消费进链路状态后
    /// 同样不进列表；demo 会话保持全量行渲染。
    #[test]
    fn serial_chunk_routes_annotations_only_into_device_session_lines() {
        let device = SessionView::new("dut-a".into(), "s1".into(), "t".into(), "device", 0);
        // 设备输出三类：只走网格快照渲染
        for style in [ChunkStyle::Normal, ChunkStyle::Good, ChunkStyle::Prompt] {
            assert!(
                !device.should_queue_chunk_line(style, false),
                "{style:?} 是设备输出，不得进注记列表"
            );
        }
        // 注记三类：进注记列表
        for style in [ChunkStyle::Dim, ChunkStyle::Mark, ChunkStyle::Error] {
            assert!(
                device.should_queue_chunk_line(style, false),
                "{style:?} 是注记行"
            );
        }
        // 打开确认信令行（Dim）：消费进链路状态，不进列表
        assert!(!device.should_queue_chunk_line(ChunkStyle::Dim, true));

        // demo 会话：全量行渲染，任何样式都入列
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
    /// 分段标记行（`[mark] …`，日志分割）：终端用分隔样式渲染
    Mark,
}

impl LogLine {
    fn color(&self, p: &'static Palette) -> gpui_kit::Rgba {
        match self.kind {
            LineKind::Normal => p.term_fg,
            LineKind::Dim => p.muted,
            LineKind::Good => p.accent,
            LineKind::Prompt => p.prompt,
            LineKind::Error => p.red,
            LineKind::Mark => p.prompt,
        }
    }
}
/// 任务类型：结果队列按类型回写对应界面（审核发现 P2-1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Adb,
    Hdc,
}

/// 一条已完成的后台任务结果：携带任务类型 / 任务 ID / 发起 DUT。
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

/// 视图模式：首页（无会话时的欢迎页）/ 串口工作台
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Home,
    Serial,
}

pub(crate) enum FlashPresetTest {
    Running,
    Finished(duthub_core::DeviceResult<duthub_core::ProcessResult>),
}

/// 每 DUT 的 Agent 输入草稿（F13）：InputState 常驻 Workbench，按 DUT id 隔离；
/// `_subscription` 持有 Enter 提交订阅（发送 prompt），丢弃即退订。
pub(crate) struct AgentPromptEditor {
    pub(crate) state: Entity<InputState>,
    _subscription: gpui_kit::Subscription,
}

/// Agent 注册表内联编辑器：`id=None` 为新增，`Some` 为编辑已有条目。
pub(crate) struct AgentRegistryEdit {
    pub(crate) id: Option<String>,
}

/// W4c：外部 goose 路径保存的待落盘载荷——程序探测已移到后台执行器，
/// 校验结果回来（generation 匹配）后才写 agents.json。
#[derive(Clone)]
pub(crate) struct PendingGooseSave {
    pub path: String,
    pub generation: u64,
}

/// W4c：注册表保存的待落盘载荷。definition 已通过统一草稿门并填好除
/// `expected_sha256` 外的全部字段（哈希由后台探测回填）。
#[derive(Clone)]
pub(crate) struct PendingRegistrySave {
    pub definition: duthub_core::agent_config::AgentDefinition,
    /// 该条目是否为默认 goose（探测结果要同步到 `goose_probe`）。
    pub goose: bool,
    pub generation: u64,
}

/// Agent 宿主工作线程 → GUI 的事件出口（F9）。
///
/// HostEvents 由工作线程调用，不能直接触达 UI：失败进 AlertCenter（渲染层
/// 统一弹窗），一般退出只进 toast 队列，由 `agent_host_tick` 搬到
/// `pending_toasts`。
struct AgentHostEvents {
    alerts: Arc<Mutex<AlertCenter>>,
    toasts: Arc<Mutex<Vec<String>>>,
    /// 已上报失败的 DUT：随后的 on_exited 不重复 toast。
    failed: Mutex<std::collections::HashSet<String>>,
}

impl agent_host::HostEvents for AgentHostEvents {
    fn on_started(&self, dut: &str, _session: &str) {
        self.failed.lock().unwrap().remove(dut);
    }

    fn on_exited(&self, dut: &str, code: Option<i32>) {
        if self.failed.lock().unwrap().remove(dut) {
            // 失败已单独告警，不重复提示。
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

pub struct Workbench {
    pub(crate) agent_credential_listener: Option<String>,
    pub(crate) agent_token_input: Option<Entity<InputState>>,
    /// 内嵌 goose ACP 宿主（W3）：`--self-test` 下保持 None（不构造、不启动 goose）。
    pub(crate) agent_host: Option<agent_host::HostManager>,
    /// Agent 宿主工作线程的 toast 出口；渲染 tick 搬到 `pending_toasts`。
    agent_host_toasts: Arc<Mutex<Vec<String>>>,
    /// MCP HTTP 监听器句柄（F1/T16）：主端点 + 附加/每 DUT 端点；Drop 时关闭。
    pub(crate) mcp_runtime: crate::mcp_runtime::McpRuntimeHandles,
    /// 与 MCP 运行时共享的状态（HostManager 构造来源；T16 保存避免只活在线程闭包）。
    pub(crate) mcp_state: Arc<dyn duthub_mcp::state::McpState>,
    /// Live flash tool presets & ADB path mirrored into `RuntimeMcpState`.
    mcp_tools_config: Arc<Mutex<crate::mcp_runtime::RuntimeToolsConfig>>,
    /// Agent 接入上下文（认证/租约/审计）：每 DUT Agent 会话的运行时来源。
    pub(crate) agent_access: Option<Arc<duthub_mcp::auth_http::AccessContext>>,
    /// 每 DUT 独立 Agent 输入草稿（F13：切换 DUT 不串文本）。
    pub(crate) agent_prompts: HashMap<String, AgentPromptEditor>,
    /// Transcript 虚拟列表滚动句柄（按 DUT；uniform_list 专用）。
    pub(crate) agent_transcript_scroll: HashMap<String, gpui_kit::UniformListScrollHandle>,
    /// Transcript 已见的单调序号（`SessionView::total_entries()`；内存上限
    /// 2000 条截断后 `transcript.len()` 停涨，只有它能在 2000 条后继续贴底）。
    pub(crate) agent_transcript_seen: HashMap<String, u64>,
    /// 发送成功后待清空的草稿：dut_id → (已发送文本, 发送 token)。清空延到
    /// 渲染帧（`set_value` 需要 Window），且仅当输入仍等于已发送文本才清——
    /// 用户在等待帧里继续输入的草稿必须保留；跨 DUT 互不影响。
    agent_prompt_clear: HashMap<String, (String, u64)>,
    /// 「发送后清空」请求的单调 token（取最新语义，测试钉住值匹配规则）。
    agent_prompt_token: u64,
    /// `agents.json` 注册表（每 DUT 可选择的 ACP Agent；goose 为默认内嵌条目）。
    pub(crate) agents: duthub_core::agent_config::AgentsFile,
    /// 注册表加载错误（损坏/版本不符时 fail closed；UI 显示原因，不回退猜测）。
    pub(crate) agents_error: Option<String>,
    /// 每 DUT 的 Agent 选择（sidecar，`<work_dir>/agent-bindings.json`）。
    pub(crate) agent_bindings: duthub_core::agent_config::AgentBindingsFile,
    /// 注册表内联编辑器（name/program/args/envKeys）。
    pub(crate) agent_registry_edit: Option<AgentRegistryEdit>,
    /// W4b env 值编辑器：当前选中键（值写入/删除的目标；只在编辑既有条目时可用）。
    pub(crate) agent_env_selected: Option<String>,
    /// W4b env 值编辑器：选中键的 keyring 读取状态（None = 未选中）。
    pub(crate) agent_env_value_state: Option<duthub_desktop::agent_ui::EnvValueState>,
    /// 外部 goose 路径最近一次探测（版本/hash/未签名）；探测在后台执行器
    /// 上运行（W4c），UI 只读结果与 pending 状态。
    pub(crate) goose_probe: Option<agent_host::registry::ProgramProbe>,
    /// 外部 goose 探测仍在后台运行（UI 显示「探测中」，不再阻塞启动）。
    pub(crate) goose_probe_pending: bool,
    /// 后台探测/保存的代数（generation）：过期结果按代数丢弃，不覆盖新状态。
    goose_probe_generation: u64,
    /// 等待后台探测完成后落盘的外部 goose 路径保存（W4c）。
    pub(crate) pending_goose_save: Option<PendingGooseSave>,
    /// 等待后台程序探测完成后落盘的注册表保存（W4c）。
    pub(crate) pending_registry_save: Option<PendingRegistrySave>,
    /// env 值 keyring 读取的代数：切键/重读时递增，旧读取结果直接丢弃。
    agent_env_read_generation: u64,
    /// `--self-test`：Agent 宿主整体禁用（自检不解包/不探测外部 exe）。
    agent_self_test: bool,
    /// 每 DUT A2A Token 缓存（选中 DUT 时从 keyring 读取一次，避免每帧访问密钥库）。
    a2a_tokens: HashMap<String, Result<Option<String>, String>>,
    pub(crate) provider_models: crate::provider_models::Catalog,
    pub(crate) flash_preset_tests: HashMap<String, FlashPresetTest>,
    pub relay_press: Option<RelayPress>,
    pub relay_board_selected: usize,
    pub relay_board_name: Option<(
        String,
        gpui_kit::Entity<gpui_kit::component::input::InputState>,
    )>,
    /// 控制设备（继电器板）编辑草稿。字段与「连接管理」表单同语义：编辑只进
    /// 草稿（`Some` 即「有未保存的修改」），「保存设备」才写回板卡列表并落盘，
    /// 「取消修改」/切换板卡/离开页面即丢弃。id 必须与当前选中的已保存板卡
    /// 一致，否则视为过期草稿。
    pub(crate) relay_board_draft: Option<duthub_core::model::RelayBoard>,
    /// 视图模式：首页 / 串口工作台（关闭全部会话自动回首页）
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
    /// 主 MCP 监听器实际对外地址（局域网模式为探测到的局域网 IP）。
    mcp_bound_endpoint: Arc<OnceLock<SocketAddr>>,
    /// 后台任务结果队列（携带任务类型/任务 ID/DUT ID，按发起请求回写界面）
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
    /// 烧录测试结果按 DUT 归属：dut_id → 结果视图
    usb_discovery: [UsbDiscovery; 2],
    usb_debug_view: Option<bool>,

    pub settings: duthub_core::SettingsFile,
    /// Agent 接入认证运行时（凭据/Grant/auth 开关）：MCP HTTP 中间件、租约管理器
    /// 与设置页面板共用同一实例；保存后经 update() 整体热替换。
    pub access_runtime: Arc<duthub_core::AgentAccessRuntime>,
    pub duts: DutsState,
    /// 配置目录（bootstrap）：settings.json / duts.json 固定 exe 同级 .duthub/。
    data_dir: PathBuf,
    /// 工作目录（解析生效值）：logs / captures / exports / runtime 输出根。
    /// 解析规则见 `duthub_core::store::resolve_work_dir_in`（settings > --workdir > 默认）。
    pub work_dir: PathBuf,
    pub artifact_root: PathBuf,
    pub(crate) artifact_quota: Arc<duthub_core::dut_quota::DutQuota>,
    pub(crate) boot_learning_dut: Option<String>,
    boot_similarity_drafts: HashMap<String, String>,

    pub sessions: Vec<SessionView>,
    pub active_session: usize,
    pub serial_ports: Vec<SerialPortInfo>,

    /// MCP 快照（每帧刷新，mcp_runtime 落盘）
    snapshot_ref: Arc<Mutex<duthub_mcp::snapshot::StateSnapshot>>,
    /// 心跳引擎（需求⑦）：desktop 定时器驱动；Agent 经 heartbeat.set 改写命令。
    pub heartbeat: Arc<duthub_device::heartbeat::HeartbeatService>,
    heartbeat_drafts: HashMap<String, duthub_core::heartbeat::HeartbeatConfig>,
    /// 共用设备操作入口（控制动作授权/绑定/能力校验收口；DUT 配置每帧注册）
    pub control: Arc<duthub_device::control::ControlService>,
    /// 审批令牌管理器（与 MCP 运行时共享；GUI 人工操作经它签发令牌走同一入口）
    /// MCP 控制面（Power/Reset/PowerKey 触发走这里；后端接线见 M3.3）
    pub overlay: Option<ConfigPage>,
    connection_open: bool,
    connection_dut_scope: Option<String>,
    pub(crate) expanded_mcp_config: Option<String>,
    connection_pane_width: Option<Pixels>,
    tools_pane_width: Option<Pixels>,
    pub menu_open: bool,
    pub side_panel: crate::side_panel::SidePanel,
    pub agent: AgentState,
    #[allow(dead_code)] // M5.4 LLM 任务调用时读取
    agent_model_provider: String,
    #[allow(dead_code)] // M5.4 LLM 任务调用时读取
    agent_model_name: String,
    pub find_open: bool,
    pub find_query: String,
    pub rec_on: bool,
    pub rec_buffer: Vec<String>,
    /// 待推送的系统通知（kit Notification，渲染时经 Root 层弹出）
    pub pending_toasts: Vec<String>,
    /// 异常弹窗提醒中心（需求⑤）：全局抑制列表来自 settings.alerts，每 DUT
    /// 开关来自 duts.json alertsByDut（缺省开启）；与 MCP HTTP 线程共用同一
    /// 实例（MCP 侧控制动作失败经此回 GUI 弹窗）。
    pub alerts: Arc<Mutex<AlertCenter>>,
    /// 「需要人工干预」Dialog 是否正打开（自愈：外部关闭后下一帧复位）
    alert_dialog_open: bool,
    pub modal: Option<ModalState>,
    /// 终端右键菜单：Some((x, y)) = 菜单显示位置（窗口坐标 px）
    pub terminal_ctx: Option<(f32, f32)>,
    pub open_dd: Option<String>,
    /// 每 DUT A2A 运行时（W4a）：状态查询、Token 热轮换、停用/删除联动
    /// 与退出关停都收口在此；`None` = `--self-test`（纯净自检不启动任何
    /// 监听器/运行时，状态查询一律降级为 Disabled）。
    pub a2a: Option<crate::a2a_runtime::A2aRuntime>,
    pub cfg_search: String,
    // 数字字段草稿（可清空重输；合法时同步模型）
    drafts: std::collections::HashMap<In, String>,

    input: String,
    input_focus: FocusHandle,
    terminal_focus_pending: bool,
    text_editors: HashMap<In, BoundEditor<InputState>>,
    textareas: HashMap<In, BoundEditor<TextareaState>>,
    /// 终端视口滚动句柄：新输出到达时自动跟随到底部（原型 nearBottom<65 语义）
    /// 终端虚拟列表滚动句柄（uniform_list 专用；仅可见行构建元素）。
    terminal_scroll: gpui_kit::UniformListScrollHandle,
    /// 串口设置页：当前编辑的高亮规则下标（规则列表行点击选择）
    pub(crate) serial_rule_selected: usize,
    /// Cached compiled prompt highlight rules (`Arc` avoids cloning Regex + Strings every frame).
    highlight_cache: std::cell::RefCell<HashMap<String, Option<regex::Regex>>>,
    highlight_rules_cache: std::cell::RefCell<HighlightRulesCache>,
    /// Cached host MXC OS sandbox status (10s TTL) so render frames never run OS probes at 60 FPS.
    sandbox_status_cache:
        std::cell::RefCell<Option<(std::time::Instant, duthub_sandbox::SandboxStatus)>>,
    /// Shared Tokio runtime owned for the lifetime of the desktop process.
    _shared_runtime: Arc<tokio::runtime::Runtime>,
    /// 最近一次下发给 real 会话的网格尺寸（None = 未发送；会话新建后重发）
    term_grid: Option<(usize, usize)>,
    terminal_zoom_size: Option<f32>,
    terminal_font_picker: Option<FontPicker>,

    pub frames: Arc<AtomicUsize>,
    pub core_lines: Arc<AtomicUsize>,
}

/// 启动期上下文（main.rs → Workbench 一次性注入）。
///
/// settings.json 必须只加载一次（`load_settings` 对损坏文件有改名备份副作用，
/// 读两遍会丢损坏告警）；工作目录在 main 统一解析后与串口运行时共享，
/// 保证 logs 与 captures/exports/runtime 同根。
pub struct StartupContext {
    /// 配置目录（bootstrap）：settings.json / duts.json 固定 exe 同级 .duthub/。
    pub data_dir: PathBuf,
    /// 工作目录（解析生效值）：logs / captures / exports / runtime 输出根。
    pub work_dir: PathBuf,
    pub artifact_root: PathBuf,
    pub artifact_quota: Arc<duthub_core::dut_quota::DutQuota>,
    /// 已加载的设置（main.rs 解析结果，含启动失败回退默认值的情况）。
    pub settings: duthub_core::SettingsFile,
    /// 启动提示（settings 损坏 / 工作目录回退），随首个 toast 弹出。
    pub notice: Option<String>,
    /// `--self-test`：不构造 Agent 宿主（不安装/探测 goose），其余 UI 照常渲染。
    pub self_test: bool,
    /// Shared Tokio runtime from `main.rs` (used by DeviceService, ControlService, A2aRuntime).
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
        // W4a 固定关停顺序：停 Agent 会话 → 停 A2A 监听器（释放端口）→
        // 关 MCP HTTP 监听器与快照线程（F1/T16、F12）。
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
    transition: LifecycleTransition,
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
            let _ = weak.update(cx, |this, cx| this.release_relay_for_close(cx));
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
            let _ = weak.update(cx, |this, cx| this.finish_hide_to_tray(cx));
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
            let _ = weak.update(cx, |this, cx| this.show_from_tray(cx));
        }
        LifecycleAction::RecoverFromIconLoss => {
            let _ = weak.update(cx, |this, cx| this.recover_from_icon_loss(cx));
        }
        LifecycleAction::PresentAlert {
            presentation,
            count,
        } => {
            let _ = weak.update(cx, |this, _| {
                this.present_hidden_alert(presentation, count);
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

/// W4a：DUT 删除五步清理序的 Workbench 实现。
/// 顺序契约（strip → persist → stop_and_join → delete token → delete dir）
/// 由 `apps/desktop/tests/agent_ui_logic.rs` 以 mock ops 钉住。
impl duthub_desktop::agent_ui::DutCleanupOps for Workbench {
    fn strip_mcp(&mut self, dut_id: &str) -> bool {
        // 未配置 DUT 专用端点 = 无需 strip（成功）；strip 失败（含同步
        // settings 落盘失败）返回 false 让整个删除中止。
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
        // 先落盘删除再动运行时（AGENTS #37）：磁盘版本移除目标记录，避免
        // 停止失败时重启把 DUT 复活。
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
        // 每 DUT Agent 选择随 DUT 删除；默认 goose 不写冗余条目。失败只
        // 告警不中止（旧绑定会随下次保存自然收敛）。
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
        // Agent 会话：非阻塞停止（token 吊销/关 stdin 立即执行，等待在宿主
        // 线程）——与关 tab/会话面板「停止」走同一入口。
        self.stop_agent_session(dut_id);
        // A2A 监听器立即停止并释放端口。
        if let Some(a2a) = self.a2a.as_ref() {
            a2a.stop_dut(dut_id);
        }
        // 串口会话级联关闭（demo 会话同样注册在 DeviceService）；重复关闭
        // 返回 NotFound 已忽略。
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
                &[("error", &error.to_string())],
            ));
        }
        // 同步告警弹窗（AlertCenter 与 MCP 线程共享）：清理未完成必须可见。
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
        auto_open: Option<String>,
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
        // Agent 注册表：缺失生成默认 goose；损坏时 fail closed（保留备份）并显示
        // 原因。每 DUT 选择存 sidecar，不修改 duts.json 的既有 schema。
        // W4c `--self-test`：只持有内存默认值，绝不读/写 agents.json 与
        // agent-bindings sidecar（load_agents 在缺失时会落盘默认注册表）。
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
        // W4c：外部 goose 探测不再阻塞启动（文件哈希 + `--version` 子进程
        // 全部在后台执行器上跑）；构造完成后由 spawn 发起，self-test 不探测。
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

        // 默认不打开任何会话：空态引导用户从 ＋ 新建（选 DUT）。
        // --open-port COM31：启动即打开真实串口会话（快速调试入口）。
        let mut sessions: Vec<SessionView> = Vec::new();
        if let Some(port) = &auto_open {
            let dut_id = Self::dut_id_for_port(&duts, port);
            let matched = duts.file.duts.iter().position(|dut| dut.id == dut_id);
            let title = matched.map_or_else(
                || port.clone(),
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
            .map(|p| provider_setting(p.protocol).to_owned())
            .unwrap_or_default();
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
        // 共用设备操作入口：审批令牌管理器与 MCP 运行时共享同一实例，
        // 控制动作（Power/Reset/PowerKey）的授权/绑定/能力校验收口在 ControlService。
        // CH340 驱动在 real-hardware 下直接构造注入（审核建议：单一实现不加选择层）；
        // demo 模式传 None，入口报 Unsupported 且不消费令牌。
        let approval = Arc::new(duthub_core::ApprovalManager::new());
        #[cfg(feature = "real-hardware")]
        let control_backend: Option<Arc<dyn duthub_core::ControlBackend>> =
            Some(Arc::new(duthub_control::Ch340RelayDriver::new()));
        #[cfg(not(feature = "real-hardware"))]
        let control_backend: Option<Arc<dyn duthub_core::ControlBackend>> = None;
        // Agent 接入（MCP 层，方案 §6）：认证运行时（凭据/Grant/auth 开关）+ 接入
        // 上下文（与 DeviceService 写入通道共享同一租约管理器实例——复核口径一致）。
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
        // 通断前切段，动作结果写入已打开的段。
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
            // 透传落盘成败：分段失败时控制入口拦截派发（LOG-5）
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
        // 心跳引擎（需求⑦）：持久化钩子把 Agent 的 heartbeat.set 写回 duts.json
        //（bootstrap 配置目录；load → 改该 DUT 的 heartbeat 段 → 原子保存）。
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
        // 异常弹窗提醒中心（需求⑤）：全局抑制列表来自 settings.alerts，
        // 每 DUT 开关随后从 duts.json alertsByDut 镜像（缺省开启）；与 MCP
        // HTTP 线程共用同一实例（MCP 侧控制动作失败经此回 GUI 弹窗）。
        let alerts = Arc::new(std::sync::Mutex::new(AlertCenter::from_settings(
            &settings.alerts.suppressed_sources,
        )));
        alerts
            .lock()
            .unwrap()
            .set_dut_settings(&duts.file.alerts_by_dut);
        // MCP 端口/局域网开关（settings 被 wb 结构移走前取值；重启生效）
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
        // A2A 运行时（W4a）：为已启用 DUT 绑定回环监听器；MCP 端口预登记为
        // External。启动失败只降级为 per-DUT 状态（Failed/PortConflict），
        // 绝不 panic——Workbench 必须能在 A2A 不可用时照常启动。
        // W4c `--self-test`：纯净自检不构造运行时（无 tokio 驱动、无监听）。
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
            device: device.clone(),
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
            side_panel: Default::default(),
            agent,
            find_open: false,
            find_query: String::new(),
            rec_on: false,
            rec_buffer: Vec::new(),
            pending_toasts: toast.into_iter().collect(),
            alerts,
            alert_dialog_open: false,
            flash_preset_tests: HashMap::new(),
            agent_credential_listener: None,
            agent_token_input: None,
            a2a_tokens: HashMap::new(),
            a2a,
            provider_models: Default::default(),
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
            duthub_desktop::tool_ui::sweep_staging_leftovers(&tools_root);
        }
        // 启动重试上次删除遗留的清理任务（W4a pending-cleanup）：成功的
        // 条目从清单移除，仍失败的 toast 提醒（下次启动继续重试）。
        // W4c `--self-test`：纯净自检不触碰 agent/ 目录（无输入即无重试）。
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
            // W4c：外部 goose 路径探测后台化——启动不再被文件哈希 +
            // `--version` 子进程探测阻塞；结果按代数回填（过期丢弃）。
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
        // 串口事件到达即唤醒 UI，批量吸收已经排队的事件。
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
                // 持续输出时让其他 UI 任务有机会处理输入。
                tokio::task::yield_now().await;
            }
        })
        .detach();

        // 周期任务仅在有变化时重绘。
        // 空闲时不渲染 —— hitbox 稳定，鼠标按下/抬起不被重渲染打断（否则按钮点击会丢）。
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
                let changed = this
                    .update(cx, |view, cx| view.tick_frame_pump(cx))
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

        // MCP 运行时：默认 127.0.0.1:{agentAccess.httpPort}；局域网模式绑 0.0.0.0
        // （同一端口同时接受回环与局域网地址）+ 快照落盘（后台线程）。
        // 句柄保存在 Workbench：退出/关闭时先 shutdown 再 join（F1/T16）。
        // W4c `--self-test`：纯净自检不启动任何 HTTP 监听（netstat 无 8797/8790）。
        if !self_test {
            wb.ensure_dut_mcp_endpoints();
            wb.mcp_runtime = crate::mcp_runtime::spawn_mcp_runtime(
                mcp_state,
                Some(runtime_state as Arc<dyn duthub_mcp::notify::UserNotifier>),
                access_ctx,
                mcp_port,
                mcp_lan_access,
                wb.settings.agent_access.listeners.clone(),
                snapshot_ref.clone(),
                wb_work_dir,
                mcp_bound_endpoint,
            );
        }
        wb
    }

    #[allow(dead_code)]
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
// ＋ 表单 → DUT 合并（审核发现 P1-1：编辑以原配置为基底，只更新修改的字段）
// ---------------------------------------------------------------------------

/// 表单直接呈现、总是随保存更新的字段。
#[cfg(test)]
pub(crate) struct DutFormFields {
    pub name: String,
    pub port: String,
    pub baud: u32,
    /// Some = 更新 ADB 序列号（表单留空不覆盖原绑定）
    pub adb_serial: Option<String>,
    pub flash: FlashConfig,
}

/// 继电器绑定的表单变更：
/// - `Unchanged`：用户未动模式芯片 —— 同类型仅更新节点，SNMP PDU/通道映射原样保留；
/// - `ReplaceUsb` / `ReplaceDisabled`：用户在表单内明确切换类型才替换绑定。
#[cfg(test)]
pub(crate) enum RelayFormChange {
    Unchanged { node: String },
    ReplaceUsb { node: String },
    ReplaceDisabled,
}

/// 编辑模式合并。media、未变更的 relay（含 PDU 插座绑定/继电器通道）、
/// 其他 MCP 传输形态（stdio）一律保留。
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
                channels: Default::default(),
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

    /// portable 归档名守卫：保留名形状（COM 口名直存）必须兜底，否则
    /// `open_dut_for_write` 逐条拒绝、写线程静默吞错 = 全部日志丢失。
    #[test]
    fn archive_safe_name_guards_windows_reserved_shapes() {
        // Windows 保留名是 COM1–9 / LPT1–9（单数字）；COM10 并不保留。
        // 兜底优先用 dut_id 本身（程序 slug，通常已带 dut- 前缀且合法）
        assert_eq!(archive_safe_name("COM8", "dut-com8"), "dut-com8");
        assert_eq!(archive_safe_name("com1", "dut-com1"), "dut-com1");
        assert_eq!(archive_safe_name("lpt2", "dut-lpt2"), "dut-lpt2");
        assert_eq!(archive_safe_name("com10", "dut-com10"), "com10");
        // 合法名原样保留（改名/保存过的 DUT 维持既有归档目录）
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
            adb: Default::default(),
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
            a2a: Default::default(),
            flash: FlashConfig {
                preset_id: Some("preset-upgrade_tool".into()),
                executable: "upgrade_tool".into(),
                test_args: vec!["--help".into()],
            },
            media: Default::default(),
            heartbeat: Default::default(),
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
            panic!("USB 继电器绑定必须保留");
        };
        assert_eq!(node, "COM7");
        assert_eq!(channels.power, Some(4));
        assert_eq!(channels.reset, Some(1));
        assert_eq!(channels.power_key, Some(5));
        // media 未在表单中呈现 → 保留
        assert_eq!(dut.media, Default::default());
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
            // 表单无法呈现 PDU：未动模式芯片时按 Unchanged 处理
            RelayFormChange::Unchanged {
                node: String::new(),
            },
            None,
        );
        let duthub_core::model::RelayConfig::SnmpPdu { snmp, outlets } = &dut.relay else {
            panic!("SNMP PDU 绑定必须保留");
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
            panic!("明确切换为 USB 继电器必须替换绑定");
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
            panic!("绑定类型不应改变");
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
        // 逐字段写入（编辑），再读回（回填）——两个源各自独立。
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
                .expect("DUT 字段必须可编辑");
            *dest = value.into();
            assert_eq!(field.value(&settings, Some(&dut)), value, "{field:?}");
        }
        // 写入落在 dut.media 上，等待 save_duts() 持久化。
        assert_eq!(dut.media.hdmi.video, "UGREEN 35866");
        assert!(dut.media.hdmi.has_video());
        assert_eq!(dut.media.hdmi.frame_rate, "60");
        assert_eq!(dut.media.hdmi.port, "8554");
        assert_eq!(dut.media.camera.video, "Integrated Camera");
        assert_eq!(dut.media.camera.audio, "", "音频留空 = 无声采集");
        // 工具级设置（ffmpeg/ffplay/监听 IP）走同一编辑通道，但不挂到 DUT 上。
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

/// portable 归档的 DUT 文件夹名守卫：`name_component` 拒绝的名字（Windows
/// 保留名形状如 "COM8"，会映射成 `_COM8` 与名字不一致）换用 dut_id（程序
/// 生成的 slug，通常已带 `dut-` 前缀且合法），否则 `open_dut_for_write`
/// 逐条拒绝、写盘错误被写线程静默吞掉 = 全部日志丢失（验收发现的既有
/// bug）。合法名原样返回（改名的历史 DUT 保持既有归档目录）。
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
        adb: Default::default(),
        relay: duthub_core::model::RelayConfig::Disabled,
        mcp: duthub_core::model::McpConfig::StreamableHttp {
            url: "http://127.0.0.1:8787/mcp".into(),
        },
        a2a: Default::default(),
        flash: FlashConfig::default(),
        media: Default::default(),
        heartbeat: Default::default(),
    }
}

// 后台任务统一入口：gpui background executor 不可从 &mut Self 直接拿时退化为线程
fn cx_spawn_job(f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(f);
}

/// 工具安装失败的本地化标题（lib 层枚举 → bin 层 i18n，坑 #66 模式）；
/// 技术细节字符串由内核错误原文随行展示，不做翻译。
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
