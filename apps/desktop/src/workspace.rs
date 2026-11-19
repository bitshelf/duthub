//! 工作台外壳 —— 1:1 移植原型 `#page-serial` + 全局件：
//! tabbar（menu/spark/divider/tabs/add/工具栏）→ find 条 → 终端（含 edit-row）→ 命令 dock
//! → workspace-split（终端 | Agent 面板）→ zed-statusbar；浮层：配置壳 / 菜单 / 弹窗 / Toast。
//!
//! 输入体系：`In` 枚举标识设置字段，gpui-kit 组件负责文本编辑；
//! 下拉：`Dd`/select id → `open_dd`；弹窗：`ModalState`；通知：kit Notification（Root 层）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use duthub_adb::AdbClient;
use duthub_core::ipc::{ChunkStyle, UiEvent};
use duthub_core::model::{FlashConfig, QuickCommand};
use duthub_core::store;
use duthub_core::{AdbDevice, DutAction, DutGrant, SerialPortInfo};
use duthub_device::{DeviceService, broadcast};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AppContext, Axis, Context, Div, DragMoveEvent, Empty, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Window, WindowControlArea,
    div, img, px,
};

use crate::agent::{AgentState, RunStatus};
use crate::alerts::{AlertCenter, AlertItem, AlertSource};
use crate::components::{icon_btn, input_box, typed_char};
use crate::data::DutsState;
use crate::i18n::Lang;
use crate::icons::{icon, kit_icon};
use crate::pages::{render_connection, render_flash_tools, render_settings, render_usb_debug};
use crate::theme::Palette;
use crate::{agent, i18n, theme};
use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{HandleEdge, ResizeHandle, resize_handle};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};

type FontSelectState = SelectState<SearchableVec<String>>;

struct FontPicker {
    lang: Lang,
    available: Vec<String>,
    state: Entity<FontSelectState>,
    _subscription: gpui_kit::Subscription,
}

