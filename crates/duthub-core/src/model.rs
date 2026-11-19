//! v1 配置 schema（`settings.json` / `duts.json`）。
//!
//! 字段形状与 UI 原型 `settingsFile()` / `dutsFile()` 的 JSON 输出一一对应，
//! 语义见 `docs/DATA_MODEL.md`。serde 容忍未知字段（向前兼容），可选字段带 default。

use serde::{Deserialize, Serialize};

/// `settings.json` 根对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsFile {
    pub version: u32,
    pub appearance: Appearance,
    #[serde(default)]
    pub terminal: TerminalSettings,
    #[serde(default)]
    pub commands: Vec<QuickCommand>,
    #[serde(default, rename = "flashTools")]
    pub flash_tools: Vec<FlashToolPreset>,
    #[serde(default)]
    pub agent: AgentSettings,
    #[serde(default)]
    pub tools: ToolsSettings,
    /// A2A 互联配置（原型 `a2a` 段；token 永不入文件，走密钥库）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a: Option<A2aConfig>,
    /// 工作目录（logs / captures / exports / runtime 等工作文件的输出根）。
    /// None = 可执行文件所在目录。CLI 可覆盖此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dut_storage_gib: Option<u32>,
    /// 弹窗提醒（需要人工干预）设置（需求⑤）：总开关 + 按 source 抑制列表。
    /// 旧 settings.json 无此段 → default（开、无抑制）；等于默认值时序列化
    /// 省略该段（docs/examples/settings.json 往返必须无损，roundtrip.rs 既有测试）。
    #[serde(default, skip_serializing_if = "alerts_is_default")]
    pub alerts: AlertSettings,
    /// Agent 接入统一认证与独占控制（docs/SECURITY.md）：
    /// 凭据 / Grant / auth 开关 / MCP 端口 / 租约 TTL。
    /// 旧 settings.json 无此段 → default（auth 关、无凭据）；等于默认值时
    /// 序列化省略该段（roundtrip.rs 既有测试不动一行仍绿，坑 #19/#24 同款）。
    #[serde(
        default,
        skip_serializing_if = "crate::agent_access::agent_access_is_default"
    )]
    pub agent_access: crate::agent_access::AgentAccessSettings,
}

impl SettingsFile {
    pub fn dut_storage_gib(&self) -> u32 {
        self.dut_storage_gib
            .filter(|limit| *limit > 0)
            .unwrap_or(crate::dut_quota::DEFAULT_DUT_STORAGE_GIB)
    }
}

/// 弹窗提醒设置：`suppressedSources` 存来源稳定 key
/// （`heartbeat` / `serial` / `control` / `agent`，见桌面端 alerts 模块）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertSettings {
    #[serde(default = "default_alerts_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub suppressed_sources: Vec<String>,
}

fn alerts_is_default(alerts: &AlertSettings) -> bool {
    alerts == &AlertSettings::default()
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            enabled: default_alerts_enabled(),
            suppressed_sources: Vec::new(),
        }
    }
}

fn default_alerts_enabled() -> bool {
    true
}

/// A2A 本机服务与远端 peer（仅元数据；credential/token 走 OS 密钥库）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aConfig {
    #[serde(default = "default_a2a_agent_name")]
    pub agent_name: String,
    #[serde(default = "default_a2a_port")]
    pub port: u16,
    /// 永远 loopback（SECURITY：不得绑定对外地址）
    #[serde(default = "default_a2a_host")]
    pub host: String,
    #[serde(default)]
    pub peers: Vec<A2aPeer>,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            agent_name: default_a2a_agent_name(),
            port: default_a2a_port(),
            host: default_a2a_host(),
            peers: Vec::new(),
        }
    }
}

fn default_a2a_agent_name() -> String {
    "Duthub Agent".into()
}
fn default_a2a_port() -> u16 {
    8790
}
fn default_a2a_host() -> String {
    "127.0.0.1".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aPeer {
    pub id: String,
    pub name: String,
    pub card_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Appearance {
    pub theme: String,
    pub language: String,
    pub platform: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Legacy terminal scale, used when no explicit font size is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_percent: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_logs: Option<bool>,
    #[serde(default)]
    pub jump_to_latest: bool,
    #[serde(default)]
    pub show_command_bar: bool,
    #[serde(default)]
    pub tab_numbers: bool,
    #[serde(default)]
    pub tab_status_icons: bool,
    #[serde(default)]
    pub include_ansi: bool,
    #[serde(default)]
    pub prompt_highlight: PromptHighlight,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptHighlight {
    #[serde(default)]
    pub regex: String,
    #[serde(default = "default_match_color")]
    pub color: String,
}

fn default_match_color() -> String {
    "#edbe7a".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuickCommand {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub icon: String,
}

/// 设置页的烧录工具预设库；`testArgs` 是单条字符串（与 DUT 上的数组形状不同）。
///
/// `id` 是稳定预设标识（`preset-…`）：下拉/选择按 id 定位预设，
/// 不再用可执行文件路径区分（同一工具多套参数时按路径会选错）。
/// 旧 settings.json 无 `id` 字段时反序列化为空串，加载时由
/// `store::load_settings` 归一化补齐并持久化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlashToolPreset {
    /// 归一化前（旧文件）为空；序列化时省略以保持旧文件形状
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub name: String,
    pub executable: String,
    #[serde(
        default = "default_test_args",
        deserialize_with = "string_or_list",
        serialize_with = "serialize_single_string"
    )]
    pub test_args: String,
}

fn default_test_args() -> String {
    "--help".into()
}

/// 兼容旧配置：testArgs 可能写成字符串或数组（两者都接受）。
fn string_or_list<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Str(String),
        List(Vec<String>),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Str(s) => Ok(s),
        // 旧数组形状：按空白拆回单串（与编辑表单的拆分规则一致）
        Raw::List(items) => Ok(items.join(" ")),
    }
}