const DEFAULT_TERMINAL_FONT_SIZE: f32 = 12.5;
pub(crate) const MIN_TERMINAL_FONT_SIZE: f32 = 7.5;
pub(crate) const MAX_TERMINAL_FONT_SIZE: f32 = 48.0;

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
        .unwrap_or_else(|| {
            DEFAULT_TERMINAL_FONT_SIZE
                * f32::from(settings.font_percent.unwrap_or(100).clamp(60, 240))
                / 100.0
        })
        .clamp(MIN_TERMINAL_FONT_SIZE, MAX_TERMINAL_FONT_SIZE)
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
    Port,
    Baud,
    /// 会话状态条的波特率热切换输入（按活动会话独立草稿）
    SessionBaud,
    AdbSerial,
    DutName,
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
    Regex,
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
    Media(crate::media_page::MediaField),
    A2aName,
    A2aPort,
    CmdName,
    CmdValue,
    NewPresetName,
    NewPresetExe,
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
            In::Port => "port",
            In::Baud => "baud",
            In::SessionBaud => "session-baud",
            In::AdbSerial => "adb-serial",
            In::DutName => "dut-name",
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
            In::Regex => "regex",
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
            In::Media(field) => field.key(),
            In::A2aName => "a2a-name",
            In::A2aPort => "a2a-port",
            In::CmdName => "cmd-name",
            In::CmdValue => "cmd-value",
            In::NewPresetName => "new-preset-name",
            In::NewPresetExe => "new-preset-exe",
            In::PeerName => "peer-name",
            In::PeerUrl => "peer-url",
            In::RenameName => "rename-name",
        }
    }

    fn multiline(self) -> bool {
        matches!(self, In::McpArgs | In::McpEnv | In::Regex)
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
    pub(crate) fn key(self) -> &'static str {
        match self {
            Dd::Port => "dd-port",
            Dd::RelayBoardPort => "dd-relay-board-port",
            Dd::Baud => "dd-baud",
            Dd::AdbSerial => "dd-adb",
            Dd::CaptureMode(field) => field.key(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPage {
    AgentModel,
    Flash,
    Adb,
    Relay,
    Media,
    Settings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // RenameDut 供后续右键菜单
pub enum ModalKind {
    RenameDut(usize),
    RemoveDut(String),
    NewSession,
    NewCommand,
    NewPreset,
    NewPeer,
}

#[derive(Clone)]
pub struct ModalState {
    pub kind: ModalKind,
    pub draft: String,
    pub draft2: String,
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
    /// 设备输出一律走这里的网格行。
    pub screen: Option<duthub_core::ipc::ScreenSnapshot>,
    crash_line_tail: String,
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
            crash_line_tail: String::new(),
        }
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
    fn terminal_font_size_preserves_legacy_zoom_and_prefers_explicit_size() {
        let mut settings: TerminalSettings =
            serde_json::from_str(r#"{"fontPercent":120}"#).unwrap();
        assert_eq!(configured_terminal_font_size(&settings), 15.);
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

#[derive(Clone)]
pub struct LogLine {
    pub text: SharedString,
    pub kind: LineKind,
}

#[derive(Clone, Copy, PartialEq)]
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

pub struct Workbench {
    pub(crate) agent_credential_listener: Option<String>,
    pub(crate) agent_token_input: Option<Entity<InputState>>,
    pub(crate) provider_models: crate::provider_models::Catalog,
    pub(crate) flash_preset_tests: HashMap<String, FlashPresetTest>,
    pub relay_press: Option<RelayPress>,
    pub relay_board_selected: usize,
    pub relay_board_name: Option<(
        String,
        gpui_kit::Entity<gpui_kit::component::input::InputState>,
    )>,
    /// 视图模式：首页 / 串口工作台（关闭全部会话自动回首页）
    pub view: ViewMode,
    pub lang: Lang,
    pub theme_mode: theme::ThemeMode,
    pub p: &'static Palette,

    pub device: Arc<DeviceService>,
    pub(crate) media: crate::media_page::MediaView,
    adb_client: Arc<AdbClient>,
    mcp_bound_port: Arc<OnceLock<u16>>,
    /// 后台任务结果队列（携带任务类型/任务 ID/DUT ID，按发起请求回写界面）
    job_queue: JobQueue,
    next_job: u64,
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
    /// 异常弹窗提醒中心（需求⑤）：GUI 与 MCP HTTP 线程共用；enabled/抑制列表
    /// 镜像 settings.alerts，弹窗调度在 render_root（无活动 Dialog 时出队弹）。
    pub alerts: Arc<Mutex<AlertCenter>>,
    /// 「需要人工干预」Dialog 是否正打开（自愈：外部关闭后下一帧复位）
    alert_dialog_open: bool,
    pub modal: Option<ModalState>,
    /// 终端右键菜单：Some((x, y)) = 菜单显示位置（窗口坐标 px）
    pub terminal_ctx: Option<(f32, f32)>,
    pub open_dd: Option<String>,
    pub a2a_running: bool,
    pub cfg_search: String,
    // 数字字段草稿（可清空重输；合法时同步模型）
    drafts: std::collections::HashMap<In, String>,

    input: String,
    input_focus: FocusHandle,
    terminal_focus_pending: bool,
    focus_map: HashMap<In, FocusHandle>,
    text_editors: HashMap<In, BoundEditor<InputState>>,
    textareas: HashMap<In, BoundEditor<TextareaState>>,
    /// 终端视口滚动句柄：新输出到达时自动跟随到底部（原型 nearBottom<65 语义）
    terminal_scroll: gpui_kit::ScrollHandle,
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
        } = startup;
        let (mut duts, duts_err) = DutsState::load(&data_dir);
        let mut toast = startup_notice.or(duts_err);
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
            let name = matched.map_or(port.as_str(), |index| duts.file.duts[index].name.as_str());
            match device.open_named_session(
                &dut_id,
                name,
                port,
                auto_baud,
                duthub_device::SessionKind::Real,
            ) {
                Ok(session_id) => {
                    sessions.push(SessionView::new(
                        dut_id, session_id, title, "device", auto_baud,
                    ));
                }
                Err(err) => {
                    toast = Some(format!("打开 {port} 失败: {err}"));
                }
            }
        }
        let core_lines = core_lines_in;

        let agent_model_provider = settings.agent.provider.clone();
        let agent_model_name = settings.agent.slow.clone();
        let wb_work_dir = work_dir.clone();
        let frames = frames_in;
        let mut agent = AgentState::new();
        // Composer 焦点句柄：AgentState 无 cx，只能由 Workbench 构造时注入
        // （否则 composer 输入框永远无法聚焦、无法输入）。
        agent.prompt_focus = Some(cx.focus_handle());
        // 模型名与设置打通：默认回填 settings.agent.slow（探针/任务调用同源）
        if agent.model_name.is_empty() && !agent_model_name.is_empty() {
            agent.model_name = agent_model_name.clone();
        }
        if duthub_core::Provider::from_setting(&agent_model_provider).is_some() {
            agent.model_provider.clone_from(&agent_model_provider);
        }
        let snapshot_ref = Arc::new(std::sync::Mutex::new(
            duthub_mcp::snapshot::StateSnapshot::default(),
        ));
        let initial_view = if sessions.is_empty() {
            ViewMode::Home
        } else {
            ViewMode::Serial
        };
        let media = crate::media_page::MediaView::new(artifact_root.clone());
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
            let mut svc =
                duthub_device::control::ControlService::new(control_backend, approval.clone());
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
            let _ = power_device.log_control_boundary(dut_id, label, true);
        }));
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
                            .ok_or_else(|| "duts.json 不存在".to_string())?;
                        let Some(dut) = file.duts.iter_mut().find(|d| d.id == dut_id) else {
                            return Err(format!("duts.json 无 DUT {dut_id}"));
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
        // 异常弹窗提醒中心（需求⑤）：enabled/抑制列表来自 settings.alerts，
        // 与 MCP HTTP 线程共用同一实例（MCP 侧控制动作失败经此回 GUI 弹窗）。
        let alerts = Arc::new(std::sync::Mutex::new(AlertCenter::from_settings(
            settings.alerts.enabled,
            &settings.alerts.suppressed_sources,
        )));
        alerts
            .lock()
            .unwrap()
            .set_dut_settings(&duts.file.alerts_by_dut);
        // MCP 端口（settings 被 wb 结构移走前取值；偏差 8 可配置，重启生效）
        let mcp_port = settings.agent_access.http_port;
        let mcp_listeners = settings.agent_access.listeners.clone();
        let mcp_bound_port = Arc::new(OnceLock::new());
        let mcp_state: Arc<dyn duthub_mcp::state::McpState> =
            Arc::new(crate::mcp_runtime::RuntimeMcpState::new(
                device.clone(),
                snapshot_ref.clone(),
                control.clone(),
                approval,
                heartbeat.clone(),
                media.urls.clone(),
                alerts.clone(),
                wb_work_dir.clone(),
                access_ctx.clone(),
            ));
        let wb = Self {
            relay_board_selected: 0,
            relay_press: None,
            relay_board_name: None,
            view: initial_view,
            lang,
            theme_mode,
            p: &theme::DARK,
            device: device.clone(),
            adb_client,
            mcp_bound_port: mcp_bound_port.clone(),
            agent_model_provider,
            agent_model_name,
            job_queue: Arc::new(Mutex::new(Vec::new())),
            next_job: 1,
            usb_discovery: Default::default(),
            usb_debug_view: None,
            settings,
            access_runtime,
            duts,
            data_dir,
            work_dir,
            artifact_root,
            artifact_quota,
            sessions,
            active_session: 0,
            serial_ports,
            overlay: None,
            menu_open: false,
            connection_open: false,
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
            provider_models: Default::default(),
            terminal_ctx: None,
            modal: None,
            open_dd: None,
            a2a_running: false,
            cfg_search: String::new(),
            drafts: std::collections::HashMap::new(),
            input: String::new(),
            input_focus: cx.focus_handle(),
            terminal_focus_pending: initial_view == ViewMode::Serial,
            focus_map: HashMap::new(),
            text_editors: HashMap::new(),
            textareas: HashMap::new(),
            terminal_scroll: gpui_kit::ScrollHandle::new(),
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
        };
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
                    .update(cx, |view, _cx| {
                        view.media.tick(&view.duts)
                            | view.poll_relay_press()
                            | view.tick_side_panel()
                            | view.drain_model_test()
                            | view.drain_jobs()
                            | view.heartbeat_tick()
                            | view.agent_tick()
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

        // MCP 运行时：127.0.0.1:8797/mcp + 快照落盘（后台线程）
        // MCP 运行时：127.0.0.1:{agentAccess.httpPort 或默认 8797}/mcp + 快照落盘（后台线程）
        crate::mcp_runtime::spawn_mcp_runtime(
            mcp_state,
            access_ctx.clone(),
            mcp_port,
            mcp_listeners,
            snapshot_ref.clone(),
            wb_work_dir,
            mcp_bound_port,
        );
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

    #[allow(dead_code)]
    pub fn focus(&mut self, id: In, cx: &mut Context<Self>) -> FocusHandle {
        self.focus_map
            .entry(id)
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    pub fn notify_self(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    pub fn notify_toast(&mut self, message: impl Into<String>) {
        self.pending_toasts.push(message.into());
    }

    // ------------------------------------------------------------------
    // 设置持久化
    // ------------------------------------------------------------------

    pub fn persist_settings(&mut self) {
        if let Err(err) = store::save_settings_atomic(&self.data_dir, &self.settings) {
            self.notify_toast(format!("保存 settings 失败: {err}"));
        }
    }

    pub(crate) fn save_dut_storage_limit(&mut self) {
        let input = self.input_value(In::DutStorageGib);
        let Ok(limit) = input.trim().parse::<std::num::NonZeroU32>() else {
            self.notify_toast(crate::i18n::label("invalidDutStorageLimit"));
            return;
        };
        let mut settings = self.settings.clone();
        settings.dut_storage_gib = Some(limit.get());
        if let Err(error) = store::save_settings_atomic(&self.data_dir, &settings) {
            self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            return;
        }
        self.settings = settings;
        self.drafts.remove(&In::DutStorageGib);
        self.artifact_quota
            .set_limit(u64::from(limit.get()) * duthub_core::dut_quota::GIB);
        self.notify_toast(crate::i18n::label("dutStorageLimitSaved"));
    }

    pub(crate) fn save_model_providers(
        &mut self,
        providers: Vec<duthub_core::model::ModelProvider>,
    ) -> Result<(), String> {
        let mut candidate = self.settings.clone();
        candidate.agent.providers = providers;
        store::save_settings_atomic(&self.data_dir, &candidate).map_err(|err| err.to_string())?;
        self.settings = candidate;
        Ok(())
    }

    pub(crate) fn terminal_font_size(&self) -> f32 {
        configured_terminal_font_size(&self.settings.terminal)
    }

    pub(crate) fn set_terminal_font_size(&mut self, size: Option<f32>) {
        self.terminal_zoom_size = None;
        self.settings.terminal.font_size = size.filter(|size| *size != DEFAULT_TERMINAL_FONT_SIZE);
        self.settings.terminal.font_percent = None;
        self.persist_settings();
    }

    pub(crate) fn terminal_font(&self, window: &Window) -> (SharedString, Pixels, Pixels) {
        let family = self
            .settings
            .terminal
            .font_family
            .as_deref()
            .filter(|family| !family.trim().is_empty())
            .unwrap_or(theme::MONO_FONT);
        let size = self
            .terminal_zoom_size
            .unwrap_or_else(|| self.terminal_font_size());
        let text = window.text_system();
        let font_id = text.resolve_font(&gpui_kit::font(family.to_owned()));
        let cell_width = text
            .ch_advance(font_id, px(size))
            .unwrap_or(px(size * 0.6))
            .max(px(1.));
        (SharedString::from(family.to_owned()), px(size), cell_width)
    }

    pub(crate) fn terminal_font_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let default = i18n::t(self.lang, "terminalFontDefault").to_owned();
        if self
            .terminal_font_picker
            .as_ref()
            .is_none_or(|picker| picker.lang != self.lang)
        {
            let text = window.text_system();
            let mut fonts = text.all_font_names();
            fonts.retain(|family| {
                let id = text.resolve_font(&gpui_kit::font(family.clone()));
                let advances = ['i', 'M', '0', ' '].map(|ch| text.advance(id, px(20.), ch).ok());
                let Some(first) = advances[0] else {
                    return false;
                };
                first.width > px(0.)
                    && advances.into_iter().all(|advance| {
                        advance.is_some_and(|advance| {
                            (f32::from(advance.width - first.width)).abs() < 0.05
                        })
                    })
            });
            fonts.insert(0, default.clone());
            let items = SearchableVec::new(fonts.clone());
            let state = cx.new(|cx| SelectState::new(items, None, window, cx).searchable(true));
            let default_label = default.clone();
            let subscription = cx.subscribe(&state, move |this, _, event, cx| {
                if let SelectEvent::Confirm(Some(family)) = event {
                    this.settings.terminal.font_family =
                        (family != &default_label).then(|| family.clone());
                    this.persist_settings();
                    cx.notify();
                }
            });
            self.terminal_font_picker = Some(FontPicker {
                lang: self.lang,
                available: fonts,
                state,
                _subscription: subscription,
            });
        }
        let picker = self
            .terminal_font_picker
            .as_ref()
            .expect("font picker initialized");
        let state = &picker.state;
        let selected = self
            .settings
            .terminal
            .font_family
            .as_ref()
            .filter(|family| picker.available.contains(family))
            .unwrap_or(&default);
        if state.read(cx).selected_value() != Some(selected) {
            state.update(cx, |state, cx| {
                state.set_selected_value(selected, window, cx)
            });
        }
        div().flex_none().w(px(214.)).child(
            Select::new(state)
                .w_full()
                .accessibility_label(i18n::t(self.lang, "terminalFontFamily").to_owned())
                .search_placeholder(i18n::t(self.lang, "terminalFontSearch").to_owned()),
        )
    }

    pub(crate) fn terminal_zoom_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let k = &event.keystroke;
        let primary = if cfg!(target_os = "macos") {
            k.modifiers.platform && !k.modifiers.control
        } else {
            k.modifiers.control && !k.modifiers.platform
        };
        if !primary || k.modifiers.alt {
            return false;
        }
        let base = self.terminal_font_size();
        let current = self.terminal_zoom_size.unwrap_or(base);
        let next = match k.key.as_str() {
            "+" | "=" => (current + 1.).min(MAX_TERMINAL_FONT_SIZE),
            "-" | "_" => (current - 1.).max(MIN_TERMINAL_FONT_SIZE),
            "0" => base,
            _ => return false,
        };
        if next != current {
            self.terminal_zoom_size = (next != base).then_some(next);
            cx.notify();
        }
        cx.stop_propagation();
        true
    }

    // ------------------------------------------------------------------
    // Agent 接入（设置页面板动作；认证运行时热替换 + settings.json 落盘）
    // ------------------------------------------------------------------

    /// 先原子保存，再发布运行时配置；写盘失败时恢复已提交配置。
    fn sync_agent_access(&mut self) -> bool {
        if let Err(err) = store::save_settings_atomic(&self.data_dir, &self.settings) {
            self.settings.agent_access = self.access_runtime.settings();
            self.notify_toast(format!("保存 settings 失败: {err}"));
            return false;
        }
        self.access_runtime
            .update(self.settings.agent_access.clone());
        if let Some(lease) = self.device.lease_manager() {
            lease.set_auth_enabled(self.access_runtime.enabled());
            lease.set_ttl(Duration::from_secs(u64::from(
                self.access_runtime.lease_ttl_secs().max(1),
            )));
        }
        true
    }

    /// 设置页「Agent 接入」保存：提交端口/TTL 草稿（解析失败保留原值）并同步。
    /// 监听端口重启生效（mcp_runtime 启动时绑定）。
    pub fn save_agent_access(&mut self) {
        let Ok(port @ 1..=u16::MAX) = self.input_value(In::AgentAccessPort).trim().parse::<u16>()
        else {
            self.notify_toast(crate::i18n::label("mcpListenerInvalidPort"));
            return;
        };
        let Ok(ttl @ 1..=u32::MAX) = self.input_value(In::AgentAccessTtl).trim().parse::<u32>()
        else {
            self.notify_toast(crate::i18n::label("mcpInvalidLeaseTtl"));
            return;
        };
        let access = &self.settings.agent_access;
        if access
            .listeners
            .iter()
            .any(|listener| listener.port == port)
        {
            self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
            return;
        }
        self.settings.agent_access.http_port = port;
        self.settings.agent_access.lease_ttl_secs = ttl;
        if self.sync_agent_access() {
            self.drafts.remove(&In::AgentAccessPort);
            self.drafts.remove(&In::AgentAccessTtl);
            self.notify_toast("Agent 接入配置已保存（监听端口重启生效）");
        }
    }

    /// auth 总开关翻转（开启即 deny by default；关闭恢复现状兼容免 token）。
    pub fn toggle_agent_auth(&mut self) {
        if !self.settings.agent_access.listeners.is_empty() {
            return;
        }
        self.settings.agent_access.enabled = !self.settings.agent_access.enabled;
        if self.sync_agent_access()
            && self.access_runtime.enabled()
            && let Some(lease) = self.device.lease_manager()
        {
            lease.revoke_credential("local-admin");
        }
    }

    /// 「生成并添加」凭据：token 由服务端 CSPRNG 生成（`dth-` 前缀 ≥256bit），
    /// 默认启用、未授权任何 DUT（deny by default）。
    pub fn add_agent_credential(&mut self) {
        let name = self
            .drafts
            .get(&In::AgentAccessCredName)
            .cloned()
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            self.notify_toast("请先填写 Agent 名称");
            return;
        }
        let id = format!("cred-{}", uuid::Uuid::new_v4());
        let token = duthub_core::agent_access::generate_token();
        self.settings
            .agent_access
            .agents
            .push(duthub_core::AgentCredential {
                id,
                listener_id: self.agent_credential_listener.clone(),
                agent_name: name,
                token,
                enabled: true,
                grants: Vec::new(),
            });
        if self.sync_agent_access() {
            self.drafts.remove(&In::AgentAccessCredName);
            self.notify_toast("凭据已添加并保存");
        }
    }

    pub(crate) fn add_mcp_listener(&mut self) {
        if !self.access_runtime.enabled() {
            self.notify_toast(crate::i18n::label("mcpEnableAuthFirst"));
            return;
        }
        let port = self
            .input_value(In::AgentAccessExtraPort)
            .trim()
            .parse::<u16>();
        let Ok(port @ 1..=u16::MAX) = port else {
            self.notify_toast(crate::i18n::label("mcpListenerInvalidPort"));
            return;
        };
        let access = &mut self.settings.agent_access;
        if port == access.http_port
            || access
                .listeners
                .iter()
                .any(|listener| listener.port == port)
            || std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_err()
        {
            self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
            return;
        }
        let id = uuid::Uuid::new_v4().to_string();
        access
            .listeners
            .push(duthub_core::agent_access::AgentListener {
                id: id.clone(),
                port,
            });
        access.enabled = true;
        if self.sync_agent_access() {
            self.agent_credential_listener = Some(id);
            self.drafts.remove(&In::AgentAccessExtraPort);
            self.notify_toast(crate::i18n::label("mcpListenerRestart"));
        }
    }

    pub(crate) fn remove_mcp_listener(&mut self, id: &str) {
        let access = &mut self.settings.agent_access;
        if access
            .agents
            .iter()
            .any(|cred| cred.listener_id.as_deref() == Some(id))
        {
            self.notify_toast(crate::i18n::label("mcpListenerRevokeFirst"));
            return;
        }
        access.listeners.retain(|listener| listener.id != id);
        if self.sync_agent_access() {
            if self.agent_credential_listener.as_deref() == Some(id) {
                self.agent_credential_listener = None;
            }
            self.notify_toast(crate::i18n::label("mcpListenerRestart"));
        }
    }

    /// 凭据启用/停用（停用后运行时立即拒绝其 token）。
    pub fn toggle_agent_credential(&mut self, cred_id: &str) {
        let Some(cred) = self
            .settings
            .agent_access
            .agents
            .iter_mut()
            .find(|c| c.id == cred_id)
        else {
            return;
        };
        cred.enabled = !cred.enabled;
        let disabled = !cred.enabled;
        if self.sync_agent_access()
            && disabled
            && let Some(lease) = self.device.lease_manager()
        {
            lease.revoke_credential(cred_id);
        }
    }

    /// 撤销凭据：同时释放该 Agent 在所有 DUT 上持有的租约（凭据已不存在，
    /// 其租约不能再被续期，显式回收避免占用 TTL 到期）。
    pub fn revoke_agent_credential(&mut self, cred_id: &str) {
        self.settings
            .agent_access
            .agents
            .retain(|c| c.id != cred_id);
        if self.sync_agent_access() {
            if let Some(lease) = self.device.lease_manager() {
                lease.revoke_credential(cred_id);
            }
            self.notify_toast("凭据已撤销");
        }
    }

    /// 授权勾选翻转（deny by default：无该 DUT/动作即拒绝；全空动作回收授权行）。
    pub fn toggle_agent_grant(&mut self, cred_id: &str, dut_id: &str, action: DutAction) {
        let Some(cred) = self
            .settings
            .agent_access
            .agents
            .iter_mut()
            .find(|c| c.id == cred_id)
        else {
            return;
        };
        let existing = cred
            .grants
            .iter_mut()
            .find(|g| g.dut_id == dut_id)
            .map(|g| {
                if g.actions.contains(&action) {
                    g.actions.retain(|a| *a != action);
                } else {
                    g.actions.push(action);
                }
            });
        if existing.is_none() {
            cred.grants.push(DutGrant {
                dut_id: dut_id.to_string(),
                actions: vec![action],
            });
        }
        cred.grants.retain(|g| !g.actions.is_empty());
        self.sync_agent_access();
    }

    /// DUT 控制权面板「释放」：仅回收 GUI 自己持有的租约（Agent 租约走强制接管）。
    pub fn gui_release_lease(&mut self, dut_id: &str) {
        if let Some(lease) = self.device.lease_manager() {
            lease.release_for_holder(dut_id, &duthub_core::LeaseHolder::Gui);
            self.notify_toast(format!("已释放 {dut_id} 的 GUI 租约"));
        }
    }

    /// DUT 控制权面板「强制接管」：撤销旧租约（generation+1）并授予 GUI；
    /// Protected/待检查期拒绝（err.message() 带原因）。
    pub fn gui_force_takeover(&mut self, dut_id: &str) {
        if let Some(lease) = self.device.lease_manager() {
            match lease.force_takeover(dut_id) {
                Ok(grant) => {
                    self.notify_toast(format!(
                        "已接管 {dut_id}（gen {}，TTL {}s）",
                        grant.generation, self.settings.agent_access.lease_ttl_secs
                    ));
                }
                Err(err) => self.notify_toast(format!("接管失败: {}", err.message())),
            }
        }
    }

    /// DUT 控制权面板「安全恢复」：NeedsInspection 的人工确认入口。
    pub fn gui_resolve_inspection(&mut self, dut_id: &str) {
        if let Some(lease) = self.device.lease_manager() {
            lease.resolve_inspection(dut_id, true);
            self.notify_toast(format!("{dut_id} 已确认恢复可用"));
        }
    }

    pub fn save_duts(&mut self) {
        let mut file = self.duts.file.clone();
        let mut drafts = self.heartbeat_drafts.clone();
        if let Some(connection) = &self.duts.new_connection {
            drafts.remove(&connection.dut.id);
        }
        let result = self.persist_bindings(&mut file, &drafts);
        match result {
            Ok(()) => {
                self.duts.file = file;
                self.duts.sync_saved_media();
                self.media.reconcile_bindings(&self.duts);
                self.duts.dirty = false;
                self.heartbeat_drafts
                    .retain(|id, _| !drafts.contains_key(id));
                for session in &mut self.sessions {
                    if let Some(dut) = self
                        .duts
                        .file
                        .duts
                        .iter()
                        .find(|dut| dut.id == session.dut_id)
                    {
                        session.title = format!("{} · {}", dut.name, dut.serial.port);
                    }
                }
                self.notify_toast(i18n::t(self.lang, "saveBinding"));
            }
            Err(err) => self.notify_toast(format!("保存失败: {err}")),
        }
    }

    pub fn dut_alert_settings(&self, dut_id: &str) -> &duthub_core::model::AlertSettings {
        if let Some(draft) = &self.duts.new_connection
            && draft.dut.id == dut_id
        {
            return draft.alerts.as_ref().unwrap_or(&self.settings.alerts);
        }
        self.duts
            .file
            .alerts_by_dut
            .get(dut_id)
            .unwrap_or(&self.settings.alerts)
    }

    pub fn edit_dut_alerts(
        &mut self,
        dut_id: &str,
        edit: impl FnOnce(&mut duthub_core::model::AlertSettings),
    ) {
        if let Some(draft) = &mut self.duts.new_connection
            && draft.dut.id == dut_id
        {
            edit(
                draft
                    .alerts
                    .get_or_insert_with(|| self.settings.alerts.clone()),
            );
            return;
        }
        if !self.duts.file.duts.iter().any(|dut| dut.id == dut_id) {
            return;
        }
        let mut settings = self.dut_alert_settings(dut_id).clone();
        edit(&mut settings);
        let result = self.heartbeat.commit_configs(&HashMap::new(), |_| {
            let mut file = store::load_duts(&self.data_dir)
                .map_err(|err| err.to_string())?
                .unwrap_or_else(|| self.duts.file.clone());
            if !file.duts.iter().any(|dut| dut.id == dut_id) {
                return Err(crate::i18n::label("saveDutBeforeAlerts").into());
            }
            file.alerts_by_dut
                .insert(dut_id.to_owned(), settings.clone());
            store::save_duts_atomic(&self.data_dir, &file).map_err(|err| err.to_string())
        });
        match result {
            Ok(()) => {
                self.duts
                    .file
                    .alerts_by_dut
                    .insert(dut_id.to_owned(), settings);
                self.alerts
                    .lock()
                    .unwrap()
                    .set_dut_settings(&self.duts.file.alerts_by_dut);
            }
            Err(err) => self.notify_toast(format!("{} {err}", crate::i18n::label("saveFailed"))),
        }
    }

    fn persist_bindings(
        &self,
        file: &mut duthub_core::DutsFile,
        heartbeat_drafts: &HashMap<String, duthub_core::heartbeat::HeartbeatConfig>,
    ) -> Result<(), String> {
        Self::validate_bindings(file)?;
        let mut renamed = Vec::new();
        let result = self.heartbeat.commit_configs(heartbeat_drafts, |configs| {
            Self::apply_heartbeat_configs(file, configs);
            for dut in &file.duts {
                if let Some(old) = self.duts.saved_name(&dut.id)
                    && duthub_core::dut_artifacts::rename_owned(
                        &self.artifact_root,
                        &dut.id,
                        old,
                        &dut.name,
                    )
                    .map_err(|error| format!("移动 {} 的资料目录失败: {error}", dut.name))?
                {
                    renamed.push((dut.id.clone(), old.to_owned(), dut.name.clone()));
                }
            }
            store::save_duts_atomic(&self.data_dir, file).map_err(|err| err.to_string())
        });
        if let Err(mut error) = result {
            for (id, old, new) in renamed.into_iter().rev() {
                if let Err(rollback) =
                    duthub_core::dut_artifacts::rename_owned(&self.artifact_root, &id, &new, &old)
                {
                    error.push_str(&format!(" · 无法恢复目录 {old}: {rollback}"));
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn validate_bindings(file: &mut duthub_core::DutsFile) -> Result<(), String> {
        file.migrate_usb_relays();
        crate::media_page::validate_bindings(&file.duts)?;
        let mut ports = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for dut in &file.duts {
            if !ids.insert(&dut.id) {
                return Err(i18n::message("duplicateDutId", &[("id", &dut.id)]));
            }
            if dut.name.trim().is_empty() {
                return Err(crate::i18n::label("enterDutName").into());
            }
            duthub_core::dut_artifacts::name_component(&dut.name)
                .map_err(|error| format!("{}: {error}", dut.name))?;
            if !names.insert(dut.name.to_lowercase()) {
                return Err(format!("DUT 目录名重复: {}", dut.name));
            }
            let port = if cfg!(windows) {
                dut.serial.port.to_ascii_lowercase()
            } else {
                dut.serial.port.clone()
            };
            if !port.is_empty() && !ports.insert((&dut.serial.host, port)) {
                return Err(format!("串口 {} 已绑定到另一台 DUT", dut.serial.port));
            }
        }
        for board in &file.relay_boards {
            let node = board.node.trim();
            if board.name.trim().is_empty() || node.is_empty() {
                return Err(crate::i18n::label("relayNameAndPort").into());
            }
            let port = if cfg!(windows) {
                node.to_ascii_lowercase()
            } else {
                node.to_owned()
            };
            if !ports.insert((&board.host, port)) {
                return Err(format!("继电器串口 {node} 已被占用"));
            }
        }
        for dut in &file.duts {
            if let duthub_core::model::RelayConfig::UsbBoard { board_id, channels } = &dut.relay {
                let limit = file
                    .relay_boards
                    .iter()
                    .find(|board| board.id == *board_id)
                    .ok_or_else(|| format!("「{}」的继电器板不存在", dut.name))?
                    .channel_count;
                if let Some(channel) = channels
                    .assigned()
                    .find(|&channel| channel == 0 || channel > limit)
                {
                    return Err(format!("「{}」的 CH{channel} 超出板卡通道范围", dut.name));
                }
            }
        }
        Ok(())
    }

    fn apply_heartbeat_configs(
        file: &mut duthub_core::DutsFile,
        configs: &HashMap<String, duthub_core::heartbeat::HeartbeatConfig>,
    ) {
        for dut in &mut file.duts {
            if let Some(config) = configs.get(&dut.id) {
                dut.heartbeat = config.clone();
            }
        }
    }

    pub(crate) fn heartbeat_config(&self, dut_id: &str) -> duthub_core::heartbeat::HeartbeatConfig {
        self.heartbeat_drafts
            .get(dut_id)
            .cloned()
            .unwrap_or_else(|| self.heartbeat.config(dut_id))
    }

    pub(crate) fn toggle_heartbeat(&mut self) {
        if let Some(dut) = self.duts.selected() {
            let mut config = self.heartbeat_config(&dut.id);
            config.enabled = !config.enabled;
            config.paused_until_unix_ms = None;
            self.heartbeat_drafts.insert(dut.id.clone(), config);
            self.duts.mark_connection_dirty();
        }
    }

    pub fn import_config(&mut self, base: &'static str, cx: &mut Context<Self>) {
        let path = self.data_dir.join(format!("{base}.json"));
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                if base == "settings" {
                    match serde_json::from_str::<duthub_core::SettingsFile>(&raw) {
                        Ok(s) => {
                            self.lang = Lang::from_setting(&s.appearance.language);
                            self.lang.activate();
                            self.theme_mode = theme::ThemeMode::from_setting(&s.appearance.theme);
                            theme::apply_brand(cx, self.theme_mode);
                            self.settings = s;
                            self.drafts.remove(&In::DutStorageGib);
                            self.artifact_quota.set_limit(
                                u64::from(self.settings.dut_storage_gib())
                                    * duthub_core::dut_quota::GIB,
                            );
                            self.provider_models = crate::provider_models::Catalog::default();
                            self.terminal_zoom_size = None;
                            self.notify_toast("settings.json 已导入");
                        }
                        Err(err) => self.notify_toast(format!("导入失败: {err}")),
                    }
                } else {
                    match serde_json::from_str::<duthub_core::DutsFile>(&raw) {
                        Ok(d) => {
                            self.duts = DutsState::from_file(d);
                            self.media.reconcile_bindings(&self.duts);
                            self.relay_board_selected = 0;
                            self.relay_board_name = None;
                            self.alerts
                                .lock()
                                .unwrap()
                                .set_dut_settings(&self.duts.file.alerts_by_dut);
                            self.notify_toast("duts.json 已导入");
                        }
                        Err(err) => self.notify_toast(format!("导入失败: {err}")),
                    }
                }
            }
            Err(err) => self.notify_toast(format!("导入失败（{path:?}）: {err}")),
        }
    }

    pub(crate) fn config_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub fn export_a2a_config(&mut self) {
        let a2a = self.settings.a2a.clone().unwrap_or_default();
        let exports = self.work_dir.join("exports");
        let _ = std::fs::create_dir_all(&exports);
        let path = exports.join("a2a_server.json");
        let payload = serde_json::json!({
            "agentName": a2a.agent_name,
            "port": a2a.port,
            "peers": a2a.peers.iter().map(|p| serde_json::json!({"id": p.id, "name": p.name, "agentCardUrl": p.card_url})).collect::<Vec<_>>(),
        });
        match serde_json::to_string_pretty(&payload)
            .map_err(|e| e.to_string())
            .and_then(|s| std::fs::write(&path, s).map_err(|e| e.to_string()))
        {
            Ok(()) => self.notify_toast(format!("已导出 {}", path.display())),
            Err(err) => self.notify_toast(format!("导出失败: {err}")),
        }
    }

    /// 导出 MCP 使用技能文档（需求⑥）：`<工作目录>/skill/duthub/SKILL.md`。
    /// 与 MCP 工具 skill_export 同一生成器（duthub_mcp::skill）。
    pub fn export_skill_md(&mut self) {
        match duthub_mcp::skill::export_skill_for_listener(
            &self.work_dir,
            self.mcp_bound_port.get().copied(),
        ) {
            Ok(path) => self.notify_toast(format!("已导出 {}", path.display())),
            Err(err) => self.notify_toast(format!("导出 skill 失败: {err}")),
        }
    }

    pub fn export_agent_report(&mut self, report: &str) {
        use std::io::Write;
        let Some((dut_id, name)) = self
            .agent
            .active_dut_id()
            .and_then(|id| self.duts.saved_name(id).map(|name| (id, name)))
        else {
            self.notify_toast(crate::i18n::label("dutNotFound"));
            return;
        };
        let filename = format!("agent-report-{}.md", uuid::Uuid::new_v4());
        let saved =
            duthub_core::dut_artifacts::open_dut_for_write(&self.artifact_root, dut_id, name)
                .and_then(|(name, owner)| {
                    let mut file = self.artifact_quota.create(dut_id, &owner, &filename)?;
                    file.write_all(report.as_bytes())?;
                    Ok(self.artifact_root.join(name).join(&filename))
                });
        match saved {
            Ok(path) => self.notify_toast(format!("报告已导出 {}", path.display())),
            Err(err) => self.notify_toast(format!("导出失败: {err}")),
        }
    }

    // ------------------------------------------------------------------
    // Agent 辅助（agent.rs 调用）
    // ------------------------------------------------------------------

    fn sync_serial_agent(&mut self) {
        let serial = self
            .sessions
            .get(self.active_session)
            .filter(|_| self.view == ViewMode::Serial);
        self.agent
            .bind(serial.map(|view| (view.dut_id.as_str(), view.title.as_str())));
    }

    pub fn agent_start_from_prompt(&mut self, cx: &mut Context<Self>) {
        self.sync_serial_agent();
        let Some(session) = self.agent.active_mut() else {
            return;
        };
        if session.status == RunStatus::Running {
            return;
        }
        let prompt = std::mem::take(&mut session.prompt);
        self.start_agent();
        if let Some(session) = self.agent.active_mut()
            && !prompt.trim().is_empty()
        {
            session.events.insert(
                0,
                agent::AgentEvent {
                    step_title: "指令".into(),
                    description: prompt.trim().to_owned(),
                },
            );
        }
        cx.notify();
    }

    pub(crate) fn start_agent(&mut self) {
        self.sync_serial_agent();
        let Some(run) = self.agent.active() else {
            return;
        };
        if run.workflow != agent::Workflow::Dut {
            self.agent.start();
            return;
        }
        let Some(view) = self.sessions.get(self.active_session) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        let active = (view.source == "device").then_some(view);
        let (label, evidence) = active.map_or_else(
            || ("未连接真实串口".to_string(), None),
            |view| {
                let log = self
                    .device
                    .read_log(&view.session_id, 0, usize::MAX)
                    .into_iter()
                    .filter_map(|entry| match entry.event {
                        UiEvent::SerialChunk {
                            style: ChunkStyle::Normal,
                            text,
                        } => Some(text),
                        _ => None,
                    })
                    .collect::<String>();
                (
                    view.title.clone(),
                    agent::serial_crash_evidence(&log).map(str::to_owned),
                )
            },
        );
        self.agent.start();
        if let Some(run) = self.agent.active_mut() {
            run.step = run.workflow.steps().len();
            run.status = RunStatus::Done;
            run.events.push(agent::AgentEvent {
                step_title: crate::i18n::label("preliminarySerial").into(),
                description: if active.is_some() {
                    crate::i18n::label("reviewSerialLog").into()
                } else {
                    crate::i18n::label("connectSerialBeforeAnalysis").into()
                },
            });
            if let Some(line) = evidence {
                run.findings.push(("串口崩溃迹象".into(), line.clone()));
                self.alerts.lock().unwrap().report_for(
                    &dut_id,
                    AlertSource::Agent,
                    crate::i18n::label("serialCrash"),
                    format!("{label}\n{line}"),
                );
            } else if active.is_some() {
                run.events.push(agent::AgentEvent {
                    step_title: crate::i18n::label("analysisResult").into(),
                    description: "当前缓冲区未命中已知崩溃特征".into(),
                });
            }
        }
    }

    // ------------------------------------------------------------------
    // 输入体系
    // ------------------------------------------------------------------

    pub fn input_value(&self, id: In) -> String {
        let dut = self.duts.selected();
        match id {
            In::RelayBoardName | In::RelayBoardNode => self
                .duts
                .file
                .relay_boards
                .get(self.relay_board_selected)
                .map(|board| {
                    if id == In::RelayBoardName {
                        board.name.clone()
                    } else {
                        board.node.clone()
                    }
                })
                .unwrap_or_default(),
            In::Find => self.find_query.clone(),
            In::CfgSearch => self.cfg_search.clone(),
            In::Target => self
                .agent
                .active()
                .map(|s| s.target.clone())
                .unwrap_or_default(),
            In::AgentModel => self.agent.model_name.clone(),
            In::Prompt => self.input.clone(),
            In::Port => dut.map(|d| d.serial.port.clone()).unwrap_or_default(),
            In::DutName => dut.map(|d| d.name.clone()).unwrap_or_default(),
            In::Baud => dut.map(|d| d.serial.baud.to_string()).unwrap_or_default(),
            In::SessionBaud => self
                .sessions
                .get(self.active_session)
                .map(|s| {
                    s.baud_draft.clone().unwrap_or_else(|| {
                        if s.baud == 0 {
                            String::new()
                        } else {
                            s.baud.to_string()
                        }
                    })
                })
                .unwrap_or_default(),
            In::AdbSerial => dut.map(|d| d.adb.serial.clone()).unwrap_or_default(),
            In::RelayNode => dut
                .map(|d| match &d.relay {
                    duthub_core::model::RelayConfig::UsbSerial { node, .. } => node.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default(),
            In::PduAddress => dut
                .map(|d| match &d.relay {
                    duthub_core::model::RelayConfig::SnmpPdu { snmp, .. } => snmp.address.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default(),
            In::PduCredential => dut
                .map(|d| match &d.relay {
                    duthub_core::model::RelayConfig::SnmpPdu { snmp, .. } => {
                        snmp.credential_ref.clone()
                    }
                    _ => String::new(),
                })
                .unwrap_or_default(),
            In::McpUrl => dut
                .map(|d| match &d.mcp {
                    duthub_core::model::McpConfig::StreamableHttp { url } => url.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default(),
            In::McpCommand => dut
                .map(|d| match &d.mcp {
                    duthub_core::model::McpConfig::Stdio { command, .. } => command.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default(),
            In::McpArgs => dut
                .map(|d| match &d.mcp {
                    duthub_core::model::McpConfig::Stdio { args, .. } => args.join("\n"),
                    _ => String::new(),
                })
                .unwrap_or_default(),
            In::McpCwd => dut
                .map(|d| match &d.mcp {
                    duthub_core::model::McpConfig::Stdio { cwd, .. } => cwd.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default(),
            In::McpEnv => dut
                .map(|d| match &d.mcp {
                    duthub_core::model::McpConfig::Stdio { env, .. } => env
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                })
                .unwrap_or_default(),
            In::Regex => self.settings.terminal.prompt_highlight.regex.clone(),
            In::AdbExe => self.settings.tools.adb_executable.clone(),
            In::HdcExe => self.settings.tools.hdc_executable.clone(),
            In::WorkDir => self.settings.workspace_dir.clone().unwrap_or_default(),
            In::DutStorageGib => self
                .drafts
                .get(&id)
                .cloned()
                .unwrap_or_else(|| self.settings.dut_storage_gib().to_string()),
            In::HeartbeatInterval => dut
                .map(|d| self.heartbeat_config(&d.id).interval_secs.to_string())
                .unwrap_or_default(),
            In::HeartbeatCommand => dut
                .map(|d| self.heartbeat_config(&d.id).command)
                .unwrap_or_default(),
            In::HeartbeatExpectedResponse => dut
                .and_then(|d| self.heartbeat_config(&d.id).expected_response)
                .unwrap_or_default(),
            In::AgentAccessPort => self
                .drafts
                .get(&In::AgentAccessPort)
                .cloned()
                .unwrap_or_else(|| self.settings.agent_access.http_port.to_string()),
            In::AgentAccessTtl => self
                .drafts
                .get(&In::AgentAccessTtl)
                .cloned()
                .unwrap_or_else(|| self.settings.agent_access.lease_ttl_secs.to_string()),
            In::AgentAccessExtraPort => self.drafts.get(&id).cloned().unwrap_or_default(),
            In::AgentAccessCredName => self
                .drafts
                .get(&In::AgentAccessCredName)
                .cloned()
                .unwrap_or_default(),
            In::Media(field) => field.value(&self.settings.tools.capture, dut).to_owned(),
            In::A2aName => self
                .settings
                .a2a
                .as_ref()
                .map(|a| a.agent_name.clone())
                .unwrap_or_default(),
            In::A2aPort => self.drafts.get(&In::A2aPort).cloned().unwrap_or_else(|| {
                self.settings
                    .a2a
                    .as_ref()
                    .map(|a| a.port.to_string())
                    .unwrap_or_default()
            }),
            In::CmdName
            | In::CmdValue
            | In::NewPresetName
            | In::NewPresetExe
            | In::PeerName
            | In::PeerUrl
            | In::RenameName => self
                .modal
                .as_ref()
                .map(|m| match id {
                    In::CmdName | In::NewPresetName | In::PeerName | In::RenameName => {
                        m.draft.clone()
                    }
                    _ => m.draft2.clone(),
                })
                .unwrap_or_default(),
        }
    }

    fn input_set(&mut self, id: In, value: String) {
        let dut = self.duts.selected_mut();
        let settings = &mut self.settings;
        match id {
            In::Find => self.find_query = value,
            In::CfgSearch => self.cfg_search = value,
            In::Target => {
                if let Some(session) = self.agent.active_mut() {
                    session.target = value;
                }
            }
            In::AgentModel => self.agent.model_name = value,
            In::DutName => {
                if let Some(dut) = dut {
                    dut.name = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::Prompt => self.input = value,
            In::Port => {
                if let Some(d) = dut {
                    d.serial.port = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::Baud => {
                if let Ok(b) = value.parse::<u32>()
                    && let Some(d) = dut
                {
                    d.serial.baud = b;
                    self.duts.mark_connection_dirty();
                }
            }
            In::SessionBaud => {
                if let Some(view) = self.sessions.get_mut(self.active_session) {
                    view.baud_draft = if value.is_empty() { None } else { Some(value) };
                }
            }
            In::AdbSerial => {
                if let Some(d) = dut {
                    d.adb.serial = value;
                    d.adb.mode = "usb".into();
                    self.duts.mark_connection_dirty();
                }
            }
            In::RelayBoardName | In::RelayBoardNode => {
                if let Some(board) = self
                    .duts
                    .file
                    .relay_boards
                    .get_mut(self.relay_board_selected)
                {
                    if id == In::RelayBoardName {
                        board.name = value;
                    } else {
                        board.node = value;
                    }
                    self.duts.dirty = true;
                }
            }
            In::RelayNode => {
                if let Some(d) = dut
                    && let duthub_core::model::RelayConfig::UsbSerial { node, .. } = &mut d.relay
                {
                    *node = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::PduAddress => {
                if let Some(d) = dut
                    && let duthub_core::model::RelayConfig::SnmpPdu { snmp, .. } = &mut d.relay
                {
                    snmp.address = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::PduCredential => {
                if let Some(d) = dut
                    && let duthub_core::model::RelayConfig::SnmpPdu { snmp, .. } = &mut d.relay
                {
                    snmp.credential_ref = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpUrl => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::StreamableHttp { url } = &mut d.mcp
                {
                    *url = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpCommand => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::Stdio { command, .. } = &mut d.mcp
                {
                    *command = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpArgs => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::Stdio { args, .. } = &mut d.mcp
                {
                    *args = value
                        .lines()
                        .map(str::to_string)
                        .filter(|l| !l.is_empty())
                        .collect();
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpCwd => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::Stdio { cwd, .. } = &mut d.mcp
                {
                    *cwd = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpEnv => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::Stdio { env, .. } = &mut d.mcp
                {
                    env.clear();
                    for line in value.lines() {
                        let line = line.trim();
                        if let Some((k, v)) = line.split_once('=') {
                            env.insert(k.trim().to_string(), v.trim().to_string());
                        }
                    }
                    self.duts.mark_connection_dirty();
                }
            }
            In::HeartbeatInterval => {
                if let Ok(secs) = value.parse::<u32>()
                    && let Some(d) = dut
                {
                    let cfg = self
                        .heartbeat_drafts
                        .entry(d.id.clone())
                        .or_insert_with(|| self.heartbeat.config(&d.id));
                    cfg.interval_secs = secs;
                    cfg.interval_secs = cfg.normalized_interval();
                    self.duts.mark_connection_dirty();
                }
            }
            In::HeartbeatCommand | In::HeartbeatExpectedResponse => {
                if let Some(d) = dut {
                    let cfg = self
                        .heartbeat_drafts
                        .entry(d.id.clone())
                        .or_insert_with(|| self.heartbeat.config(&d.id));
                    if id == In::HeartbeatCommand {
                        cfg.command = value;
                    } else {
                        cfg.expected_response = (!value.is_empty()).then_some(value);
                    }
                    self.duts.mark_connection_dirty();
                }
            }
            In::Regex => settings.terminal.prompt_highlight.regex = value,
            In::AdbExe => settings.tools.adb_executable = value,
            In::HdcExe => settings.tools.hdc_executable = value,
            In::WorkDir => settings.workspace_dir = (!value.trim().is_empty()).then_some(value),
            In::DutStorageGib => {
                self.drafts.insert(id, value);
            }
            In::Media(field) => {
                if let Some(dest) = field.edit(&mut settings.tools.capture, dut) {
                    *dest = value;
                    if field.is_dut() {
                        self.duts.mark_connection_dirty();
                    }
                }
            }
            In::A2aName => {
                let a2a = settings
                    .a2a
                    .get_or_insert_with(duthub_core::A2aConfig::default);
                a2a.agent_name = value;
            }
            In::A2aPort => {
                // 与 In::Baud 同模式：草稿保原文（可清空），解析成功才写模型
                if value.is_empty() {
                    self.drafts.remove(&In::A2aPort);
                } else {
                    self.drafts.insert(In::A2aPort, value.clone());
                }
                if let Ok(port) = value.parse::<u16>() {
                    let a2a = settings
                        .a2a
                        .get_or_insert_with(duthub_core::A2aConfig::default);
                    a2a.port = port;
                }
            }
            In::AgentAccessPort
            | In::AgentAccessTtl
            | In::AgentAccessCredName
            | In::AgentAccessExtraPort => {
                self.drafts.insert(id, value);
            }
            In::CmdName | In::NewPresetName | In::PeerName | In::RenameName => {
                if let Some(m) = self.modal.as_mut() {
                    m.draft = value;
                }
            }
            In::CmdValue | In::NewPresetExe | In::PeerUrl => {
                if let Some(m) = self.modal.as_mut() {
                    m.draft2 = value;
                }
            }
        }
    }

    /// 中央按键路由；返回值：Enter 是否按下。
    fn input_key(&mut self, id: In, event: &KeyDownEvent) -> bool {
        let k = &event.keystroke;
        if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
            return false;
        }
        if k.key == "enter" {
            if id == In::SessionBaud {
                self.apply_session_baud();
                return true;
            }
            if id.multiline() {
                self.input_set(id, format!("{}\n", self.input_value(id)));
                return true;
            }
            return true;
        }
        if k.key == "backspace" {
            let mut value = self.input_value(id);
            value.pop();
            self.input_set(id, value);
            return false;
        }
        if let Some(ch) = typed_char(event) {
            let mut value = self.input_value(id);
            if value.len() < 8192 {
                value.push_str(&ch);
                self.input_set(id, value);
            }
        }
        false
    }

    fn editor_scope(&self, id: In) -> Option<&str> {
        match id {
            In::RelayBoardNode | In::RelayBoardName => self
                .duts
                .file
                .relay_boards
                .get(self.relay_board_selected)
                .map(|board| board.id.as_str()),
            In::Target | In::SessionBaud => self
                .sessions
                .get(self.active_session)
                .map(|view| view.dut_id.as_str()),
            _ => match id {
                In::Port
                | In::DutName
                | In::Baud
                | In::AdbSerial
                | In::RelayNode
                | In::PduAddress
                | In::PduCredential
                | In::McpUrl
                | In::McpCommand
                | In::McpArgs
                | In::McpCwd
                | In::McpEnv
                | In::HeartbeatInterval
                | In::HeartbeatCommand
                | In::HeartbeatExpectedResponse => self.duts.selected().map(|dut| dut.id.as_str()),
                In::Media(field) if field.is_dut() => {
                    self.duts.selected().map(|dut| dut.id.as_str())
                }
                _ => None,
            },
        }
    }

    fn line_editor(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Entity<InputState> {
        let scope = self.editor_scope(id).map(str::to_owned);
        if let Some(editor) = self.text_editors.get_mut(&id)
            && editor.scope == scope
        {
            if editor.model_value != value || editor.placeholder != placeholder {
                editor.state.update(cx, |state, cx| {
                    if editor.placeholder != placeholder {
                        state.set_placeholder(placeholder.to_owned(), window, cx);
                    }
                    if editor.model_value != value && state.value().as_ref() != value {
                        state.set_value(value.to_owned(), window, cx);
                    }
                });
                value.clone_into(&mut editor.model_value);
                placeholder.clone_into(&mut editor.placeholder);
            }
            return editor.state.clone();
        }

        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder.to_owned())
                .default_value(value.to_owned())
        });
        let owner = scope.clone();
        let subscription = cx.subscribe(&state, move |this, state, event, cx| {
            if this.editor_scope(id) != owner.as_deref()
                || this
                    .text_editors
                    .get(&id)
                    .is_none_or(|editor| editor.state != state)
            {
                return;
            }
            match event {
                InputEvent::Change => {
                    let value = state.read(cx).value().to_string();
                    this.input_set(id, value);
                    let model = this.input_value(id);
                    if let Some(editor) = this.text_editors.get_mut(&id) {
                        // Keep the editor's raw text for incomplete numbers and composition.
                        editor.model_value = model;
                    }
                }
                InputEvent::Focus => {
                    let dropdown = match id {
                        In::Port => Some(Dd::Port),
                        In::RelayBoardNode => Some(Dd::RelayBoardPort),
                        In::Baud => Some(Dd::Baud),
                        In::AdbSerial => Some(Dd::AdbSerial),
                        In::Media(
                            field @ (crate::media_page::MediaField::Size(_)
                            | crate::media_page::MediaField::Rate(_)),
                        ) => Some(Dd::CaptureMode(field)),
                        _ => None,
                    };
                    if let Some(dd) = dropdown {
                        this.open_combo(dd);
                    } else {
                        this.open_dd = None;
                    }
                }
                InputEvent::PressEnter { .. } if id == In::SessionBaud => this.apply_session_baud(),
                _ => return,
            }
            cx.notify();
        });
        self.text_editors.insert(
            id,
            BoundEditor {
                state: state.clone(),
                model_value: value.to_owned(),
                placeholder: placeholder.to_owned(),
                scope,
                _subscription: subscription,
            },
        );
        state
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text_field(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        mono: bool,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Input {
        let state = self.line_editor(id, value, placeholder, window, cx);
        let input = Input::new(&state)
            .id(id.key())
            .accessibility_id(id.key())
            .w_full()
            .bg(p.bg)
            .text_size(px(13.));
        if mono {
            input.font_family(theme::MONO_FONT)
        } else {
            input
        }
    }

    pub(crate) fn path_input(
        &mut self,
        id: In,
        placeholder: &str,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let input = self.text_field(id, &self.input_value(id), placeholder, true, p, window, cx);
        let label = i18n::label("browsePath");
        let button_id = format!("browse-{}", id.key());
        let button = if id == In::Target {
            self.select_field(
                "browse-target",
                label.into(),
                vec![
                    (i18n::label("selectPathFile").into(), 0),
                    (i18n::label("selectPathDirectory").into(), 1),
                ],
                None,
                Arc::new(|this, index, _, cx| this.browse_path(In::Target, index == 1, cx)),
                p,
                cx,
            )
            .min_w(px(110.))
            .into_any_element()
        } else {
            let choose = cx.listener(move |this, _, _, cx| {
                this.browse_path(id, matches!(id, In::WorkDir | In::McpCwd), cx);
            });
            if id == In::NewPresetExe {
                // 跟随弹窗内其他按钮的绘制方式。
                div()
                    .id(SharedString::from(button_id))
                    .min_h(px(32.))
                    .px(px(12.))
                    .rounded(px(8.))
                    .bg(p.raised)
                    .text_color(p.text)
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .on_click(choose)
                    .child(label)
                    .into_any_element()
            } else {
                crate::components::btn(button_id, label, &crate::components::BtnVariant::Normal, p)
                    .on_click(choose)
                    .into_any_element()
            }
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().flex_1().min_w_0().child(input))
            .child(div().flex_none().child(button))
    }

    pub fn multiline_field(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Textarea {
        let scope = self.editor_scope(id).map(str::to_owned);
        if self
            .textareas
            .get(&id)
            .is_none_or(|editor| editor.scope != scope)
        {
            let state = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(3)
                    .placeholder(placeholder.to_owned())
                    .default_value(value.to_owned())
            });
            let owner = scope.clone();
            let subscription = cx.subscribe(&state, move |this, state, event, cx| {
                if this.editor_scope(id) != owner.as_deref()
                    || this
                        .textareas
                        .get(&id)
                        .is_none_or(|editor| editor.state != state)
                {
                    return;
                }
                match event {
                    InputEvent::Change => {
                        this.input_set(id, state.read(cx).value().to_string());
                        let model = this.input_value(id);
                        if let Some(editor) = this.textareas.get_mut(&id) {
                            editor.model_value = model;
                        }
                    }
                    InputEvent::Focus => this.open_dd = None,
                    _ => return,
                }
                cx.notify();
            });
            self.textareas.insert(
                id,
                BoundEditor {
                    state,
                    model_value: value.to_owned(),
                    placeholder: placeholder.to_owned(),
                    scope,
                    _subscription: subscription,
                },
            );
        }
        let editor = self
            .textareas
            .get_mut(&id)
            .expect("textarea editor initialized");
        if editor.model_value != value || editor.placeholder != placeholder {
            editor.state.update(cx, |state, cx| {
                if editor.placeholder != placeholder {
                    state.set_placeholder(placeholder.to_owned(), window, cx);
                }
                if editor.model_value != value && state.value().as_ref() != value {
                    state.set_value(value.to_owned(), window, cx);
                }
            });
            value.clone_into(&mut editor.model_value);
            placeholder.clone_into(&mut editor.placeholder);
        }
        Textarea::new(&editor.state)
            .accessibility_id(id.key())
            .w_full()
            .bg(p.bg)
            .font_family(theme::MONO_FONT)
            .text_size(px(13.))
    }

    fn open_combo(&mut self, dd: Dd) {
        if self.overlay.is_some() && dd != Dd::RelayBoardPort {
            return;
        }
        match dd {
            Dd::AdbSerial => {
                if let Some(dut) = self.duts.selected() {
                    self.scan_usb_devices(dut.adb.tool == "hdc");
                }
            }
            Dd::Port | Dd::RelayBoardPort => self.serial_ports = self.device.list_ports(),
            Dd::CaptureMode(_) => self.media.retry_mode_probe(),
            Dd::Baud => {}
        }
        self.open_dd = Some(dd.key().into());
    }

    /// 下拉选择（连接页 combobox）：input + chevron + 展开列表。
    #[allow(clippy::too_many_arguments)]
    pub fn combobox(
        &mut self,
        id: In,
        dd: Dd,
        value: &str,
        placeholder: &str,
        mono: bool,
        options: &[(String, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Div {
        let p = self.p;
        let open = self.open_dd.as_deref() == Some(dd.key());
        let media_dut = matches!(dd, Dd::CaptureMode(_))
            .then(|| self.duts.selected().map(|dut| dut.id.clone()))
            .flatten();
        let key = id.key();
        let mut container = div().relative().w_full();
        let mut row = div().flex().items_center().gap(px(4.)).child(
            div().flex_1().min_w_0().child(self.text_field(
                id,
                value,
                placeholder,
                mono,
                p,
                window,
                cx,
            )),
        );
        row = row.child(
            div()
                .id(SharedString::from(format!("{key}-toggle")))
                .flex_none()
                .size(px(34.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .text_color(p.muted)
                .hover(|s| s.bg(p.hover).text_color(p.text))
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    if this.open_dd.as_deref() == Some(dd.key()) {
                        this.open_dd = None;
                    } else {
                        this.open_combo(dd);
                    }
                    cx.notify();
                }))
                .child(icon("chevron").size(px(16.)).text_color(p.muted)),
        );
        container = container.child(row);
        if open {
            // 内联展开（非悬浮）：跟随文档流推挤后续内容，从根上避免被兄弟元素遮挡
            let mut list = div()
                .id(SharedString::from(format!("{key}-list")))
                .mt(px(4.))
                .w_full()
                .max_h(px(250.))
                .overflow_y_scroll()
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .rounded(px(8.))
                .shadow_md()
                .p(px(4.))
                .occlude();
            if options.is_empty() {
                list = list.child(
                    div()
                        .px(px(10.))
                        .py(px(8.))
                        .text_color(p.muted)
                        .child(crate::i18n::label("noCandidates")),
                );
            }
            for (value_label, detail) in options {
                let active = value_label == value;
                let v = value_label.clone();
                let media_dut = media_dut.clone();
                let (label, detail) = if value_label.is_empty() {
                    (detail.as_str(), "")
                } else {
                    (value_label.as_str(), detail.as_str())
                };
                list = list.child(
                    div()
                        .id(SharedString::from(format!(
                            "{key}-opt-{}",
                            value_label.replace(['\\', '/'], "-")
                        )))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            if let Some(ref target) = media_dut
                                && this.duts.selected().is_none_or(|dut| &dut.id != target)
                            {
                                return;
                            }
                            this.input_set(id, v.clone());
                            let model = this.input_value(id);
                            if let Some(editor) = this.text_editors.get_mut(&id) {
                                editor.state.update(cx, |state, cx| {
                                    if state.value().as_ref() != v {
                                        state.set_value(v.clone(), window, cx);
                                    }
                                });
                                editor.model_value = model;
                            }
                            this.open_dd = None;
                            cx.notify();
                        }))
                        .flex()
                        .items_center()
                        .gap(px(11.))
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_color(p.text)
                        .hover(|s| s.bg(p.hover))
                        .when(active, |el| el.bg(p.accent_weak))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_family(crate::theme::MONO_FONT)
                                .font_weight(gpui_kit::FontWeight(600.0))
                                .child(SharedString::from(label.to_owned())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .font_family(crate::theme::MONO_FONT)
                                .text_color(p.faint)
                                .overflow_hidden()
                                .child(SharedString::from(detail.to_owned())),
                        )
                        .when(active, |el| {
                            el.child(icon("check").size(px(14.)).text_color(p.accent))
                        }),
                );
            }
            container = container.child(gpui_kit::deferred(list).with_priority(1));
        }
        container
    }

    /// 下拉选择（固定选项集 + 回调）：agent/settings 使用。
    #[allow(clippy::too_many_arguments)]
    pub fn select_field(
        &mut self,
        key: &'static str,
        value: String,
        options: Vec<(String, usize)>,
        selected: Option<usize>,
        on_change: OnChangeFn,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Div {
        let open = self.open_dd.as_deref() == Some(key);
        let mut container = div().relative().min_w_0();
        container = container.child(
            div()
                .id(SharedString::from(format!("{key}-btn")))
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.open_dd = if this.open_dd.as_deref() == Some(key) {
                        None
                    } else {
                        Some(key.to_string())
                    };
                    cx.notify();
                }))
                .min_h(px(33.))
                .w_full()
                .px(px(11.))
                .py(px(6.))
                .rounded(px(8.))
                .bg(p.bg)
                .border_1()
                .border_color(p.border)
                .text_size(px(12.5))
                .text_color(p.text)
                .overflow_hidden()
                .flex()
                .items_center()
                .child(SharedString::from(value))
                .child(
                    div()
                        .ml_auto()
                        .pl(px(6.))
                        .child(icon("chevron").size(px(14.)).text_color(p.muted)),
                ),
        );
        if open {
            let mut list = div()
                .absolute()
                .top_full()
                .left_0()
                .mt(px(4.))
                .w_full()
                .max_h(px(240.))
                .id(SharedString::from(format!("{key}-list")))
                .overflow_y_scroll()
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .rounded(px(8.))
                .shadow_md()
                .p(px(4.))
                .occlude();
            for (label, index) in options {
                let active = selected == Some(index);
                let on_change = on_change.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("{key}-opt-{index}")))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.open_dd = None;
                            on_change(this, index, window, cx);
                            cx.notify();
                        }))
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(p.text)
                        .hover(|s| s.bg(p.hover))
                        .when(active, |el| el.bg(p.accent_weak))
                        .child(SharedString::from(label))
                        .when(active, |el| {
                            el.child(icon("check").size(px(14.)).text_color(p.accent))
                        }),
                );
            }
            container = container.child(gpui_kit::deferred(list).with_priority(1));
        }
        container
    }

    // ------------------------------------------------------------------
    // USB 调试设备扫描
    // ------------------------------------------------------------------

    pub fn usb_discovery(&self, hdc: bool) -> &UsbDiscovery {
        &self.usb_discovery[usize::from(hdc)]
    }

    pub(crate) fn test_flash_preset(&mut self, id: &str, cx: &mut Context<Self>) {
        if matches!(
            self.flash_preset_tests.get(id),
            Some(FlashPresetTest::Running)
        ) {
            return;
        }
        let Some(preset) = self
            .settings
            .flash_tools
            .iter()
            .find(|preset| preset.id == id)
        else {
            return;
        };
        let preset = preset.clone();
        self.flash_preset_tests
            .insert(id.to_owned(), FlashPresetTest::Running);
        let id = preset.id;
        let probe = cx.background_executor().spawn(async move {
            duthub_flash::FlashTester::real().run_preset_test(&preset.executable, &preset.test_args)
        });
        cx.spawn(async move |view, cx| {
            let result = probe.await;
            let _ = view.update(cx, |this, cx| {
                this.flash_preset_tests
                    .insert(id, FlashPresetTest::Finished(result));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn set_dut_flash_preset(&mut self, dut_id: &str, preset_id: &str) {
        if let Some(dut) = self.duts.connection_mut(dut_id) {
            dut.flash = crate::data::selected_flash_config(
                Some(&dut.flash),
                Some(preset_id),
                &self.settings.flash_tools,
            );
            self.duts.mark_connection_dirty();
        }
    }

    pub(crate) fn save_usb_tools(&mut self) {
        let message = match store::save_settings_atomic(&self.data_dir, &self.settings) {
            Ok(()) => crate::i18n::label("debugToolsSaved").to_owned(),
            Err(err) => format!("{} {err}", crate::i18n::label("saveFailed")),
        };
        self.notify_toast(message);
    }

    fn browse_path(&mut self, id: In, directory: bool, cx: &mut Context<Self>) {
        let Some(editor) = self.text_editors.get(&id) else {
            return;
        };
        let state = editor.state.clone();
        let scope = editor.scope.clone();
        let selection = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: !directory,
            directories: directory,
            multiple: false,
            prompt: Some(i18n::label("selectPath").into()),
        });
        cx.spawn(async move |view, cx| match selection.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.first() {
                    let _ = view.update(cx, |this, cx| {
                        if this.editor_scope(id) == scope.as_deref()
                            && this
                                .text_editors
                                .get(&id)
                                .is_some_and(|editor| editor.state == state)
                        {
                            this.input_set(id, path.to_string_lossy().into_owned());
                            cx.notify();
                        }
                    });
                }
            }
            Ok(Err(err)) => {
                let _ = view.update(cx, |this, cx| {
                    this.notify_toast(format!("{} {err}", i18n::label("browsePathFailed")));
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }

    pub(crate) fn select_usb_debug_tool(&mut self, tool: Option<&str>) {
        let Some(dut) = self.duts.selected_mut() else {
            return;
        };
        match tool {
            Some(tool) => {
                if !usb_debug_enabled(&dut.adb) || dut.adb.tool != tool {
                    dut.adb.tool = tool.into();
                    dut.adb.mode = "usb".into();
                    dut.adb.serial.clear();
                    self.duts.mark_connection_dirty();
                }
                let hdc = tool == "hdc";
                self.usb_debug_view = Some(hdc);
                self.scan_usb_devices(hdc);
                self.open_dd = Some(Dd::AdbSerial.key().into());
            }
            None => {
                dut.adb.mode = "disabled".into();
                dut.adb.serial.clear();
                self.duts.mark_connection_dirty();
            }
        }
    }

    pub(crate) fn show_dut_connections(&mut self, dut_id: &str) {
        if let Some(index) = self.duts.file.duts.iter().position(|dut| dut.id == dut_id) {
            self.select_connection(index);
            self.connection_open = true;
        }
    }

    pub(crate) fn select_connection(&mut self, index: usize) {
        self.duts.editing_new_connection = false;
        self.duts.selected = index;
        self.open_dd = None;
    }

    pub(crate) fn set_config_page(&mut self, page: Option<ConfigPage>) {
        self.overlay = page;
        self.open_dd = None;
    }

    fn pane_widths(&self, viewport: Pixels) -> (Pixels, Pixels) {
        let connection = self.connection_open.then(|| {
            self.connection_pane_width
                .unwrap_or_else(|| (viewport * 0.4).clamp(px(300.), px(520.)))
        });
        let tools =
            (self.view == ViewMode::Serial && self.side_panel.visible.is_some()).then(|| {
                self.tools_pane_width
                    .unwrap_or_else(|| (viewport * 0.42).clamp(px(260.), px(440.)))
            });
        workspace_pane_widths(viewport, connection, tools)
    }

    fn drag_pane(&mut self, event: &DragMoveEvent<PaneDivider>, cx: &mut Context<Self>) {
        let viewport = event.bounds.size.width;
        let (left, right) = self.pane_widths(viewport);
        let x = event.event.position.x - event.bounds.origin.x;
        let divider = *event.drag(cx);
        match divider {
            PaneDivider::Connection => {
                let next = x.min(viewport - CENTER_PANE_MIN_WIDTH - right);
                self.connection_pane_width = Some(next);
                self.connection_pane_width = Some(self.pane_widths(viewport).0);
            }
            PaneDivider::Tools => {
                let next = (viewport - x).min(viewport - CENTER_PANE_MIN_WIDTH - left);
                self.tools_pane_width = Some(next);
                self.tools_pane_width = Some(self.pane_widths(viewport).1);
            }
        }
        cx.notify();
    }

    pub(crate) fn begin_new_connection(
        &mut self,
        connect_after_save: bool,
        cx: &mut Context<Self>,
    ) {
        if self.duts.new_connection.is_none() {
            let name = format!("DUT-{}", self.duts.file.duts.len() + 1);
            let mut dut = demo_dut_template(&name, "");
            dut.id = format!("dut-{}", uuid::Uuid::new_v4());
            dut.mcp = duthub_core::model::McpConfig::StreamableHttp {
                url: format!(
                    "http://127.0.0.1:{}/mcp",
                    self.settings.agent_access.http_port
                ),
            };
            self.duts.new_connection = Some(crate::data::NewConnection {
                dut,
                alerts: None,
                connect_after_save,
            });
        }
        self.show_new_connection();
        self.serial_ports = self.device.list_ports();
        cx.notify();
    }

    pub(crate) fn show_new_connection(&mut self) {
        self.duts.editing_new_connection = true;
        self.connection_open = true;
        self.open_dd = None;
    }

    pub(crate) fn cancel_new_connection(&mut self) {
        if let Some(draft) = self.duts.new_connection.take() {
            self.heartbeat_drafts.remove(&draft.dut.id);
        }
        self.duts.editing_new_connection = false;
        self.open_dd = None;
    }

    fn persist_new_connection(&mut self) -> Result<(usize, bool), String> {
        let draft = self
            .duts
            .new_connection
            .as_ref()
            .ok_or_else(|| crate::i18n::label("connectionMissing").to_owned())?;
        let mut dut = draft.config_to_save().map_err(crate::i18n::label)?;
        let connect = draft.connect_after_save;
        let alert_settings = draft.alerts.clone();
        let heartbeat = HashMap::from([(dut.id.clone(), self.heartbeat_config(&dut.id))]);
        self.heartbeat.commit_configs(&heartbeat, |configs| {
            let mut file = store::load_duts(&self.data_dir)
                .map_err(|err| err.to_string())?
                .unwrap_or_else(|| DutsState::empty().file);
            file.duts.push(dut.clone());
            if let Some(alerts) = &alert_settings {
                file.alerts_by_dut.insert(dut.id.clone(), alerts.clone());
            }
            Self::validate_bindings(&mut file)?;
            Self::apply_heartbeat_configs(&mut file, configs);
            store::save_duts_atomic(&self.data_dir, &file).map_err(|err| err.to_string())?;
            dut = file.duts.pop().expect("the new connection was appended");
            Ok(())
        })?;
        if let Some(alerts) = alert_settings {
            self.duts.file.alerts_by_dut.insert(dut.id.clone(), alerts);
            self.alerts
                .lock()
                .unwrap()
                .set_dut_settings(&self.duts.file.alerts_by_dut);
        }
        let mut resolved = dut.clone();
        resolved.relay = self.duts.file.resolve_relay(&dut.relay);
        self.control.register_dut(resolved);
        self.duts.add_saved_connection(dut);
        self.cancel_new_connection();
        let index = self.duts.file.duts.len() - 1;
        self.select_connection(index);
        Ok((index, connect))
    }

    pub(crate) fn save_connection(&mut self, cx: &mut Context<Self>) {
        if !self.duts.editing_new_connection {
            self.save_duts();
        } else {
            match self.persist_new_connection() {
                Ok((index, true)) => {
                    self.connection_open = false;
                    self.connect_dut(index, cx);
                }
                Ok((_, false)) => self.notify_toast(crate::i18n::label("saveBinding")),
                Err(err) => {
                    self.notify_toast(format!("{} {err}", crate::i18n::label("saveFailed")))
                }
            }
        }
        cx.notify();
    }

    pub fn scan_usb_devices(&mut self, hdc: bool) {
        let (kind, configured, fallback) = if hdc {
            (JobKind::Hdc, &self.settings.tools.hdc_executable, "hdc")
        } else {
            (JobKind::Adb, &self.settings.tools.adb_executable, "adb")
        };
        let exe = if configured.trim().is_empty() {
            fallback
        } else {
            configured.trim()
        }
        .to_owned();
        let job = self.next_job;
        self.next_job += 1;
        self.usb_discovery[usize::from(hdc)] = UsbDiscovery { job, result: None };
        let client = self.adb_client.clone();
        let queue = self.job_queue.clone();
        cx_spawn_job(move || {
            let outcome = if hdc {
                duthub_adb::HdcClient::real()
                    .list_targets(&exe)
                    .map(|targets| {
                        targets
                            .into_iter()
                            .map(|target| AdbDevice {
                                serial: target.key,
                                state: target.state,
                                model: None,
                            })
                            .collect()
                    })
            } else {
                client.devices_l(&exe)
            }
            .map_err(|err| err.to_string());
            queue.lock().unwrap().push(JobResult { job, kind, outcome });
        });
    }

    pub fn press_relay(
        &mut self,
        dut_id: &str,
        binding: &'static str,
        channel: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.relay_press.is_some() {
            return;
        }
        let Some(mut target) = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| dut.id == dut_id)
            .cloned()
        else {
            return;
        };
        target.relay = self.duts.file.resolve_relay(&target.relay);
        let (release, released) = tokio::sync::oneshot::channel();
        let control = self.control.clone();
        let worker = std::thread::spawn(move || {
            control
                .hold_channel(&target, channel, released)
                .map_err(|error| error.to_string())
        });
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.release_relay(cx);
            }
        });
        self.relay_press = Some(RelayPress {
            dut_id: dut_id.into(),
            binding,
            channel,
            release: Some(release),
            worker: Some(worker),
            _activation: activation,
        });
        cx.notify();
    }

    pub fn release_relay(&mut self, cx: &mut Context<Self>) {
        if let Some(press) = self.relay_press.as_mut()
            && press.release.take().is_some()
        {
            cx.notify();
        }
    }

    fn poll_relay_press(&mut self) -> bool {
        if !self
            .relay_press
            .as_ref()
            .and_then(|press| press.worker.as_ref())
            .is_some_and(|worker| worker.is_finished())
        {
            return false;
        }
        let mut press = self.relay_press.take().expect("checked worker");
        let worker = press.worker.take().expect("checked worker");
        let result = worker
            .join()
            .unwrap_or_else(|_| Err(crate::i18n::label("relayWorkerFailed").into()));
        if let Err(err) = result {
            let name = self
                .duts
                .file
                .duts
                .iter()
                .find(|dut| dut.id == press.dut_id)
                .map_or(press.dut_id.as_str(), |dut| dut.name.as_str());
            let action = format!("{} CH{}", crate::i18n::label("relayHold"), press.channel);
            let (title, detail) = crate::alerts::control_action_alert_texts(name, &action, &err);
            self.alerts.lock().unwrap().report_for(
                &press.dut_id,
                AlertSource::Control,
                title,
                detail,
            );
            self.notify_toast(err);
        }
        true
    }

    /// 把当前 DUT 摘要 + 会话日志推入共享快照（MCP HTTP/stdio 数据源）；
    /// 同时把 DUT 配置注册到共用设备操作入口（控制动作据此校验绑定与解析地址）。
    fn refresh_snapshot(&mut self) {
        if !self.duts.dirty {
            for dut in &self.duts.file.duts {
                let mut resolved = dut.clone();
                resolved.relay = self.duts.file.resolve_relay(&dut.relay);
                self.control.register_dut(resolved);
            }
        }
        let mut snap = self.snapshot_ref.lock().unwrap();
        snap.version = 1;
        snap.media_bindings = self
            .duts
            .file
            .duts
            .iter()
            .filter(|dut| {
                crate::media::Source::ALL.into_iter().any(|source| {
                    crate::media_page::saved_capture(&self.duts, &dut.id, source).is_ok()
                })
            })
            .map(|dut| dut.id.clone())
            .collect();
        snap.ts_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        snap.duts = self
            .duts
            .file
            .duts
            .iter()
            .map(|d| {
                let source = self
                    .device
                    .session_source(&d.id)
                    .unwrap_or("idle")
                    .to_string();
                let session = format!("sess-{}", d.id);
                // 日志窗口必须取缓冲「最新」150 条：read_log 自队头（最旧）起截取，
                // 直接传 limit 会永远停在最早段，stdio 侧 serial_read 的 seq 游标
                // （seq > 窗口末尾）续读恒为空、会话超过 150 行后数据冻结。
                let envelopes = self.device.read_log(&session, 0, usize::MAX);
                let skip = envelopes.len().saturating_sub(150);
                let lines = envelopes
                    .into_iter()
                    .skip(skip)
                    .filter_map(|e| match e.event {
                        duthub_core::ipc::UiEvent::SerialChunk { text, .. } => {
                            Some(duthub_mcp::snapshot::SnapshotLine {
                                seq: e.seq,
                                text: text.to_string(),
                            })
                        }
                        _ => None,
                    })
                    .collect();
                duthub_mcp::snapshot::SnapshotDut {
                    dut_id: d.id.clone(),
                    name: self.duts.saved_name(&d.id).unwrap_or(&d.name).to_owned(),
                    source,
                    serial: duthub_mcp::state::SerialBindInfo {
                        host: d.serial.host.clone(),
                        port: d.serial.port.clone(),
                        baud: d.serial.baud,
                        format: d.serial.format.clone(),
                    },
                    adb_serial: self
                        .duts
                        .saved_usb(&d.id)
                        .filter(|adb| adb.tool != "hdc" && !adb.serial.trim().is_empty())
                        .map(|adb| adb.serial.clone()),
                    relay: None,
                    mcp_label: None,
                    lines,
                }
            })
            .collect();
    }

    /// 取回模型探针结果 → Agent 状态 + Toast
    fn drain_model_test(&mut self) -> bool {
        let taken = crate::agent::MODEL_TEST_RESULT.lock().unwrap().take();
        let Some((model_id, outcome)) = taken else {
            return false;
        };
        let msg = match outcome {
            Ok((latency, reply)) => format!(
                "{model_id} 连接成功 · {latency} ms · {}",
                reply.chars().take(60).collect::<String>()
            ),
            Err(err) => format!("{model_id} 测试失败: {err}"),
        };
        self.agent.model_testing = false;
        self.agent.model_status = Some(msg.clone());
        self.notify_toast(msg);
        true
    }

    fn drain_jobs(&mut self) -> bool {
        let drained: Vec<JobResult> = std::mem::take(&mut *self.job_queue.lock().unwrap());
        if drained.is_empty() {
            return false;
        }
        for result in drained {
            self.apply_job_result(result);
        }
        true
    }

    /// 只接收对应扫描的最新结果。
    fn apply_job_result(&mut self, result: JobResult) {
        let scan = &mut self.usb_discovery[usize::from(result.kind == JobKind::Hdc)];
        if result.job == scan.job {
            scan.result = Some(result.outcome);
        }
    }

    // ------------------------------------------------------------------
    // 设备事件吸收
    // ------------------------------------------------------------------

    fn ensure_session(&mut self, session_id: &str, dut_id: &str) -> usize {
        if let Some(index) = self
            .sessions
            .iter()
            .position(|s| s.session_id == session_id)
        {
            return index;
        }
        let source = self.device.session_source(dut_id).unwrap_or("device");
        self.sessions.push(SessionView::new(
            dut_id.to_string(),
            session_id.to_string(),
            format!("{dut_id} · {source}"),
            source,
            0,
        ));
        // 新会话需要一次 term.resize（网格尺寸缓存按会话重置）
        self.term_grid = None;
        self.sessions.len() - 1
    }

    fn absorb_envelope(&mut self, env: duthub_core::ipc::EventEnvelope) {
        let Some(session_id) = env.session_id.clone() else {
            return;
        };
        let Some(dut_id) = env.dut_id.clone() else {
            return;
        };
        if !self
            .sessions
            .iter()
            .any(|view| view.session_id == session_id)
            && self.device.session_source(&dut_id).is_none()
        {
            return;
        }
        let index = self.ensure_session(&session_id, &dut_id);
        let view = &mut self.sessions[index];
        // 首开失败 payload（需求⑤接入点②）：view 借用期间先记下，出借后再入队
        let mut serial_open_failure: Option<(String, String)> = None;
        match &env.event {
            UiEvent::SerialChunk { text, style } => {
                let kind = match style {
                    ChunkStyle::Normal => LineKind::Normal,
                    ChunkStyle::Dim => LineKind::Dim,
                    ChunkStyle::Good => LineKind::Good,
                    ChunkStyle::Prompt => LineKind::Prompt,
                    ChunkStyle::Error => LineKind::Error,
                    ChunkStyle::Mark => LineKind::Mark,
                };
                if self.rec_on {
                    self.rec_buffer.push(text.clone().to_string());
                }
                let opened_baud = if view.source == "device" && *style == ChunkStyle::Dim {
                    parse_serial_open_baud(text)
                } else {
                    None
                };
                // 链路状态推导（仅 device 会话）：驱动"[serial]"行即状态信令
                if view.source == "device" {
                    if *style == ChunkStyle::Normal {
                        view.crash_line_tail.push_str(text);
                        if let Some(end) = view.crash_line_tail.rfind('\n') {
                            let remainder = view.crash_line_tail.split_off(end + 1);
                            if let Some(line) = agent::serial_crash_evidence(&view.crash_line_tail)
                            {
                                self.alerts.lock().unwrap().report_for(
                                    &view.dut_id,
                                    AlertSource::Agent,
                                    crate::i18n::label("serialCrash"),
                                    format!("{}\n{line}", view.title),
                                );
                            }
                            view.crash_line_tail = remainder;
                        }
                        if view.crash_line_tail.len() > 8192 {
                            view.crash_line_tail.clear();
                        }
                    }
                    if let Some(baud) = opened_baud {
                        view.link = LinkState::Connected;
                        view.reconnect_attempts = 0;
                        view.baud = baud;
                    } else if text.starts_with("[serial] ") && text.contains("重连") {
                        view.link = LinkState::Reconnecting;
                        view.reconnect_attempts += 1;
                    } else if let Some(target) = crate::alerts::parse_serial_open_failure(text) {
                        // 首开失败：驱动只报错一次并结束会话 task（坑 #7），
                        // 即「task 死亡」——转弹窗提醒（需求⑤接入点②）。
                        // view 借用仍在，payload 先记下、出 match 后再入队。
                        serial_open_failure = Some((target, text.clone()));
                    }
                }
                // device 会话（PuTTY 模式）：设备输出由仿真器网格（SerialScreen）
                // 渲染，SerialChunk 只收注记行（[serial] 信令 / [mark] / 错误）；
                // demo 会话保持原有全量行渲染。
                let annotation =
                    !matches!(kind, LineKind::Normal | LineKind::Good | LineKind::Prompt);
                if opened_baud.is_none() && (view.source != "device" || annotation) {
                    view.lines.push(LogLine {
                        text: SharedString::from(text.clone()),
                        kind,
                    });
                }
                view.last_seq = env.seq;
                self.core_lines.fetch_add(1, Ordering::Relaxed);
            }
            UiEvent::SerialScreen { screen } => {
                // real 会话：整屏快照替换（渲染源唯一归属仿真器网格）
                if view.source == "device" {
                    view.screen = Some(screen.clone());
                    view.last_seq = env.seq;
                }
            }
            UiEvent::SerialDisconnected { reason } => {
                // device 会话断线后驱动自动进入退避重连 → 状态条切"重连中"
                if view.source == "device" {
                    view.crash_line_tail.clear();
                    view.link = LinkState::Reconnecting;
                }
                view.lines.push(LogLine {
                    text: SharedString::from(format!("[会话已断开: {reason}]")),
                    kind: LineKind::Dim,
                });
                view.last_seq = env.seq;
            }
            _ => {}
        }
        if self.sessions[index].lines.len() > 5000 {
            let drain = self.sessions[index].lines.len() - 4000;
            self.sessions[index].lines.drain(..drain);
        }
        // 首开失败 → 弹窗提醒中心（去重窗口兜底重连风暴）
        if let Some((target, line)) = serial_open_failure {
            let dut_label = self
                .duts
                .file
                .duts
                .iter()
                .find(|d| d.id == dut_id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| dut_id.clone());
            self.report_serial_session_failure(
                &dut_id,
                &dut_label,
                &crate::i18n::message("alertSerialOpenFailed", &[("target", &target)]),
                &line,
            );
        }
    }

    /// 接入点②（serial）：会话首开失败 / task 死亡 → 弹窗提醒中心。
    /// detail 携带驱动原文（端口/波特率/失败原因），便于人工定位。
    fn report_serial_session_failure(
        &mut self,
        dut_id: &str,
        dut_label: &str,
        title: &str,
        detail: &str,
    ) -> bool {
        self.alerts.lock().unwrap().report_for(
            dut_id,
            AlertSource::Serial,
            title.to_string(),
            crate::i18n::message(
                "alertSerialDetail",
                &[("dut", dut_label), ("detail", detail)],
            ),
        )
    }

    fn absorb_device_events(
        &mut self,
        first: Result<duthub_core::ipc::EventEnvelope, broadcast::error::TryRecvError>,
        rx: &mut broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
    ) -> usize {
        let _span = tracing::info_span!("serial.ui_apply").entered();
        let mut handled = 0usize;
        let events = std::iter::once(first).chain(std::iter::from_fn(|| Some(rx.try_recv())));
        for event in events.take(64) {
            match event {
                Ok(env) => {
                    self.absorb_envelope(env);
                    handled += 1;
                }
                Err(broadcast::error::TryRecvError::Lagged(lagged)) => {
                    let views: Vec<(String, u64)> = self
                        .sessions
                        .iter()
                        .map(|s| (s.session_id.clone(), s.last_seq))
                        .collect();
                    for (session_id, since) in views {
                        for env in self.device.read_log(&session_id, since, lagged as usize) {
                            self.absorb_envelope(env);
                            handled += 1;
                            if handled >= 256 {
                                break;
                            }
                        }
                    }
                    break;
                }
                Err(_) => break,
            }
        }
        handled
    }

    // ------------------------------------------------------------------
    // 会话心跳（需求⑦）：定时驱动 + 告警 drain
    // ------------------------------------------------------------------

    /// 心跳引擎单次驱动（50ms tick 调用）：到期下发命令、窗口内收响应、
    /// 丢联达阈值发告警（toast 提示 + 转弹窗提醒中心，需求⑤接入点①）。
    /// 返回是否有状态变化（供渲染循环决定重绘）。
    pub fn heartbeat_tick(&mut self) -> bool {
        let changed = self.heartbeat.tick();
        for alert in self.heartbeat.drain_alerts() {
            self.notify_toast(format!("⚠ {}", alert.message));
            let label = self
                .duts
                .file
                .duts
                .iter()
                .find(|d| d.id == alert.dut_id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| alert.dut_id.clone());
            let (title, detail) = crate::alerts::heartbeat_alert_texts(&label, &alert.message);
            self.alerts.lock().unwrap().report_for(
                &alert.dut_id,
                AlertSource::Heartbeat,
                title,
                detail,
            );
        }
        changed
    }

    /// Agent 面板推进（50ms tick / 渲染帧共用）：推进一步并在会话由「运行中」
    /// 进入 Done / Stopped 时产出初步分析提醒（需求④接入点，findings 计数）。
    /// 返回是否有状态变化（供渲染循环决定重绘）。
    pub fn agent_tick(&mut self) -> bool {
        let alerts = &self.alerts;
        self.agent.tick(|dut_id, session| {
            if let Some((title, detail)) = crate::alerts::agent_transition_alert_texts(
                RunStatus::Running,
                session.status,
                session.workflow.zh(),
                session.findings.len(),
            ) {
                alerts
                    .lock()
                    .unwrap()
                    .report_for(dut_id, AlertSource::Agent, title, detail);
            }
        })
    }

    // ------------------------------------------------------------------
    // 终端输入
    // ------------------------------------------------------------------

    /// demo 会话输入行（旧行为：本地缓冲 + [TX] 回显演示）。
    fn send_input(&mut self, cx: &mut Context<Self>) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        if text == "clear" {
            if let Some(view) = self.sessions.get_mut(self.active_session) {
                view.lines.clear();
            }
        } else {
            let Some(view) = self.sessions.get(self.active_session) else {
                return;
            };
            let dut_id = view.dut_id.clone();
            if let Err(err) =
                self.device
                    .write_line(&dut_id, duthub_core::lease::WriteIdentity::gui(), &text)
                && let Some(view) = self.sessions.get_mut(self.active_session)
            {
                view.lines.push(LogLine {
                    text: SharedString::from(format!("[写入失败: {err}]")),
                    kind: LineKind::Error,
                });
            }
        }
        self.input.clear();
        cx.notify();
    }

    /// real 会话键盘路径（PuTTY 行为）：字节原样下发，无行尾追加、无本地回显
    /// （回显/提示符由设备 shell 经仿真器渲染）。失败转注记行。
    #[tracing::instrument(name = "serial.key_enqueue", skip_all, fields(bytes = bytes.len()))]
    fn send_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(view) = self.sessions.get(self.active_session) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        if let Err(err) =
            self.device
                .write_bytes(&dut_id, duthub_core::lease::WriteIdentity::gui(), bytes)
            && let Some(view) = self.sessions.get_mut(self.active_session)
        {
            view.lines.push(LogLine {
                text: SharedString::from(format!("[写入失败: {err}]")),
                kind: LineKind::Error,
            });
            cx.notify();
        }
    }

    fn paste_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self
            .sessions
            .get(self.active_session)
            .filter(|view| view.source == "device")
        else {
            return;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let mut text = text.replace("\r\n", "\r").replace('\n', "\r");
        text.retain(|ch| !ch.is_control() || matches!(ch, '\r' | '\t'));
        if text.is_empty() {
            return;
        }
        if view
            .screen
            .as_ref()
            .is_some_and(|screen| screen.bracketed_paste)
        {
            text = format!("\x1b[200~{text}\x1b[201~");
        }
        self.send_bytes(text.as_bytes(), cx);
    }

    // ------------------------------------------------------------------
    // 会话状态条动作（H1.2 换行 / H1.3 波特率热切换 / H1.4 手动重连）
    // ------------------------------------------------------------------

    /// 波特率热切换（SetBaud 关闭重开路径）：输入草稿为空时用当前确认值。
    /// 确认以"[serial] … @ N 已打开"行回填 view.baud 为准（状态条实时显示）。
    fn apply_session_baud(&mut self) {
        let Some(view) = self.sessions.get(self.active_session) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        let draft = view.baud_draft.clone().unwrap_or_default();
        let current = view.baud;
        let text = if draft.is_empty() {
            if current == 0 {
                String::new()
            } else {
                current.to_string()
            }
        } else {
            draft
        };
        match text.parse::<u32>() {
            Ok(b) if (1..=10_000_000).contains(&b) => match self.device.set_baud(&dut_id, b) {
                Ok(()) => {
                    if let Some(view) = self.sessions.get_mut(self.active_session) {
                        view.baud_draft = None;
                    }
                    self.notify_toast(format!("波特率切换请求已发送 → {b}（关闭重开生效）"));
                }
                Err(err) => self.notify_toast(format!("波特率切换失败: {err}")),
            },
            _ => self.notify_toast("波特率无效（1–10000000）"),
        }
    }

    fn reset_serial_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.sessions.get_mut(self.active_session) else {
            return;
        };
        if view.source == "device"
            && let Err(error) = self.device.reset_terminal(&view.dut_id)
        {
            self.notify_toast(format!("{} {error}", i18n::label("resetTerminalFailed")));
            cx.notify();
            return;
        }
        view.lines.clear();
        self.terminal_scroll.scroll_to_bottom();
        window.focus(&self.input_focus, cx);
        cx.notify();
    }

    /// 手动重连：唤醒退避等待立即重试（连接正常时为无害唤醒）。
    fn request_manual_reconnect(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.sessions.get(self.active_session) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        let reconnecting = view.link == LinkState::Reconnecting;
        match self.device.manual_reconnect(&dut_id) {
            Ok(()) => {
                self.notify_toast(if reconnecting {
                    "已请求立即重连（跳过本次退避等待）"
                } else {
                    "重连请求已受理（当前链路正常，唤醒在下次断线时生效）"
                });
            }
            Err(err) => self.notify_toast(format!("重连请求失败: {err}")),
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 渲染
    // ------------------------------------------------------------------

    pub fn render_root(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _frame = tracing::info_span!(
            "ui.frame",
            frames = self.frames.load(std::sync::atomic::Ordering::Relaxed)
        )
        .entered();
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.sync_serial_agent();
        let usb_debug_view = if self.connection_open {
            self.duts
                .selected()
                .filter(|dut| usb_debug_enabled(&dut.adb))
                .map(|dut| dut.adb.tool == "hdc")
        } else {
            None
        };
        if self.usb_debug_view != usb_debug_view {
            self.usb_debug_view = usb_debug_view;
            if let Some(hdc) = usb_debug_view {
                self.scan_usb_devices(hdc);
            }
        }
        if self.relay_press.as_ref().is_some_and(|press| {
            !self.connection_open
                || self.overlay.is_some()
                || !window.is_window_active()
                || self
                    .duts
                    .selected()
                    .is_none_or(|dut| dut.id != press.dut_id)
        }) {
            self.release_relay(cx);
        }
        self.drain_jobs();
        self.agent_tick();
        self.media.reconcile_bindings(&self.duts);
        self.sync_monitor_panel(window);
        self.refresh_snapshot();
        let p = self.palette();
        self.p = p;

        let panel_open = self.side_panel.visible.is_some();
        let (connection_width, side_width) = self.pane_widths(window.viewport_size().width);
        let mut split = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .on_drag_move(cx.listener(|this, event, _, cx| this.drag_pane(event, cx)))
            .when(self.connection_open, |el| {
                el.child(self.render_connection_dock(connection_width, p, window, cx))
            })
            .when(self.view == ViewMode::Home, |el| {
                el.child(self.render_home(p, cx))
            })
            .when(self.view == ViewMode::Serial, |el| {
                el.child(self.render_serial_page(window, cx))
            });
        if panel_open && self.view == ViewMode::Serial {
            split = split.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(side_width)
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .child(self.render_side_content(side_width, p, window, cx))
                    .child(PaneDivider::Tools.handle()),
            );
        } else if self.side_panel.visible.is_some() {
            self.close_side_panel(window);
        }
        let mut bottom = div()
            .flex()
            .items_center()
            .h(px(40.))
            .flex_none()
            .px(px(8.))
            .gap(px(8.))
            .bg(p.rail)
            .border_t_1()
            .border_color(p.line)
            .child(
                icon_btn("bottom-connection", "link", p)
                    .flex_none()
                    .when(self.connection_open, |button| button.bg(p.raised))
                    .tooltip(crate::i18n::label("connectionTitle"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.menu_open = false;
                        this.connection_open = !this.connection_open;
                        this.open_dd = None;
                        cx.notify();
                    })),
            );
        bottom = if self.show_command_dock() {
            bottom.child(self.render_command_dock(p, cx))
        } else {
            bottom.child(div().flex_1())
        };
        bottom = bottom.child(self.render_side_strip(p, cx));

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .font_family(theme::UI_FONT)
            .text_color(p.text)
            .bg(p.bg)
            .relative()
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.release_relay(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.release_relay(cx)),
            )
            // 下拉打开时：底层透明捕获层命中“空白处”点击 → 关闭下拉
            // （下拉列表自身 occlude 在上层，内部交互不受影响）
            .when(self.open_dd.is_some(), |el| {
                el.child(
                    div()
                        .id("dd-backdrop")
                        .absolute()
                        .inset_0()
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            this.open_dd = None;
                            cx.notify();
                        })),
                )
            })
            .child(self.render_titlebar(p, window, cx))
            .child(split)
            .child(bottom);

        // 浮层：菜单 / 配置壳 / 弹窗 / Toast / 下拉关闭背景
        if self.menu_open {
            // 空白捕获层：点菜单以外任意区域关闭（菜单面板自身 occlude 挡住本层）
            root = root.child(
                div()
                    .id("menu-backdrop")
                    .absolute()
                    .inset_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _win, cx| {
                            this.menu_open = false;
                            cx.notify();
                        }),
                    ),
            );
            root = root.child(self.render_workspace_menu(p, cx));
        }
        if let Some((ctx_x, ctx_y)) = self.terminal_ctx {
            root = root.child(self.render_terminal_ctx_menu(ctx_x, ctx_y, p, cx));
        }
        if self.overlay != Some(ConfigPage::AgentModel) {
            self.agent_token_input = None;
            self.provider_models.close_editor();
        }
        if let Some(page) = self.overlay {
            root = root.child(self.render_config_overlay(page, window, cx));
        }
        // 异常弹窗提醒调度（需求⑤）：仅在没有活动 Dialog 时弹出（不与表单弹窗
        // 叠加，避免 close_dialog 关错层）；「需要人工干预」弹窗禁用了 Esc/遮罩/
        // 右上角 X 关闭，只能经按钮收尾，alert_dialog_open 若因外部关闭残留
        //（如其他代码 close_all_dialogs）由本帧自愈复位。
        if self.alert_dialog_open && !window.has_active_dialog(cx) {
            self.alert_dialog_open = false;
        }
        if !self.alert_dialog_open && !window.has_active_dialog(cx) {
            let next = self.alerts.lock().unwrap().pop_next();
            if let Some(item) = next {
                self.alert_dialog_open = true;
                self.open_alert_dialog(item, window, cx);
            }
        }
        // kit Notification：渲染帧统一推送（Root 层管理生命周期/动画）
        for message in self.pending_toasts.drain(..) {
            window.push_notification(Notification::info(message), cx);
        }
        root
    }

    /// 打开「需要人工干预」弹窗（kit Dialog，需求⑤）：
    /// 标题固定，detail 展示来源与上下文；按钮 知道了 / 忽略此类（按 source
    /// 抑制并落盘）/ 打开面板（仅 Agent 来源）。浮层内 kit Button 渲染异常
    /// （坑 #2），按钮一律手绘 div；禁用 Esc/遮罩/X 关闭，关闭动作全部经按钮，
    /// 保证 alert_dialog_open 生命周期可控。
    fn open_alert_dialog(&mut self, item: AlertItem, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        let p = self.p;
        window.open_dialog(cx, move |dialog, _window, cx| {
            let (title, body) = view.update(cx, |w, cx| {
                (
                    crate::i18n::label("alertsTitle"),
                    w.render_alert_dialog_body(&item, p, cx),
                )
            });
            dialog
                .title(title)
                .overlay_closable(false)
                .keyboard(false)
                .close_button(false)
                .child(body)
        });
        cx.notify();
    }

    /// 「需要人工干预」弹窗正文：来源/时间 + 标题 + detail + 按钮排。
    fn render_alert_dialog_body(
        &mut self,
        item: &AlertItem,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let source = item.source;
        let dut_id = item.dut_id.clone();
        let at = crate::alerts::format_alert_time(item.at, crate::alerts::now_ms());
        let dialog_button = |id: &'static str, label: &'static str, primary: bool| {
            let mut btn = div()
                .id(SharedString::from(id))
                .flex()
                .items_center()
                .justify_center()
                .min_h(px(32.0))
                .px(px(14.0))
                .rounded(px(8.0))
                .text_size(px(13.0))
                .cursor_pointer();
            btn = if primary {
                btn.bg(p.accent)
                    .font_weight(gpui_kit::FontWeight(700.0))
                    .text_color(p.accent_contrast)
                    .hover(|s| s.opacity(0.9))
            } else {
                btn.bg(p.raised)
                    .border_1()
                    .border_color(p.border)
                    .text_color(p.text)
                    .hover(|s| s.bg(p.hover))
            };
            btn.child(label)
        };
        let mut body = div().flex().flex_col().gap(px(10.)).w_full();
        body = body
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(icon("alert").size(px(25.)).text_color(p.red))
                    .child(
                        div()
                            .px(px(7.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .bg(p.accent_weak)
                            .text_size(px(10.5))
                            .text_color(p.accent)
                            .child(crate::i18n::message(
                                "alertSourceLabel",
                                &[("source", source.label())],
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_family(theme::MONO_FONT)
                            .text_color(p.faint)
                            .child(SharedString::from(at)),
                    ),
            )
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(gpui_kit::FontWeight(650.0))
                    .text_color(p.text)
                    .child(SharedString::from(item.title.clone())),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(px(19.))
                    .text_color(p.muted)
                    .child(SharedString::from(item.detail.clone())),
            );
        // Agent 来源附「打开面板」：切到串口工作台并展开 Agent 面板
        let can_open_agent =
            source == AlertSource::Agent && self.sessions.iter().any(|view| view.dut_id == dut_id);
        let open_panel_btn = can_open_agent.then(|| {
            let target = dut_id.clone();
            dialog_button("alert-open-panel", crate::i18n::label("openPane"), false).on_click(
                cx.listener(move |this, _ev, window, cx| {
                    if let Some(index) = this.sessions.iter().position(|view| view.dut_id == target)
                    {
                        this.active_session = index;
                        this.term_grid = None;
                        this.open_side_panel(crate::side_panel::Panel::Agent, window, cx);
                    }
                    this.alert_dialog_open = false;
                    window.close_dialog(cx);
                    cx.notify();
                }),
            )
        });
        let mut row = div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(10.0))
            .mt(px(4.0));
        if let Some(btn) = open_panel_btn {
            row = row.child(btn);
        }
        row = row
            .child(
                dialog_button("alert-suppress", crate::i18n::label("muteAlertType"), false)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.edit_dut_alerts(&dut_id, |cfg| {
                            if !cfg.suppressed_sources.iter().any(|key| key == source.key()) {
                                cfg.suppressed_sources.push(source.key().into());
                            }
                        });
                        this.alert_dialog_open = false;
                        window.close_dialog(cx);
                        cx.notify();
                    })),
            )
            .child(
                dialog_button("alert-ack", crate::i18n::label("acknowledgeAlert"), true).on_click(
                    cx.listener(move |this, _ev, window, cx| {
                        this.alert_dialog_open = false;
                        window.close_dialog(cx);
                        cx.notify();
                    }),
                ),
            );
        body.child(row).into_any_element()
    }

    /// 首页：居中品牌 + 创建连接 / 设备工具 + 历史连接磁贴（两行五列，最多 10 个）。
    fn render_home(&mut self, p: &'static Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = div()
            .flex()
            .flex_col()
            .items_center()
            .mx_auto()
            .mt(px(56.0))
            .max_w(px(1240.0))
            .w_full()
            .px(px(24.0));

        // 品牌大图
        content = content.child(
            img("brand/app-256.png")
                .size(px(96.0))
                .rounded(px(22.0))
                .shadow_md(),
        );

        // 首页与标签栏共用连接表单，避免枚举顺序决定连接目标。
        content = content.child(
            div()
                .flex()
                .flex_row()
                .justify_center()
                .gap(px(18.0))
                .mt(px(44.0))
                .w_full()
                .child(
                    div()
                        .id("home-create-session")
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.begin_new_connection(true, cx);
                        }))
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(10.0))
                        .h(px(52.0))
                        .min_w(px(240.0))
                        .rounded(px(10.0))
                        .bg(gpui_kit::Rgba {
                            r: 17.0 / 255.0,
                            g: 61.0 / 255.0,
                            b: 60.0 / 255.0,
                            a: 1.0,
                        })
                        .border_1()
                        .border_color(p.accent)
                        .cursor_pointer()
                        .hover(|s| {
                            s.bg(gpui_kit::Rgba {
                                r: 23.0 / 255.0,
                                g: 79.0 / 255.0,
                                b: 77.0 / 255.0,
                                a: 1.0,
                            })
                        })
                        .child(icon("plus").size(px(16.0)).text_color(p.accent))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(gpui_kit::FontWeight(600.0))
                                .text_color(p.text)
                                .child(crate::i18n::label("createConnection")),
                        ),
                )
                .child(
                    div()
                        .id("home-devtools")
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            this.set_config_page(Some(ConfigPage::Flash));
                            cx.notify();
                        }))
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(10.0))
                        .h(px(52.0))
                        .min_w(px(240.0))
                        .rounded(px(10.0))
                        .bg(p.surface)
                        .border_1()
                        .border_color(p.border)
                        .cursor_pointer()
                        .hover(|s| s.border_color(p.accent).bg(p.hover))
                        .child(icon("maximize").size(px(16.0)).text_color(p.muted))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(gpui_kit::FontWeight(600.0))
                                .text_color(p.text)
                                .child(crate::i18n::label("deviceTools")),
                        ),
                ),
        );

        // 历史连接磁贴（两行五列；不足 10 个按实际数量显示，0 个不显示）
        let tiles: Vec<(usize, &duthub_core::model::DutConfig)> =
            self.duts.file.duts.iter().enumerate().take(10).collect();
        if !tiles.is_empty() {
            // 从中间向两边：flex wrap 居中，磁贴固定宽（每行最多 5 个）
            let mut grid = div()
                .flex()
                .flex_wrap()
                .justify_center()
                .gap(px(14.0))
                .mt(px(40.0))
                .max_w(px(1124.0))
                .w_full();
            for (index, dut) in tiles {
                let online = self
                    .sessions
                    .iter()
                    .any(|s| s.dut_id == dut.id && s.link == LinkState::Connected);
                let tool = dut.adb.tool.as_str();
                let name = dut.name.clone();
                let sub = format!("{} · {}", dut.serial.port, dut.serial.baud);
                let edit_id = dut.id.clone();
                grid = grid.child(
                    div()
                        .id(SharedString::from(format!("home-tile-{index}")))
                        .on_click(cx.listener(move |this, _ev, _win, cx| {
                            this.connect_dut(index, cx);
                        }))
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .min_h(px(116.0))
                        .px(px(14.0))
                        .py(px(12.0))
                        .rounded(px(10.0))
                        .bg(p.surface)
                        .border_1()
                        .border_color(p.border)
                        .cursor_pointer()
                        .hover(|s| s.border_color(p.accent).bg(p.hover))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(28.0))
                                        .rounded(px(7.0))
                                        .bg(p.raised)
                                        .child(
                                            icon("terminal").size(px(14.0)).text_color(p.accent),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_size(px(13.0))
                                        .font_weight(gpui_kit::FontWeight(650.0))
                                        .text_color(p.text)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("home-tile-edit-{index}")))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this, _ev: &MouseDownEvent, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.show_dut_connections(&edit_id);
                                                    cx.notify();
                                                },
                                            ),
                                        )
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(18.0))
                                        .rounded(px(5.0))
                                        .text_size(px(14.0))
                                        .text_color(p.faint)
                                        .hover(|s| s.text_color(p.text).bg(p.hover))
                                        .child("⋯"),
                                )
                                .child(
                                    crate::components::icon_btn(
                                        SharedString::from(format!("home-tile-delete-{index}")),
                                        "trash",
                                        p,
                                    )
                                    .w(px(24.0))
                                    .h(px(24.0))
                                    .tooltip(crate::i18n::label("deleteConnection"))
                                    .on_click(cx.listener(
                                        move |this, _ev, window, cx| {
                                            cx.stop_propagation();
                                            this.open_remove_dut(index, window, cx);
                                        },
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_family(theme::MONO_FONT)
                                .text_color(p.muted)
                                .overflow_hidden()
                                .child(SharedString::from(sub)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(4.0))
                                        .px(px(7.0))
                                        .py(px(2.0))
                                        .rounded(px(5.0))
                                        .bg(if online { p.accent_weak } else { p.raised })
                                        .child(div().size(px(5.0)).rounded_full().bg(if online {
                                            p.accent
                                        } else {
                                            p.faint
                                        }))
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .text_color(if online { p.accent } else { p.muted })
                                                .child(if online {
                                                    crate::i18n::label("online")
                                                } else {
                                                    crate::i18n::label("offline")
                                                }),
                                        ),
                                )
                                .when(tool == "adb", |el| {
                                    el.child(
                                        div()
                                            .px(px(7.0))
                                            .py(px(2.0))
                                            .rounded(px(5.0))
                                            .bg(p.raised)
                                            .text_size(px(10.5))
                                            .text_color(p.blue)
                                            .child("ADB"),
                                    )
                                })
                                .when(tool == "hdc", |el| {
                                    el.child(
                                        div()
                                            .px(px(7.0))
                                            .py(px(2.0))
                                            .rounded(px(5.0))
                                            .bg(p.raised)
                                            .text_size(px(10.5))
                                            .text_color(p.blue)
                                            .child("HDC"),
                                    )
                                }),
                        ),
                );
            }
            content = content.child(grid);
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(content)
    }

    fn render_serial_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.p;
        let active = self.active_session;
        if self.terminal_focus_pending && self.overlay.is_none() && !window.has_active_dialog(cx) {
            self.terminal_focus_pending = false;
            window.focus(&self.input_focus, cx);
        }
        let mut page = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(260.))
            .min_h_0()
            .bg(p.term)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.input_focus, cx);
                    this.terminal_ctx = Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                    cx.notify();
                }),
            );
        if self.find_open {
            page = page.child(self.render_find_bar(window, p, cx));
        }
        page = page.child(self.render_terminal(active, p, window, cx));
        page
    }

    fn render_titlebar(
        &mut self,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 单行集成工具行：整行 WCA::Drag（系统拖动 + 双击最大化），
        // tabs/按钮子元素命中优先于拖动；右端为 46px 宽的 Windows 风格控制钮。
        div()
            .flex()
            .items_center()
            .h(px(38.0))
            .min_h(px(38.0))
            .flex_shrink_0()
            .px(px(9.0))
            .gap(px(2.0))
            .bg(p.rail)
            .window_control_area(WindowControlArea::Drag)
            .children(self.render_titlebar_content(p, window, cx))
    }

    fn render_titlebar_content(
        &mut self,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let _ = window;
        let mut out: Vec<AnyElement> = Vec::new();

        // menu
        out.push(
            div()
                .id("ws-menu")
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    this.menu_open = !this.menu_open;
                    this.open_dd = None;
                    cx.notify();
                }))
                .h(px(26.0))
                .w(px(30.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .occlude()
                .hover(|s| s.bg(p.hover))
                .child(icon("menu").size(px(15.0)).text_color(p.muted))
                .into_any_element(),
        );
        // 分隔线
        out.push(
            div()
                .w(px(1.0))
                .h(px(18.0))
                .mx(px(6.0))
                .bg(p.line)
                .into_any_element(),
        );

        // session tabs（h_full 顶接 titlebar 底边，激活 bg(term) 与终端连通）
        let count = self.sessions.len();
        for index in 0..count {
            let is_active = index == self.active_session;
            let session = &self.sessions[index];
            let title = session.display_title(&self.duts.file.duts);
            // 重连中的会话 tab 圆点标红（H1.4 重连状态显示）
            let link_red = self.sessions[index].link == LinkState::Reconnecting;
            let mut tab = div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .occlude()
                .h_full()
                .px(px(8.0))
                .min_w(px(105.0))
                .max_w(px(200.0))
                .rounded_t(px(6.0))
                .text_size(px(12.0))
                .text_color(if is_active { p.text } else { p.muted });
            tab = if is_active {
                tab.bg(p.term)
                    .border_t_1()
                    .border_l_1()
                    .border_r_1()
                    .border_color(p.border)
            } else {
                tab.hover(|s| s.bg(p.hover))
            };
            let dot = div()
                .size(px(11.0))
                .rounded_full()
                .bg(p.accent_ring)
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(6.0)).rounded_full().bg(if link_red {
                    p.red
                } else if is_active {
                    p.accent
                } else {
                    p.faint
                }));
            out.push(
                tab.id(SharedString::from(format!("session-tab-{index}")))
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.active_session = index;
                        window.focus(&this.input_focus, cx);
                        cx.notify();
                    }))
                    .child(dot)
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .font_weight(gpui_kit::FontWeight(550.0))
                            .child(title),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("session-tab-close-{index}")))
                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                // 阻断冒泡：否则父 tab 的 on_click 也会触发，
                                // 把 active_session 设成删除前的旧索引（可能越界）
                                cx.stop_propagation();
                                this.close_session_view(index, cx);
                            }))
                            .size(px(16.0))
                            .rounded(px(5.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(14.0))
                            .text_color(p.faint)
                            .hover(|s| s.bg(p.hover).text_color(p.text))
                            .child("×"),
                    )
                    .into_any_element(),
            );
        }
        // ＋ 新建会话
        out.push(
            div()
                .id("new-session")
                .on_click(cx.listener(|this, _ev, _window, cx| {
                    this.begin_new_connection(true, cx);
                    cx.notify();
                }))
                .w(px(24.0))
                .h(px(24.0))
                .ml(px(2.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .occlude()
                .hover(|s| s.bg(p.hover))
                .child(icon("plus").size(px(14.0)).text_color(p.muted))
                .into_any_element(),
        );

        // 空档：占满 tab 与工具栏之间（TitleBar 拖动区）
        out.push(div().flex_1().h_full().into_any_element());

        if self.rec_on {
            out.push(
                div()
                    .px(px(4.0))
                    .text_size(px(10.0))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.red)
                    .child("● REC")
                    .into_any_element(),
            );
        }

        // Windows 风格窗口控制钮（46px 宽，16px 字形；系统命中区 Min/Max/Close）
        out.push(
            div()
                .flex()
                .items_center()
                .h_full()
                .flex_shrink_0()
                .ml(px(6.0))
                .occlude()
                .child(Self::win_btn(
                    "win-min",
                    "window-minimize",
                    WindowControlArea::Min,
                    p,
                    false,
                ))
                .child(Self::win_btn(
                    "win-max",
                    "window-maximize",
                    WindowControlArea::Max,
                    p,
                    false,
                ))
                .child(Self::win_btn(
                    "win-close",
                    "window-close",
                    WindowControlArea::Close,
                    p,
                    true,
                ))
                .into_any_element(),
        );
        out
    }

    /// 单个窗口控制钮：宽 46 与 Windows 原生一致；close 悬停红底。
    fn win_btn(
        id: &'static str,
        icon_name: &'static str,
        area: WindowControlArea,
        p: &'static Palette,
        close: bool,
    ) -> gpui_kit::Stateful<Div> {
        let hover_fg = if close { p.bg } else { p.text };
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(46.0))
            .h_full()
            .text_color(p.muted)
            .window_control_area(area)
            .hover(move |s| {
                s.bg(if close { p.red } else { p.hover })
                    .text_color(hover_fg)
            })
            .child(kit_icon(icon_name).size(px(16.0)))
    }

    fn render_find_bar(
        &mut self,
        window: &Window,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let query = self.find_query.clone();
        let focused = self
            .focus_map
            .get(&In::Find)
            .map(|f| f.is_focused(window))
            .unwrap_or(false);
        let find_focus = self.focus(In::Find, cx);
        let total = if query.is_empty() {
            0
        } else {
            self.sessions
                .get(self.active_session)
                .map(|v| {
                    v.lines
                        .iter()
                        .filter(|l| l.text.to_lowercase().contains(&query.to_lowercase()))
                        .count()
                })
                .unwrap_or(0)
        };
        let mut bar = div()
            .flex()
            .items_center()
            .gap(px(9.))
            .h(px(47.))
            .px(px(20.))
            .bg(p.surface)
            .border_b_1()
            .border_color(p.border)
            .child(icon("search").size(px(15.)).text_color(p.muted));
        let mut input = input_box(
            "find-input",
            &query,
            crate::i18n::label("findTerminal"),
            focused,
            false,
            p,
        )
        .max_w(px(360.))
        .flex_1()
        .track_focus(&find_focus)
        .on_click(cx.listener(move |this, _ev, window, cx| {
            window.focus(&find_focus, cx);
            this.notify_self(cx);
        }))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _win, cx| {
            this.input_key(In::Find, event);
            cx.notify();
        }));
        input = input.min_h(px(31.));
        bar = bar
            .child(input)
            .child(
                div()
                    .min_w(px(56.))
                    .text_center()
                    .text_size(px(11.))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.muted)
                    .child(SharedString::from(format!("0 / {total}"))),
            )
            .child(
                div()
                    .id("find-close")
                    .on_click(cx.listener(|this, _ev, _win, cx| {
                        this.find_open = false;
                        cx.notify();
                    }))
                    .size(px(29.))
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(p.muted)
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .child("×"),
            );
        bar
    }

    fn render_terminal(
        &mut self,
        active: usize,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let query = self.find_query.to_lowercase();
        // 跟随模式（原型 nearBottom）：上一帧视口贴近底部（或内容不足一屏）时，
        // 新输出自动滚到底部；用户上翻阅读时不打断。
        let near_bottom = {
            let offset = self.terminal_scroll.offset().y;
            let max = self.terminal_scroll.max_offset().y;
            // offset 向下滚动为负，到底时 offset.y ≈ -max；距底部距离 = max + offset
            (max + offset) < px(65.)
        };
        // real 会话（PuTTY 模式）：视口尺寸变化 → 下发 term.resize(cols, rows)
        let is_device = self
            .sessions
            .get(active)
            .map(|s| s.source == "device")
            .unwrap_or(false);
        let (font_family, font_size, cell_width) = self.terminal_font(window);
        let row_height = font_size * (20.6 / DEFAULT_TERMINAL_FONT_SIZE);
        if is_device {
            let (left, right) = self.pane_widths(window.viewport_size().width);
            let width = left + right;
            let (cols, rows) = terminal_grid_for(window, width, cell_width, row_height);
            if self.term_grid != Some((cols, rows))
                && let Some(view) = self.sessions.get(active)
            {
                let dut_id = view.dut_id.clone();
                if self.device.resize_terminal(&dut_id, cols, rows).is_ok() {
                    self.term_grid = Some((cols, rows));
                }
            }
        }
        let mut viewport = div()
            .id("terminal-viewport")
            .track_scroll(&self.terminal_scroll)
            .flex_1()
            .flex_col()
            .overflow_y_scroll()
            .px(px(19.))
            .pt(px(16.))
            .pb(px(40.))
            .text_size(font_size)
            .line_height(row_height)
            .font_family(font_family)
            .cursor_text()
            .track_focus(&self.input_focus)
            .on_click(cx.listener(|this, _ev, window, cx| {
                window.focus(&this.input_focus, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.terminal_ctx.take().is_some() {
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if this.overlay.is_some() || window.has_active_dialog(cx) {
                    return;
                }
                if this.terminal_zoom_key(event, cx) {
                    return;
                }
                if is_device {
                    let k = &event.keystroke;
                    if k.modifiers.alt || k.modifiers.platform {
                        return;
                    }
                    if k.modifiers.control {
                        if k.modifiers.shift && k.key == "v" {
                            this.paste_terminal(cx);
                            cx.stop_propagation();
                        } else if let [key @ b'a'..=b'z'] = k.key.as_bytes() {
                            this.send_bytes(&[key - b'a' + 1], cx);
                            cx.stop_propagation();
                        }
                        return;
                    }
                    match k.key.as_str() {
                        "enter" => this.send_bytes(b"\r", cx),
                        "backspace" => this.send_bytes(&[0x7f], cx),
                        "tab" => this.send_bytes(b"\t", cx),
                        "escape" => this.send_bytes(b"\x1b", cx),
                        "up" | "down" | "right" | "left" | "home" | "end" => {
                            let prefix = if this
                                .sessions
                                .get(active)
                                .and_then(|session| session.screen.as_ref())
                                .is_some_and(|screen| screen.application_cursor)
                            {
                                b'O'
                            } else {
                                b'['
                            };
                            let code = match k.key.as_str() {
                                "up" => b'A',
                                "down" => b'B',
                                "right" => b'C',
                                "left" => b'D',
                                "home" => b'H',
                                _ => b'F',
                            };
                            this.send_bytes(&[0x1b, prefix, code], cx);
                            cx.stop_propagation();
                        }
                        "delete" => this.send_bytes(b"\x1b[3~", cx),
                        _ => {
                            if let Some(ch) = typed_char(event) {
                                this.send_bytes(ch.as_bytes(), cx);
                            }
                        }
                    }
                    return;
                }
                let k = &event.keystroke;
                if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
                    return;
                }
                if k.key == "enter" {
                    this.send_input(cx);
                    return;
                }
                if k.key == "backspace" {
                    this.input.pop();
                    cx.notify();
                    return;
                }
                if let Some(ch) = typed_char(event).filter(|_| this.input.len() < 4096) {
                    this.input.push_str(&ch);
                    cx.notify();
                }
            }));
        let empty = self
            .sessions
            .get(active)
            .map(|s| {
                s.lines.is_empty()
                    && s.screen
                        .as_ref()
                        .is_none_or(|sc| sc.rows.iter().all(|r| r.text.is_empty()))
            })
            .unwrap_or(true);
        if empty && !is_device {
            viewport = viewport.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .mt(px(90.))
                    .gap(px(14.))
                    .text_color(p.muted)
                    .child(icon("terminal").size(px(34.)).text_color(p.faint))
                    .child("Duthub Console")
                    .child(
                        div()
                            .text_size(px(12.))
                            .child(SharedString::from(crate::i18n::label("waitingForSerial"))),
                    ),
            );
        }
        if let Some(session) = self.sessions.get(active) {
            for line in &session.lines {
                let hit = !query.is_empty() && line.text.to_lowercase().contains(&query);
                if line.kind == LineKind::Mark {
                    // 分段标记（日志分割）：分隔样式——两侧通栏细线夹 [mark] 文本
                    viewport = viewport.child(
                        div()
                            .mt(px(7.))
                            .mb(px(7.))
                            .flex()
                            .items_center()
                            .gap(px(9.))
                            .when(hit, |el| el.bg(p.match_))
                            .child(div().flex_1().h(px(1.)).bg(p.line))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(font_size * 0.84)
                                    .text_color(line.color(p))
                                    .child(line.text.clone()),
                            )
                            .child(div().flex_1().h(px(1.)).bg(p.line)),
                    );
                    continue;
                }
                viewport = viewport.child(
                    div()
                        .min_h(row_height)
                        .line_height(row_height)
                        .text_color(line.color(p))
                        .when(hit, |el| el.bg(p.match_))
                        .child(line.text.clone()),
                );
            }
            // real 会话：仿真器网格快照（PuTTY 渲染源）——行文本 + 前景/背景色 +
            // 光标单元反显。demo 会话没有快照，维持上面逐行渲染。
            if let Some(screen) = &session.screen {
                for (idx, row) in screen.rows.iter().enumerate() {
                    let hit = !query.is_empty() && row.text.to_lowercase().contains(&query);
                    let mut el = div()
                        .min_h(row_height)
                        .line_height(row_height)
                        .text_color(row.fg.map(theme::rgba_hex).unwrap_or(p.term_fg))
                        .when(hit, |el| el.bg(p.match_));
                    if let Some(bg) = row.bg {
                        el = el.bg(theme::rgba_hex(bg));
                    }
                    // 光标行：按字符偏移切成三段，光标单元反显（块状光标）
                    if screen.cursor_row == Some(idx as u32)
                        && let Some(cc) = screen.cursor_char
                    {
                        let chars: Vec<char> = row.text.chars().collect();
                        let cut = (cc as usize).min(chars.len());
                        let before: String = chars[..cut].iter().collect();
                        let cursor_ch: String = chars
                            .get(cut)
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| " ".to_string());
                        let after: String = chars[(cut + 1).min(chars.len())..].iter().collect();
                        el = el
                            .flex()
                            .child(SharedString::from(before))
                            .child(
                                div()
                                    .text_color(p.term)
                                    .bg(p.term_fg)
                                    .child(SharedString::from(cursor_ch)),
                            )
                            .child(SharedString::from(after));
                    } else {
                        el = el.child(row.text.clone());
                    }
                    viewport = viewport.child(el);
                }
            }
        }
        // 真实会话由设备回显。演示会话保留本地输入行。
        if !is_device {
            let input = if self.input.is_empty() {
                div()
                    .text_color(p.faint)
                    .child(crate::i18n::label("terminalInputHint"))
            } else {
                div().child(SharedString::from(self.input.clone()))
            };
            viewport = viewport.child(
                div()
                    .flex()
                    .items_start()
                    .min_h(px(25.))
                    .mt(px(2.))
                    .text_color(p.term_fg)
                    .child(
                        div()
                            .text_color(p.prompt)
                            .font_weight(gpui_kit::FontWeight(600.0))
                            .child("root:~# "),
                    )
                    .child(input),
            );
        }
        if near_bottom {
            // 下一帧 paint 时消费该标记：scroll_offset.y = -max（贴底）
            self.terminal_scroll.scroll_to_bottom();
        }
        viewport
    }

    fn show_command_dock(&self) -> bool {
        self.settings.terminal.show_command_bar && !self.settings.commands.is_empty()
    }

    fn render_command_dock(
        &mut self,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut dock = div()
            .flex()
            .items_center()
            .flex_1()
            .min_w_0()
            .gap(px(3.))
            .id("command-dock")
            .overflow_x_scroll();
        let can_run =
            self.view == ViewMode::Serial && self.sessions.get(self.active_session).is_some();
        for (index, command) in self.settings.commands.iter().enumerate() {
            let value = command.value.clone();
            dock = dock.child(
                div()
                    .id(SharedString::from(format!("dock-cmd-{index}")))
                    .when(can_run, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.input = value.clone();
                            this.send_input(cx);
                        }))
                    })
                    .when(!can_run, |button| button.opacity(0.6))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .h(px(29.))
                    .flex_none()
                    .min_w_0()
                    .whitespace_nowrap()
                    .px(px(6.))
                    .rounded(px(5.))
                    .text_size(px(11.))
                    .line_height(px(19.))
                    .font_family(theme::MONO_FONT)
                    .text_color(p.muted)
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .child(icon("bolt").size(px(14.)).flex_none().text_color(p.accent))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(command.name.clone())),
                    ),
            );
        }
        dock = dock.child(
            div()
                .id("dock-add")
                .on_click(cx.listener(|this, _ev, window, cx| {
                    this.open_modal(ModalKind::NewCommand, window, cx);
                    cx.notify();
                }))
                .size(px(29.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .text_color(p.muted)
                .hover(|s| s.bg(p.hover).text_color(p.text))
                .child(icon("plus").size(px(15.)).text_color(p.muted)),
        );
        dock
    }

    pub(crate) fn open_remove_dut(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !window.has_active_dialog(cx)
            && let Some(dut) = self.duts.file.duts.get(index)
        {
            self.open_modal(ModalKind::RemoveDut(dut.id.clone()), window, cx);
        }
    }

    pub(crate) fn remove_control_device(&mut self, id: &str) -> Result<(), String> {
        let mut draft = self.duts.file.clone();
        crate::data::remove_control_device(&mut draft, id)?;
        self.heartbeat.commit_configs(&HashMap::new(), |_| {
            if let Some(mut file) =
                store::load_duts(&self.data_dir).map_err(|err| err.to_string())?
            {
                file.migrate_usb_relays();
                if crate::data::remove_control_device(&mut file, id)? {
                    store::save_duts_atomic(&self.data_dir, &file)
                        .map_err(|err| err.to_string())?;
                }
            }
            Ok(())
        })?;
        self.duts.file = draft;
        self.relay_board_selected = self
            .relay_board_selected
            .min(self.duts.file.relay_boards.len().saturating_sub(1));
        self.relay_board_name = None;
        Ok(())
    }

    pub fn remove_dut(&mut self, dut_id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(index) = self.duts.file.duts.iter().position(|dut| dut.id == dut_id) else {
            return Err(crate::i18n::label("connectionMissing").into());
        };
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
        let removed_name = self.duts.file.duts[index].name.clone();
        crate::data::remove_connection(&mut self.duts.file, dut_id);
        self.duts.forget_saved_bindings(dut_id);
        self.media.reconcile_bindings(&self.duts);
        self.alerts
            .lock()
            .unwrap()
            .set_dut_settings(&self.duts.file.alerts_by_dut);
        if index < self.duts.selected {
            self.duts.selected -= 1;
        }
        self.duts.selected = self
            .duts
            .selected
            .min(self.duts.file.duts.len().saturating_sub(1));
        self.heartbeat_drafts.remove(dut_id);
        if let Some(index) = self.sessions.iter().position(|view| view.dut_id == dut_id) {
            self.close_session_view(index, cx);
        }
        self.heartbeat.unregister(dut_id);
        self.agent.remove(dut_id);
        self.control.unregister_dut(dut_id);
        self.notify_toast(crate::i18n::message(
            "connectionDeleted",
            &[("name", &removed_name)],
        ));
        Ok(())
    }

    fn close_session_view(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(view) = self.sessions.get(index) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        // demo 会话同样注册在 DeviceService（SessionKind::Demo），tab 关闭必须级联
        // 关闭底层会话——否则 demo 任务/注册表句柄/端口锁泄漏，同一 DUT 重开恒 Busy；
        // 注册表无此 dut 时返回 NotFound，忽略即可（NotFound 之外的错误同样不影响 tab 关闭）。
        self.agent.serial_closed(&dut_id);
        let _ = self.device.close_session(&dut_id);
        self.sessions.remove(index);
        // 关闭位于活动会话左侧的 tab 时，活动索引随列表收缩前移
        //（否则活动视图静默切到相邻会话——输入行会把命令写进另一个串口）
        if index < self.active_session {
            self.active_session -= 1;
        }
        if self.active_session >= self.sessions.len() {
            self.active_session = self.sessions.len().saturating_sub(1);
        }
        // 关闭全部会话 → 回首页
        if self.sessions.is_empty() {
            self.view = ViewMode::Home;
        }
        cx.notify();
    }

    /// 从 DUT 绑定打开会话（NewSession 弹窗回调）。
    /// 直连真实串口（首页卡片 / ＋ 新建会话共用）：成功建 tab，失败只 toast 一次。
    pub fn open_real_port(&mut self, port: &str, baud: u32, cx: &mut Context<Self>) {
        let dut_id = Self::dut_id_for_port(&self.duts, port);
        let name = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| dut.id == dut_id)
            .map_or(port, |dut| dut.name.as_str());
        match self.device.open_named_session(
            &dut_id,
            name,
            port,
            baud,
            duthub_device::SessionKind::Real,
        ) {
            Ok(session_id) => {
                let index = self.ensure_session(&session_id, &dut_id);
                self.sessions[index].title = format!("{port} · device");
                self.active_session = index;
                self.view = ViewMode::Serial;
                self.terminal_focus_pending = true;
                self.notify_toast(format!("已创建串口连接 {port} @ {baud}"));
                // 保存为主页磁贴（Xshell 式：下次直连/修改复用）
                if !self.duts.file.duts.iter().any(|d| d.id == dut_id) {
                    let mut dut = demo_dut_template(port, port);
                    dut.id = dut_id;
                    dut.serial.baud = baud;
                    self.duts.file.duts.push(dut);
                    self.duts.selected = self.duts.file.duts.len() - 1;
                    if let Err(err) = self.duts.save(&self.data_dir) {
                        self.notify_toast(format!("保存会话失败: {err}"));
                    }
                }
            }
            Err(err) => {
                let hint = if matches!(err, duthub_core::DeviceError::Unsupported) {
                    "当前构建未启用 real-hardware".to_string()
                } else {
                    format!("连接失败: {err}")
                };
                // 首开失败（端口被占/不支持）：toast + 弹窗提醒（需求⑤接入点②）
                self.report_serial_session_failure(&dut_id, port, "串口连接失败", &hint);
                self.notify_toast(hint);
            }
        }
        cx.notify();
    }

    /// 终端右键菜单：立即重连 / 重置终端 / 查找 / 复制 / 清屏。
    fn render_terminal_ctx_menu(
        &mut self,
        x: f32,
        y: f32,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let menu_x = px(x.min(980.0).clamp(4.0, f32::MAX));
        let menu_y = px(y.min(508.0).clamp(4.0, f32::MAX));

        let mut menu = div()
            .id("terminal-ctx")
            .absolute()
            .left(menu_x)
            .top(menu_y)
            .w(px(190.0))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .rounded(px(9.0))
            .shadow_lg()
            .p(px(6.0))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.terminal_ctx = None;
                cx.notify();
            }));

        let item = |this_id: &'static str, label: &'static str| {
            div()
                .id(this_id)
                .flex()
                .items_center()
                .justify_between()
                .gap(px(10.0))
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(6.0))
                .text_size(px(12.0))
                .text_color(p.text)
                .cursor_pointer()
                .hover(|s| s.bg(p.hover))
                .child(SharedString::from(label))
        };

        menu = menu.child(
            item("tctx-reconnect", crate::i18n::label("reconnectNow")).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _win, cx| {
                    this.terminal_ctx = None;
                    this.request_manual_reconnect(cx);
                    cx.notify();
                }),
            ),
        );

        menu = menu.child(
            item("tctx-reset", crate::i18n::label("resetTerminal")).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    this.reset_serial_terminal(window, cx);
                }),
            ),
        );

        menu = menu.child(
            div()
                .pt(px(6.0))
                .pb(px(2.0))
                .px(px(10.0))
                .text_size(px(10.5))
                .text_color(p.faint)
                .child(crate::i18n::label("terminal")),
        );
        menu = menu.child(
            item("tctx-find", crate::i18n::label("findShort")).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _win, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    this.find_open = !this.find_open;
                    cx.notify();
                }),
            ),
        );
        if self
            .sessions
            .get(self.active_session)
            .is_some_and(|view| view.source == "device")
        {
            menu = menu.child(
                item("tctx-paste", crate::i18n::label("paste")).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, window, cx| {
                        cx.stop_propagation();
                        this.terminal_ctx = None;
                        window.focus(&this.input_focus, cx);
                        this.paste_terminal(cx);
                        cx.notify();
                    }),
                ),
            );
        }
        menu = menu.child(
            item("tctx-copy", crate::i18n::label("copyOutput")).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _win, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    if let Some(view) = this.sessions.get(this.active_session) {
                        // real 会话：注记行 + 仿真器网格文本；demo 会话：全部行
                        let mut parts: Vec<String> =
                            view.lines.iter().map(|l| l.text.to_string()).collect();
                        if let Some(screen) = &view.screen {
                            parts.extend(screen.rows.iter().map(|r| r.text.clone()));
                        }
                        let text = parts.join("\n");
                        crate::components::copy_text(cx, &text);
                        this.notify_toast(crate::i18n::label("outputCopied"));
                    }
                    cx.notify();
                }),
            ),
        );
        menu = menu.child(
            item("tctx-clear", crate::i18n::label("clearTerminal")).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _win, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    if let Some(view) = this.sessions.get_mut(this.active_session) {
                        view.lines.clear();
                    }
                    cx.notify();
                }),
            ),
        );

        menu
    }

    fn render_workspace_menu(
        &mut self,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let item = |id: &'static str, icon_name: &str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(11.))
                .h(px(35.))
                .w_full()
                .px(px(10.))
                .rounded(px(6.))
                .text_size(px(13.))
                .text_color(p.text)
                .hover(|s| s.bg(p.hover))
                .child(icon(icon_name).size(px(16.)).text_color(p.muted))
                .child(label)
        };
        div()
            .id("ws-menu-root")
            .absolute()
            .left(px(6.))
            .top(px(48.))
            .w(px(206.))
            .p(px(6.))
            .flex()
            .flex_col()
            .gap(px(3.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .rounded(px(9.))
            .shadow_lg()
            .occlude()
            .on_click(cx.listener(|this, _ev, _win, cx| {
                this.menu_open = false;
                cx.notify();
            }))
            .child(
                item("menu-agent-model", "spark", crate::i18n::label("agent")).on_click(
                    cx.listener(|this, _ev, _win, cx| {
                        this.set_config_page(Some(ConfigPage::AgentModel));
                        this.menu_open = false;
                        cx.notify();
                    }),
                ),
            )
            .child(
                item("menu-devtools", "cpu", crate::i18n::label("deviceTools")).on_click(
                    cx.listener(|this, _ev, _win, cx| {
                        this.set_config_page(Some(ConfigPage::Flash));
                        this.menu_open = false;
                        cx.notify();
                    }),
                ),
            )
            .child(
                item("menu-settings", "settings", crate::i18n::label("settings")).on_click(
                    cx.listener(|this, _ev, _win, cx| {
                        this.set_config_page(Some(ConfigPage::Settings));
                        this.menu_open = false;
                        cx.notify();
                    }),
                ),
            )
    }

    fn render_connection_dock(
        &mut self,
        width: Pixels,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .flex_none()
            .min_h_0()
            .w(width)
            .relative()
            .overflow_hidden()
            .bg(p.surface)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(14.))
                    .h(px(44.))
                    .flex_none()
                    .border_b_1()
                    .border_color(p.line)
                    .child(crate::i18n::label(if self.duts.editing_new_connection {
                        "createConnection"
                    } else {
                        "connectionTitle"
                    }))
                    .child(
                        icon_btn("close-connection-dock", "x", p).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.connection_open = false;
                                this.open_dd = None;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(render_connection(self, p, window, cx))
            .child(PaneDivider::Connection.handle())
    }

    fn render_config_overlay(
        &mut self,
        page: ConfigPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.p;
        let title = match page {
            ConfigPage::AgentModel => crate::i18n::label("agent").to_string(),
            ConfigPage::Flash => crate::i18n::label("flashTitle").to_string(),
            ConfigPage::Adb => self.lang.usb_debug_title().to_string(),
            ConfigPage::Relay => crate::i18n::label("relayTitle").to_string(),
            ConfigPage::Media => crate::i18n::label("deviceMonitor").to_string(),
            ConfigPage::Settings => i18n::t(self.lang, "settings").to_string(),
        };
        let search = self.cfg_search.clone();
        let query = search.trim().to_lowercase();
        let nav = |w: &mut Self,
                   id: &'static str,
                   icon_name: &str,
                   label: &'static str,
                   target: ConfigPage,
                   cx: &mut Context<Self>| {
            if !query.is_empty() && !label.to_lowercase().contains(&query) {
                return div().id(id);
            }
            let active = w.overlay == Some(target);
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(10.))
                .w_full()
                .py(px(9.))
                .px(px(10.))
                .rounded(px(8.))
                .text_size(px(14.))
                .text_color(if active { p.text } else { p.muted })
                .hover(|s| s.bg(p.hover).text_color(p.text))
                .when(active, |el| {
                    el.bg(p.hover).font_weight(gpui_kit::FontWeight(600.0))
                })
                .child(icon(icon_name).size(px(16.)).text_color(if active {
                    p.accent
                } else {
                    p.muted
                }))
                .child(label)
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.set_config_page(Some(target));
                    cx.notify();
                }))
        };
        let search_input = self
            .text_field(
                In::CfgSearch,
                &search,
                crate::i18n::label("searchSettings"),
                false,
                p,
                window,
                cx,
            )
            .prefix(icon("search").size(px(15.)).text_color(p.muted));
        let content = match page {
            ConfigPage::AgentModel => {
                crate::provider_models::render(self, p, window, cx).into_any_element()
            }
            ConfigPage::Flash => render_flash_tools(self, p, cx).into_any_element(),
            ConfigPage::Adb => render_usb_debug(self, p, window, cx).into_any_element(),
            ConfigPage::Relay => {
                crate::pages::render_relay_boards(self, p, window, cx).into_any_element()
            }
            ConfigPage::Media => crate::media_page::render(self, p, window, cx).into_any_element(),
            ConfigPage::Settings => render_settings(self, p, window, cx).into_any_element(),
        };
        div()
            .absolute()
            .inset_0()
            .bg(p.backdrop)
            .flex()
            .items_center()
            .justify_center()
            .p(px(26.))
            .occlude()
            .child(
                div()
                    .w(px(1120.))
                    .max_w_full()
                    .h(px(760.))
                    .max_h_full()
                    .bg(p.surface)
                    .border_1()
                    .border_color(p.border)
                    .rounded(px(14.))
                    .shadow_lg()
                    .flex()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(260.))
                            .flex_none()
                            .bg(p.sidebar)
                            .border_r_1()
                            .border_color(p.line)
                            .p(px(12.))
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child(search_input)
                            .child(nav(
                                self,
                                "cfg-nav-agent-model",
                                "spark",
                                crate::i18n::label("agent"),
                                ConfigPage::AgentModel,
                                cx,
                            ))
                            .child(
                                div()
                                    .pt(px(14.0))
                                    .pb(px(4.0))
                                    .px(px(10.0))
                                    .text_size(px(10.5))
                                    .font_family(crate::theme::MONO_FONT)
                                    .text_color(p.faint)
                                    .child(crate::i18n::label("deviceTools")),
                            )
                            .child(nav(
                                self,
                                "cfg-nav-flash",
                                "hard-drive",
                                crate::i18n::label("flashTitle"),
                                ConfigPage::Flash,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-adb",
                                "usb",
                                self.lang.usb_debug_title(),
                                ConfigPage::Adb,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-relays",
                                "bolt",
                                crate::i18n::label("relayTitle"),
                                ConfigPage::Relay,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-media",
                                "eye",
                                crate::i18n::label("deviceMonitor"),
                                ConfigPage::Media,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-settings",
                                "settings",
                                crate::i18n::label("settings"),
                                ConfigPage::Settings,
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .min_h_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .h(px(52.))
                                    .px(px(16.))
                                    .border_b_1()
                                    .border_color(p.line)
                                    .child(
                                        div()
                                            .text_size(px(15.))
                                            .font_weight(gpui_kit::FontWeight(650.0))
                                            .text_color(p.text)
                                            .child(SharedString::from(title)),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .id("config-close")
                                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                                this.set_config_page(None);
                                                cx.notify();
                                            }))
                                            .size(px(30.))
                                            .rounded(px(7.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_color(p.muted)
                                            .hover(|s| s.bg(p.hover).text_color(p.text))
                                            .child(icon("x").size(px(16.)).text_color(p.muted)),
                                    ),
                            )
                            .child(
                                div()
                                    .id("config-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .pt(px(2.))
                                    .child(content),
                            ),
                    ),
            )
    }

    /// 主页磁贴单击：按已保存 DUT 配置直连。
    pub fn connect_dut(&mut self, dut_index: usize, cx: &mut Context<Self>) {
        let Some(dut) = self.duts.file.duts.get(dut_index).cloned() else {
            return;
        };
        let kind = if dut.serial.port == "DEMO" {
            duthub_device::SessionKind::Demo
        } else {
            duthub_device::SessionKind::Real
        };
        // Linux 设备路径在 Windows 上不可能打开：直接明确报错，不产生"重连中"僵尸会话
        #[cfg(target_os = "windows")]
        if kind == duthub_device::SessionKind::Real && dut.serial.port.starts_with("/dev/") {
            self.notify_toast(format!(
                "{} 绑定的是 Linux 串口路径（{}）——Windows 上无法打开；请在连接管理中把串口节点改为本机 COM 端口",
                dut.name, dut.serial.port
            ));
            cx.notify();
            return;
        }
        match self.device.open_named_session(
            &dut.id,
            self.duts.saved_name(&dut.id).unwrap_or(&dut.name),
            &dut.serial.port,
            dut.serial.baud,
            kind,
        ) {
            Ok(session_id) => {
                let index = self.ensure_session(&session_id, &dut.id);
                self.sessions[index].title = format!("{} · {}", dut.name, dut.serial.port);
                self.active_session = index;
                self.view = ViewMode::Serial;
                self.terminal_focus_pending = true;
                self.notify_toast(format!("正在连接 {} · {}…", dut.name, dut.serial.port));
            }
            Err(err) => {
                let hint = if matches!(err, duthub_core::DeviceError::Unsupported) {
                    "当前构建未启用 real-hardware".to_string()
                } else {
                    format!("连接失败: {err}")
                };
                // 首开失败（DUT 磁贴直连）：toast + 弹窗提醒（需求⑤接入点②）
                self.report_serial_session_failure(&dut.id, &dut.name, "串口连接失败", &hint);
                self.notify_toast(hint);
            }
        }
        cx.notify();
    }

    /// 打开 kit Dialog：外壳（遮罩/动画/标题栏）由 kit 管理，
    /// 表单主体仍由 render_modal_parts 渲染（草稿状态留在 Workbench）。
    pub fn open_modal(&mut self, kind: ModalKind, window: &mut Window, cx: &mut Context<Self>) {
        for field in [
            In::CmdName,
            In::CmdValue,
            In::NewPresetName,
            In::NewPresetExe,
            In::PeerName,
            In::PeerUrl,
            In::RenameName,
        ] {
            self.text_editors.remove(&field);
        }
        let draft = match &kind {
            ModalKind::RenameDut(index) => self
                .duts
                .file
                .duts
                .get(*index)
                .map(|dut| dut.name.clone())
                .unwrap_or_default(),
            _ => String::new(),
        };
        self.modal = Some(ModalState {
            kind,
            draft,
            draft2: String::new(),
        });
        let view = cx.entity();
        let p = self.p;
        window.open_dialog(cx, move |dialog, window, cx| {
            let modal = view.update(cx, |w, _cx| {
                w.modal.clone().unwrap_or(ModalState {
                    kind: ModalKind::NewSession,
                    draft: String::new(),
                    draft2: String::new(),
                })
            });
            let (title, body) = view.update(cx, |w, cx| w.render_modal_parts(modal, p, window, cx));
            dialog.title(title).child(body)
        });
        cx.notify();
    }

    fn render_modal_parts(
        &mut self,
        modal: ModalState,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (SharedString, gpui_kit::AnyElement) {
        let (title, body): (&str, gpui_kit::AnyElement) = match &modal.kind {
            ModalKind::RenameDut(_) => (
                crate::i18n::label("renameDut"),
                self.modal_text_input(
                    In::RenameName,
                    crate::i18n::label("dutName"),
                    "",
                    p,
                    window,
                    cx,
                ),
            ),
            ModalKind::RemoveDut(id) => (
                crate::i18n::label("deleteConnection"),
                div()
                    .text_size(px(12.5))
                    .text_color(p.muted)
                    .child(SharedString::from(format!(
                        "删除「{}」的连接配置？正在使用的会话将断开，已保存的日志会保留。",
                        self.duts
                            .file
                            .duts
                            .iter()
                            .find(|dut| dut.id == *id)
                            .map_or(id.as_str(), |dut| dut.name.as_str()),
                    )))
                    .into_any_element(),
            ),
            ModalKind::NewSession => {
                // 列出本机真实串口（打开弹窗时已刷新），点击直连 115200
                let mut body = div().flex().flex_col();
                if self.serial_ports.is_empty() {
                    body = body.child(
                        div()
                            .text_size(px(12.5))
                            .text_color(p.muted)
                            .child(crate::i18n::label("noSerialPorts")),
                    );
                }
                for (index, port_info) in self.serial_ports.iter().enumerate() {
                    let port = port_info.port.clone();
                    body = body.child(
                        div()
                            .id(SharedString::from(format!("modal-port-{index}")))
                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                this.modal = None;
                                this.open_real_port(&port, 115_200, cx);
                            }))
                            .flex()
                            .items_center()
                            .justify_between()
                            .w_full()
                            .px(px(11.))
                            .py(px(10.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.bg)
                            .mb(px(8.))
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.accent))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_family(theme::MONO_FONT)
                                    .text_color(p.text)
                                    .child(SharedString::from(port_info.port.clone())),
                            )
                            .child(div().text_size(px(11.)).text_color(p.faint).child(
                                SharedString::from(
                                    port_info.description.clone().unwrap_or_else(|| {
                                        crate::i18n::label("serialDevices").into()
                                    }),
                                ),
                            )),
                    );
                }
                (crate::i18n::label("newSerialTab"), body.into_any_element())
            }
            ModalKind::NewCommand => {
                let body = div()
                    .child(self.modal_text_input(In::CmdName, "名称", "uname", p, window, cx))
                    .child(self.modal_text_input(
                        In::CmdValue,
                        crate::i18n::label("command"),
                        "uname -a",
                        p,
                        window,
                        cx,
                    ));
                (crate::i18n::label("customCommand"), body.into_any_element())
            }
            ModalKind::NewPreset => {
                let body = div()
                    .child(self.modal_text_input(
                        In::NewPresetName,
                        crate::i18n::label("presetName"),
                        "upgrade_tool",
                        p,
                        window,
                        cx,
                    ))
                    .child(self.modal_text_input(
                        In::NewPresetExe,
                        crate::i18n::label("executable"),
                        "upgrade_tool",
                        p,
                        window,
                        cx,
                    ));
                (crate::i18n::label("addFlashTool"), body.into_any_element())
            }
            ModalKind::NewPeer => {
                let body = div()
                    .child(self.modal_text_input(
                        In::PeerName,
                        "名称",
                        "Lab Peer Agent",
                        p,
                        window,
                        cx,
                    ))
                    .child(self.modal_text_input(
                        In::PeerUrl,
                        "Agent Card URL",
                        "https://…/.well-known/agent-card.json",
                        p,
                        window,
                        cx,
                    ));
                (crate::i18n::label("addPeerAgent"), body.into_any_element())
            }
        };
        type ConfirmFn = Box<dyn Fn(&mut Workbench, &mut Context<Workbench>) + 'static>;
        let (confirm_label, on_confirm): (&str, ConfirmFn) = match &modal.kind {
            ModalKind::RenameDut(index) => {
                let index = *index;
                (
                    crate::i18n::label("save"),
                    Box::new(move |w: &mut Workbench, cx: &mut Context<Workbench>| {
                        let name = w
                            .modal
                            .as_ref()
                            .map(|m| m.draft.clone())
                            .unwrap_or_default();
                        if name.trim().is_empty() {
                            return;
                        }
                        if let Some(dut) = w.duts.file.duts.get_mut(index) {
                            dut.name = name.trim().to_string();
                        }
                        w.duts.dirty = true;
                        w.modal = None;
                        cx.notify();
                    }),
                )
            }
            ModalKind::RemoveDut(id) => (
                crate::i18n::label("delete"),
                Box::new({
                    let id = id.clone();
                    move |w: &mut Workbench, cx: &mut Context<Workbench>| {
                        match w.remove_dut(&id, cx) {
                            Ok(()) => w.modal = None,
                            Err(err) => w.notify_toast(format!("删除连接失败: {err}")),
                        }
                        cx.notify();
                    }
                }),
            ),
            ModalKind::NewCommand => (
                crate::i18n::label("add"),
                Box::new(|w: &mut Workbench, cx: &mut Context<Workbench>| {
                    let (name, value) = w
                        .modal
                        .as_ref()
                        .map(|m| (m.draft.clone(), m.draft2.clone()))
                        .unwrap_or_default();
                    if name.trim().is_empty() || value.trim().is_empty() {
                        w.notify_toast(crate::i18n::label("nameAndCommandRequired"));
                        return;
                    }
                    w.settings.commands.push(QuickCommand {
                        name: name.trim().to_string(),
                        value: value.trim().to_string(),
                        icon: "bolt".into(),
                    });
                    w.persist_settings();
                    w.modal = None;
                    cx.notify();
                }),
            ),
            ModalKind::NewPreset => (
                crate::i18n::label("add"),
                Box::new(|w: &mut Workbench, cx: &mut Context<Workbench>| {
                    let (name, exe) = w
                        .modal
                        .as_ref()
                        .map(|m| (m.draft.clone(), m.draft2.clone()))
                        .unwrap_or_default();
                    if name.trim().is_empty() || exe.trim().is_empty() {
                        w.notify_toast(crate::i18n::label("nameAndExecutableRequired"));
                        return;
                    }
                    // 稳定预设 id：由名称生成 slug，与现有预设去重（下拉按 id 选择）
                    let mut id = {
                        let slug: String = name
                            .trim()
                            .chars()
                            .map(|c| {
                                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                                    c.to_ascii_lowercase()
                                } else {
                                    '-'
                                }
                            })
                            .collect();
                        let slug = slug.trim_matches('-');
                        if slug.is_empty() {
                            "preset".to_string()
                        } else {
                            format!("preset-{slug}")
                        }
                    };
                    let mut n = 1;
                    while w.settings.flash_tools.iter().any(|t| t.id == id) {
                        n += 1;
                        id = format!("{id}-{n}");
                    }
                    w.settings
                        .flash_tools
                        .push(duthub_core::model::FlashToolPreset {
                            id,
                            name: name.trim().to_string(),
                            executable: exe.trim().to_string(),
                            test_args: String::new(),
                        });
                    w.persist_settings();
                    w.modal = None;
                    cx.notify();
                }),
            ),
            ModalKind::NewPeer => (
                crate::i18n::label("add"),
                Box::new(|w: &mut Workbench, cx: &mut Context<Workbench>| {
                    let (name, url) = w
                        .modal
                        .as_ref()
                        .map(|m| (m.draft.clone(), m.draft2.clone()))
                        .unwrap_or_default();
                    if name.trim().is_empty() || url.trim().is_empty() {
                        w.notify_toast(crate::i18n::label("nameAndAddressRequired"));
                        return;
                    }
                    let a2a = w
                        .settings
                        .a2a
                        .get_or_insert_with(duthub_core::A2aConfig::default);
                    a2a.peers.push(duthub_core::A2aPeer {
                        id: format!("peer-{}", a2a.peers.len() + 1),
                        name: name.trim().to_string(),
                        card_url: url.trim().to_string(),
                    });
                    w.persist_settings();
                    w.modal = None;
                    cx.notify();
                }),
            ),
            ModalKind::NewSession => ("", Box::new(|_w, _cx| {})),
        };
        let mut col = div().flex().flex_col().child(body);
        if !confirm_label.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(10.))
                    .mt(px(17.))
                    .child(
                        div()
                            .id("modal-cancel")
                            .flex()
                            .items_center()
                            .min_h(px(32.0))
                            .px(px(12.0))
                            .rounded(px(8.0))
                            .bg(p.raised)
                            .border_1()
                            .border_color(p.border)
                            .text_size(px(13.0))
                            .text_color(p.text)
                            .cursor_pointer()
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.modal = None;
                                window.close_dialog(cx);
                                cx.notify();
                            }))
                            .child(crate::i18n::label("cancel")),
                    )
                    .child(
                        div()
                            .id("modal-confirm")
                            .flex()
                            .items_center()
                            .min_h(px(32.0))
                            .px(px(14.0))
                            .rounded(px(8.0))
                            .bg(if matches!(&modal.kind, ModalKind::RemoveDut(_)) {
                                p.red
                            } else {
                                p.accent
                            })
                            .text_size(px(13.0))
                            .font_weight(gpui_kit::FontWeight(700.0))
                            .text_color(p.accent_contrast)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                on_confirm(this, cx);
                                // 确认逻辑成功后置 modal=None（校验失败保留输入）
                                if this.modal.is_none() {
                                    window.close_dialog(cx);
                                }
                            }))
                            .child(SharedString::from(confirm_label.to_string())),
                    ),
            );
        }
        (
            SharedString::from(title.to_string()),
            col.into_any_element(),
        )
    }

    fn modal_text_input(
        &mut self,
        id: In,
        label: &str,
        placeholder: &str,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input = if id == In::NewPresetExe {
            self.path_input(id, placeholder, p, window, cx)
                .into_any_element()
        } else {
            self.text_field(id, &self.input_value(id), placeholder, false, p, window, cx)
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .mb(px(14.))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .child(label.to_owned()),
            )
            .child(input)
            .into_any_element()
    }
}

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
    use duthub_core::model::{PduOutlets, RelayChannels, SnmpConfig};

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

fn demo_dut_template(name: &str, port: &str) -> duthub_core::model::DutConfig {
    duthub_core::model::DutConfig {
        id: format!("dut-{}", name.to_lowercase().replace(' ', "-")),
        name: name.to_string(),
        serial: duthub_core::model::SerialConfig {
            host: "local".into(),
            port: port.to_string(),
            baud: 115_200,
            format: "8N1".into(),
        },
        adb: Default::default(),
        relay: duthub_core::model::RelayConfig::Disabled,
        mcp: duthub_core::model::McpConfig::StreamableHttp {
            url: "http://127.0.0.1:8787/mcp".into(),
        },
        flash: FlashConfig::default(),
        media: Default::default(),
        heartbeat: Default::default(),
    }
}

// 后台任务统一入口：gpui background executor 不可从 &mut Self 直接拿时退化为线程
fn cx_spawn_job(f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(f);
}

impl Render for Workbench {
    #[tracing::instrument(name = "desktop.render", skip_all)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_root(window, cx)
    }
}