/// 统一以单条字符串序列化（保持 settings.json 的既有形状）。
fn serialize_single_string<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(value)
}

/// 兼容旧配置的 `testArgs`：字符串或数组都接受，运行时统一成参数数组。
fn string_or_list_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Str(String),
        List(Vec<String>),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Str(s) => Ok(s.split_whitespace().map(str::to_string).collect()),
        Raw::List(items) => Ok(items),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ModelProvider>,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub fast: String,
    #[serde(default)]
    pub slow: String,
    #[serde(default)]
    pub secondary: String,
    #[serde(default = "default_turns")]
    pub turns: u32,
    #[serde(default = "default_parallel")]
    pub parallel: u32,
    #[serde(default)]
    pub compare: bool,
    #[serde(default)]
    pub approve: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub protocol: crate::llm::Provider,
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    pub enabled: bool,
}

fn default_turns() -> u32 {
    10
}
fn default_parallel() -> u32 {
    4
}

fn default_debug_tool() -> String {
    "adb".into()
}

fn default_hdc() -> String {
    "hdc".into()
}

/// 坑 #19：默认值段序列化跳过，保证 examples 往返语义相等（旧 settings.json 无 hdcExecutable 键）。
fn is_default_hdc(tool: &str) -> bool {
    tool == "hdc"
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsSettings {
    #[serde(default = "default_adb")]
    pub adb_executable: String,
    /// OpenHarmony hdc 可执行文件（PATH 命令名或绝对路径）
    #[serde(default = "default_hdc", skip_serializing_if = "is_default_hdc")]
    pub hdc_executable: String,
    #[serde(default, skip_serializing_if = "capture_is_empty")]
    pub capture: CaptureSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureSettings {
    pub ffmpeg: String,
    pub ffplay: String,
    pub listen_ip: String,
}

fn capture_is_empty(capture: &CaptureSettings) -> bool {
    capture == &CaptureSettings::default()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DutMediaConfig {
    pub hdmi: CaptureInput,
    pub camera: CaptureInput,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureInput {
    /// Older configurations enable an input by specifying a video device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub video: String,
    pub audio: String,
    pub video_size: String,
    pub frame_rate: String,
    /// Empty or zero selects an available port.
    pub port: String,
}

impl CaptureInput {
    pub fn is_enabled(&self) -> bool {
        self.enabled
            .unwrap_or_else(|| !self.video.trim().is_empty())
    }

    pub fn has_video(&self) -> bool {
        self.is_enabled() && !self.video.trim().is_empty()
    }
}

fn default_adb() -> String {
    "adb".into()
}

/// `duts.json` 根对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DutsFile {
    pub version: u32,
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relay_boards: Vec<RelayBoard>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub alerts_by_dut: std::collections::BTreeMap<String, AlertSettings>,
    #[serde(default)]
    pub duts: Vec<DutConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayBoard {
    pub id: String,
    pub name: String,
    pub host: String,
    pub node: String,
    pub driver: String,
    #[serde(default = "default_relay_channels")]
    pub channel_count: u8,
}

fn default_relay_channels() -> u8 {
    8
}

impl DutsFile {
    /// 把旧配置中重复的继电器串口定义收口到板卡；原通道保持不变。
    pub fn migrate_usb_relays(&mut self) {
        for dut in &mut self.duts {
            let RelayConfig::UsbSerial {
                host,
                node,
                driver,
                channels,
            } = &dut.relay
            else {
                continue;
            };
            let channel_count = channels
                .assigned()
                .max()
                .unwrap_or(0)
                .max(default_relay_channels());
            let Some(board_id) = (!node.trim().is_empty()).then(|| {
                if let Some(board) = self.relay_boards.iter_mut().find(|board| {
                    board.host == *host
                        && (board.node == *node
                            || (cfg!(windows) && board.node.eq_ignore_ascii_case(node)))
                }) {
                    board.channel_count = board.channel_count.max(channel_count);
                    board.id.clone()
                } else {
                    let id = format!("relay-{}", uuid::Uuid::new_v4());
                    self.relay_boards.push(RelayBoard {
                        id: id.clone(),
                        name: node.clone(),
                        host: host.clone(),
                        node: node.clone(),
                        driver: driver.clone(),
                        channel_count,
                    });
                    id
                }
            }) else {
                continue;
            };
            dut.relay = RelayConfig::UsbBoard {
                board_id,
                channels: channels.clone(),
            };
        }
    }

    pub fn resolve_relay(&self, relay: &RelayConfig) -> RelayConfig {
        let RelayConfig::UsbBoard { board_id, channels } = relay else {
            return relay.clone();
        };
        self.relay_boards
            .iter()
            .find(|board| board.id == *board_id)
            .map_or(RelayConfig::Disabled, |board| RelayConfig::UsbSerial {
                host: board.host.clone(),
                node: board.node.clone(),
                driver: board.driver.clone(),
                channels: channels.clone(),
            })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub ports: Vec<PortInfo>,
    #[serde(default)]
    pub adb_devices: Vec<AdbDeviceInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortInfo {
    pub id: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub vid: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdbDeviceInfo {
    pub id: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DutConfig {
    pub id: String,
    pub name: String,
    pub serial: SerialConfig,
    #[serde(default)]
    pub adb: AdbConfig,
    #[serde(default = "RelayConfig::disabled")]
    pub relay: RelayConfig,
    #[serde(default = "McpConfig::default_http")]
    pub mcp: McpConfig,
    #[serde(default)]
    pub flash: FlashConfig,
    #[serde(default, skip_serializing_if = "media_is_empty")]
    pub media: DutMediaConfig,
    /// 会话心跳（需求⑦）：配置缺省 = 关闭；Agent 可经 heartbeat.set 改写。
    #[serde(default, skip_serializing_if = "heartbeat_is_default")]
    pub heartbeat: crate::heartbeat::HeartbeatConfig,
}

fn media_is_empty(media: &DutMediaConfig) -> bool {
    media == &DutMediaConfig::default()
}

fn heartbeat_is_default(heartbeat: &crate::heartbeat::HeartbeatConfig) -> bool {
    heartbeat.is_default()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialConfig {
    pub host: String,
    pub port: String,
    #[serde(default = "default_baud")]
    pub baud: u32,
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_baud() -> u32 {
    115_200
}
fn default_format() -> String {
    "8N1".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdbConfig {
    #[serde(default = "default_adb_mode")]
    pub mode: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub serial: String,
    /// USB 调试通道工具：adb | hdc（OpenHarmony）
    #[serde(
        default = "default_debug_tool",
        skip_serializing_if = "is_default_debug_tool"
    )]
    pub tool: String,
}

/// 坑 #19：默认值段序列化跳过，保证 examples 往返语义相等（旧 duts.json 无 tool 键）。
fn is_default_debug_tool(tool: &str) -> bool {
    tool == "adb"
}

impl Default for AdbConfig {
    fn default() -> Self {
        AdbConfig {
            mode: default_adb_mode(),
            host: String::new(),
            serial: String::new(),
            tool: "adb".into(),
        }
    }
}

impl AdbConfig {
    pub fn auto_detect(&self) -> bool {
        self.tool != "hdc" && self.mode == "auto"
    }
}

fn default_adb_mode() -> String {
    "disabled".into()
}

/// 控制单元绑定（JSON tag: `mode`）。SNMP PDU 只暴露 Power，不伪造 Reset/Recovery。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RelayConfig {
    Disabled,
    UsbSerial {
        #[serde(default)]
        host: String,
        #[serde(default)]
        node: String,
        #[serde(default)]
        driver: String,
        #[serde(default)]
        channels: RelayChannels,
    },
    UsbBoard {
        #[serde(rename = "boardId")]
        board_id: String,
        #[serde(default)]
        channels: RelayChannels,
    },
    SnmpPdu {
        #[serde(default)]
        snmp: SnmpConfig,
        #[serde(default)]
        outlets: PduOutlets,
    },
}

impl RelayConfig {
    fn disabled() -> Self {
        RelayConfig::Disabled
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayChannels {
    #[serde(default)]
    pub power: Option<u8>,
    #[serde(default)]
    pub reset: Option<u8>,
    #[serde(default)]
    pub recovery: Option<u8>,
    #[serde(default)]
    pub maskrom: Option<u8>,
    /// 电源按键（Rockchip power key，短接脉冲触发）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_key: Option<u8>,
}

impl RelayChannels {
    pub fn assigned(&self) -> impl Iterator<Item = u8> {
        [
            self.power,
            self.reset,
            self.recovery,
            self.power_key,
            self.maskrom,
        ]
        .into_iter()
        .flatten()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnmpConfig {
    #[serde(default)]
    pub address: String,
    #[serde(default = "default_snmp_version")]
    pub version: String,
    /// 指向 OS 密钥库的引用，永不存明文（docs/SECURITY.md #5）。
    #[serde(default)]
    pub credential_ref: String,
}

fn default_snmp_version() -> String {
    "3".into()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PduOutlets {
    #[serde(default = "default_outlet")]
    pub power: u16,
}

fn default_outlet() -> u16 {
    1
}

/// MCP 传输（JSON tag: `transport`）。stdio 无 TCP 端口；HTTP 必须是完整 URL。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpConfig {
    Stdio {
        #[serde(default)]
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: String,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
    },
    StreamableHttp {
        #[serde(default)]
        url: String,
    },
}

impl McpConfig {
    fn default_http() -> Self {
        McpConfig::StreamableHttp { url: String::new() }
    }
}

/// DUT 上的烧录配置是预设的复制快照；`testArgs` 是数组。
///
/// `presetId` 记录快照来源预设的稳定 id；旧 duts.json 可能没有该字段，
/// 也可能把 `testArgs` 写成字符串 —— 两者都在加载时兼容（运行时统一为参数数组）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlashConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    #[serde(default)]
    pub executable: String,
    #[serde(default, deserialize_with = "string_or_list_vec")]
    pub test_args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_preset_parses_without_id() {
        // 旧 settings.json：无 id 字段
        let raw = r#"{"name":"upgrade_tool","executable":"upgrade_tool","testArgs":"--help"}"#;
        let preset: FlashToolPreset = serde_json::from_str(raw).unwrap();
        assert!(preset.id.is_empty());
        assert_eq!(preset.test_args, "--help");
    }

    #[test]
    fn flash_preset_accepts_legacy_list_test_args() {
        // 更旧配置：testArgs 写成数组
        let raw = r#"{"name":"t","executable":"t","testArgs":["--help","-h"]}"#;
        let preset: FlashToolPreset = serde_json::from_str(raw).unwrap();
        assert_eq!(preset.test_args, "--help -h");
        // 序列化保持单条字符串形状
        let back = serde_json::to_value(&preset).unwrap();
        assert_eq!(back["testArgs"], serde_json::json!("--help -h"));
    }

    #[test]
    fn dut_flash_config_accepts_legacy_string_test_args() {
        // 旧 duts.json：testArgs 是字符串、无 presetId
        let raw = r#"{"executable":"upgrade_tool","testArgs":"--help"}"#;
        let cfg: FlashConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.preset_id, None);
        assert_eq!(cfg.test_args, vec!["--help".to_string()]);

        let raw2 = r#"{"executable":"upgrade_tool","testArgs":"--help --version"}"#;
        let cfg2: FlashConfig = serde_json::from_str(raw2).unwrap();
        assert_eq!(
            cfg2.test_args,
            vec!["--help".to_string(), "--version".to_string()]
        );
    }

    #[test]
    fn dut_flash_config_roundtrips_preset_id_and_list() {
        let cfg = FlashConfig {
            preset_id: Some("preset-upgrade_tool".into()),
            executable: "upgrade_tool".into(),
            test_args: vec!["--help".into()],
        };
        let value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(value["presetId"], serde_json::json!("preset-upgrade_tool"));
        assert_eq!(value["testArgs"], serde_json::json!(["--help"]));
        let back: FlashConfig = serde_json::from_value(value).unwrap();
        assert_eq!(back, cfg);
        // 无 presetId 时序列化省略字段（向前兼容旧读取方）
        let bare = FlashConfig {
            preset_id: None,
            executable: "t".into(),
            test_args: Vec::new(),
        };
        let value = serde_json::to_value(&bare).unwrap();
        assert!(value.get("presetId").is_none());
    }

    #[test]
    fn alert_settings_roundtrips_camel_case_and_defaults() {
        // 旧 settings.json 无 alerts 段 → 默认开启、无抑制
        let legacy: AlertSettings = serde_json::from_str("{}").unwrap();
        assert!(legacy.enabled);
        assert!(legacy.suppressed_sources.is_empty());
        // 往返必须写 camelCase 键（snake_case 会被静默丢弃落 default，坑 #15）
        let value = serde_json::json!({
            "enabled": false,
            "suppressedSources": ["serial", "agent"]
        });
        let cfg: AlertSettings = serde_json::from_value(value).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.suppressed_sources, vec!["serial", "agent"]);
        let back = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            back["suppressedSources"],
            serde_json::json!(["serial", "agent"])
        );
    }
}
