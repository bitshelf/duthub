//! v1 configuration schema (`settings.json` / `duts.json`).
//!
//! Field shapes match the prototype `settingsFile()` / `dutsFile()` JSON output; see
//! `docs/DATA_MODEL.md` for semantics. Unknown fields are tolerated by `serde` for forward
//! compatibility, and optional fields carry defaults.

use serde::{Deserialize, Serialize};

/// Root object of `settings.json`.
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
    /// A2A interconnection settings (the `a2a` section; tokens never touch disk and live in the
    /// OS keyring).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a: Option<A2aConfig>,
    /// Working directory root for logs, captures, exports, and runtime data.
    /// `None` defaults to the executable's directory; CLI flags can override this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dut_storage_gib: Option<u32>,
    /// Global pop-up alert settings: only the per-source suppression list.
    /// The legacy global on/off switch has been removed; an old
    /// `enabled` key is ignored by serde and every DUT defaults to enabled
    /// unless its `alertsByDut` entry disables it. The section is omitted from
    /// disk when empty (docs/examples/settings.json roundtrip stays lossless).
    #[serde(default, skip_serializing_if = "global_alerts_is_default")]
    pub alerts: GlobalAlertSettings,
    /// Unified Agent access authentication and exclusive control settings (`docs/SECURITY.md`):
    /// credentials, grants, auth toggle, MCP port, and lease TTL.
    /// Legacy `settings.json` files without this section deserialize to the default (auth off, no
    /// credentials), and the section is omitted on serialize when equal to the default so
    /// `roundtrip.rs` stays unchanged.
    #[serde(
        default,
        skip_serializing_if = "crate::agent_access::agent_access_is_default"
    )]
    pub agent_access: crate::agent_access::AgentAccessSettings,
    /// Serial terminal settings (top-level `serial` section alongside control devices): prompt
    /// highlight rules, key mappings, scrollback buffer, and session log options.
    /// Legacy `settings.json` files without this section deserialize to defaults, and the section
    /// is omitted on serialize when equal to the default so `roundtrip.rs` stays unchanged.
    #[serde(default, skip_serializing_if = "serial_is_default")]
    pub serial: SerialSettings,
}

impl SettingsFile {
    #[must_use]
    pub fn dut_storage_gib(&self) -> u32 {
        self.dut_storage_gib
            .filter(|limit| *limit > 0)
            .unwrap_or(crate::dut_quota::DEFAULT_DUT_STORAGE_GIB)
    }
}

/// Per-DUT pop-up alert settings (`duts.json` `alertsByDut`):
/// `suppressedSources` stores stable source keys (`heartbeat` / `serial` /
/// `serialGarbled` / `control` / `agent`, see the desktop alerts module).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertSettings {
    #[serde(default = "default_alerts_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub suppressed_sources: Vec<String>,
}

/// Global pop-up alert settings (`settings.json` `alerts`): per-source
/// suppression list and taskbar/tray notification mute toggle. There is
/// deliberately no global on/off switch any more; a DUT is silenced by
/// `duts.json` `alertsByDut.<id>.enabled = false`, and non-DUT subjects (relay
/// boards, generic agent notices) can only be silenced by suppressing a source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalAlertSettings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed_sources: Vec<String>,
    /// Suppress taskbar button flashing and tray balloon/icon alerts while hidden or minimized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub mute_taskbar_alerts: bool,
}

/// Per-DUT garbled-byte detection switch (`duts.json` `garbledByDut`).
/// Detection is opt-in: an absent entry means "off", and a map with no enabled
/// entries is omitted from disk entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GarbledDetection {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
}

fn global_alerts_is_default(alerts: &GlobalAlertSettings) -> bool {
    alerts == &GlobalAlertSettings::default()
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

/// Local A2A metadata and remote peer directory (tokens live in the OS keyring).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aConfig {
    #[serde(default = "default_a2a_agent_name")]
    pub agent_name: String,
    #[serde(default = "default_a2a_port")]
    pub port: u16,
    #[serde(default)]
    pub peers: Vec<A2aPeer>,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            agent_name: default_a2a_agent_name(),
            port: default_a2a_port(),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aPeer {
    pub id: String,
    pub name: String,
    pub card_url: String,
}

/// Per-DUT A2A pre-configuration (runtime lives in `duthub-a2a`; tokens live in the OS keyring at
/// `a2a/<dut_id>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DutA2aConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_a2a_port")]
    pub port: u16,
    /// When omitted, the UI falls back to the DUT name.
    #[serde(default)]
    pub agent_name: String,
    /// Listener bind address; `None` / omitted defaults to `127.0.0.1` (loopback only).
    /// Non-loopback or other bind values are classified as `NeedsProductSignoff` at runtime and
    /// rejected (ADR-2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
}

impl Default for DutA2aConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_a2a_port(),
            agent_name: String::new(),
            bind_addr: None,
        }
    }
}

fn dut_a2a_is_default(config: &DutA2aConfig) -> bool {
    config == &DutA2aConfig::default()
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "flat JSON settings schema preserved for settings.json compatibility"
)]
pub struct TerminalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Scroll back to the bottom whenever new terminal output arrives.
    #[serde(default, skip_serializing_if = "is_false")]
    pub scroll_on_output: bool,
    /// Scroll back to the bottom when typing or pasting into the terminal.
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
}

fn default_match_color() -> String {
    "#edbe7a".into()
}

/// Serial terminal settings (top-level `serial` section).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialSettings {
    /// Prompt regex highlight rules: applied in order, first-match wins without overlap. Legacy
    /// `terminal.promptHighlight` (single rule, never wired to rendering) is seeded first on load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlight_rules: Vec<PromptRule>,
    /// Keyboard backspace byte (`None` = `0x7f` DEL; `CtrlH` = `0x08` for shells/tools that expect it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backspace: Option<SerialBackspace>,
    /// Keyboard Enter line ending (`None` = CR; `Lf` / `Crlf` optional; keyboard path only, MCP
    /// line endings are configured separately).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter_ending: Option<SerialEnterEnding>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub paste_on_right_click: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub copy_on_select: bool,
    /// Terminal scrollback buffer size in rows (`None` = 1000; clamped to the desktop render cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_rows: Option<u32>,
}

fn serial_is_default(serial: &SerialSettings) -> bool {
    serial == &SerialSettings::default()
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// Single prompt regex highlight rule (render-time overlay only; does not alter the byte stream).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRule {
    #[serde(default)]
    pub regex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_match_color")]
    pub color: String,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
}

impl PromptRule {
    pub const USER_HOST_PATTERN: &str = r"[A-Za-z0-9_.-]+@[A-Za-z0-9_.-]+";
    pub(crate) const OLD_ANDROID_HOST_PATTERN: &str = r"^[0-9]+\|[A-Za-z0-9_.-]+";
    pub const ANDROID_HOST_PATTERN: &str =
        r"^(?P<highlight>(?:[0-9]+\|)?[A-Za-z0-9_.-]+):[~/][^\s$#]*[ \t]*[$#](?:\s|$)";
    pub const PATH_PATTERN: &str = r":[~/][^\s$#]*";
    pub const SUFFIX_PATTERN: &str = r"[$#]\s*$";
}

impl Default for PromptRule {
    fn default() -> Self {
        Self {
            regex: String::new(),
            description: None,
            color: default_match_color(),
            enabled: default_rule_enabled(),
        }
    }
}

/// Built-in prompt highlight rules, modelled on sermcp's semantic overlay:
/// `user@host` green, Android `status|host` green, the prompt path blue and the
/// trailing `$`/`#` sigil near-white. The rule engine is the plain `regex`
/// crate (no look-around), so the path rule includes the colon and the sigil
/// rule anchors at the end of the row. Callers seed these only when the user
/// has no rules configured; every entry stays editable, disableable and
/// deletable like a hand-written one.
#[must_use]
pub fn default_highlight_rules() -> Vec<PromptRule> {
    vec![
        PromptRule {
            regex: PromptRule::USER_HOST_PATTERN.into(),
            description: None,
            color: "#7ec699".into(),
            enabled: true,
        },
        PromptRule {
            regex: PromptRule::ANDROID_HOST_PATTERN.into(),
            description: None,
            color: "#7ec699".into(),
            enabled: true,
        },
        PromptRule {
            regex: PromptRule::PATH_PATTERN.into(),
            description: None,
            color: "#56a8f5".into(),
            enabled: true,
        },
        PromptRule {
            regex: PromptRule::SUFFIX_PATTERN.into(),
            description: None,
            color: "#c8ccd4".into(),
            enabled: true,
        },
    ]
}

fn default_rule_enabled() -> bool {
    true
}

/// Keyboard backspace byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SerialBackspace {
    /// `0x7f` (DEL, default erase byte for most Linux shells).
    Del,
    /// `0x08` (Ctrl-H, needed by some BSP shells and legacy tools).
    CtrlH,
}

/// Keyboard Enter line ending (keyboard path only; MCP/heartbeat writes use their own session
/// line-ending policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SerialEnterEnding {
    /// `\r` (default).
    Cr,
    /// `\n`.
    Lf,
    /// `\r\n`.
    Crlf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuickCommand {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub icon: String,
}

/// Settings-level flash tool preset; `testArgs` is stored as a single CLI argument string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlashToolPreset {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub name: String,
    pub executable: String,
    #[serde(default = "default_test_args")]
    pub test_args: String,
}

fn default_test_args() -> String {
    "--help".into()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ModelProvider>,
    /// Explicitly selected provider id from the Agent model catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider_id: Option<String>,
    /// Explicitly selected model id paired with [`AgentSettings::active_provider_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_model_id: Option<String>,
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

fn default_true() -> bool {
    true
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn is_true(value: &bool) -> bool {
    *value
}

/// Supported input modalities for a configured LLM model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent LLM input modality checkboxes persisted in settings.json"
)]
pub struct ModelInputModalities {
    /// Text input is always enabled.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub text: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub image: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub video: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pdf: bool,
}

impl Default for ModelInputModalities {
    fn default() -> Self {
        Self {
            text: true,
            image: false,
            video: false,
            pdf: false,
        }
    }
}

impl ModelInputModalities {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// True when the model accepts visual inputs (images or video).
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.image || self.video
    }
}

/// Optional model feature flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    #[serde(default, skip_serializing_if = "is_false")]
    pub structured_output: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub web_search: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub mid_conversation_system: bool,
}

impl ModelCapabilities {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub id: String,
    pub enabled: bool,
    /// Context window size in tokens (e.g. `1_000_000`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Maximum output tokens per response (e.g. `128_000`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "ModelInputModalities::is_default")]
    pub input_modalities: ModelInputModalities,
    #[serde(default, skip_serializing_if = "ModelCapabilities::is_default")]
    pub capabilities: ModelCapabilities,
    /// Reasoning effort levels from low to high (e.g. `["low", "high", "max"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_levels: Vec<String>,
    /// Optional JSON object mapping reasoning parameters for the provider API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_params: Option<serde_json::Value>,
}

impl ProviderModel {
    #[must_use]
    pub fn new(id: impl Into<String>, enabled: bool) -> Self {
        Self {
            id: id.into(),
            enabled,
            ..Self::default()
        }
    }
}

fn default_debug_tool() -> String {
    "adb".into()
}

fn default_hdc() -> String {
    "hdc".into()
}

/// Skip serializing the default value so example round-trips remain semantically identical
/// (legacy `settings.json` has no `hdcExecutable` key).
fn is_default_hdc(tool: &str) -> bool {
    tool == "hdc"
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsSettings {
    #[serde(default = "default_adb")]
    pub adb_executable: String,
    /// `OpenHarmony` `hdc` executable (PATH command name or absolute path).
    #[serde(default = "default_hdc", skip_serializing_if = "is_default_hdc")]
    pub hdc_executable: String,
    #[serde(default, skip_serializing_if = "capture_is_empty")]
    pub capture: CaptureSettings,
    /// Download kernel (`duthub-tooldown`) settings: proxy policy for fetching
    /// external tools (adb/ffmpeg …) into `<config>/tools/`.
    /// Old settings.json has no such section → default (follow env proxy); the
    /// section is omitted on serialize when it equals the default
    /// (roundtrip.rs stays green, pitfalls #19/#24).
    #[serde(default, skip_serializing_if = "tool_download_is_default")]
    pub download: ToolDownloadSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureSettings {
    pub ffmpeg: String,
    pub ffplay: String,
    pub listen_ip: String,
    /// Idle seconds before an Agent-opened ephemeral capture is reclaimed.
    /// `None` = the desktop default (60s); hand-edited out-of-range values fall
    /// back to that default at read time (`capture_backend::effective_idle`).
    /// The key is skipped when absent so the default section still serializes
    /// to nothing (`capture_is_empty`, pitfalls #19/#24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u32>,
}

fn capture_is_empty(capture: &CaptureSettings) -> bool {
    capture == &CaptureSettings::default()
}

/// The `tools.download` section: download preferences for the install kernel
/// (duthub-tooldown). Configuration only — the desktop side maps `mode`/`url`
/// onto the kernel's proxy config (this crate does not depend on it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDownloadSettings {
    #[serde(default, skip_serializing_if = "download_proxy_is_default")]
    pub proxy: DownloadProxySettings,
    /// User overrides for the built-in tools' archive URLs.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub urls: std::collections::BTreeMap<String, String>,
}

fn tool_download_is_default(download: &ToolDownloadSettings) -> bool {
    download == &ToolDownloadSettings::default()
}

/// Download proxy policy: `off` ignores proxy environment variables
/// (direct connection), `env` follows HTTP(S)_PROXY and friends (default),
/// `custom` forces `url` (http/https/socks5, see duthub-tooldown's
/// `ProxyConfig`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProxySettings {
    #[serde(default, skip_serializing_if = "proxy_mode_is_default")]
    pub mode: DownloadProxyMode,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

fn download_proxy_is_default(proxy: &DownloadProxySettings) -> bool {
    proxy == &DownloadProxySettings::default()
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn proxy_mode_is_default(mode: &DownloadProxyMode) -> bool {
    *mode == DownloadProxyMode::default()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadProxyMode {
    /// Direct connection: ignore proxy environment variables entirely.
    Off,
    /// Follow the standard proxy environment variables (`HTTP_PROXY` etc.).
    #[default]
    Env,
    /// Force the configured proxy URL (http/https/socks5).
    Custom,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DutMediaConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub hdmi: CaptureInput,
    pub camera: CaptureInput,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<CaptureDevice>,
}

impl DutMediaConfig {
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled != Some(false)
            && (self.hdmi != CaptureInput::default()
                || self.camera != CaptureInput::default()
                || !self.devices.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureDevice {
    pub id: u32,
    #[serde(flatten)]
    pub input: CaptureInput,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureInput {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
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
    #[must_use]
    pub fn same_capture(&self, other: &Self) -> bool {
        self.is_enabled() == other.is_enabled()
            && self.video == other.video
            && self.audio == other.audio
            && self.video_size == other.video_size
            && self.frame_rate == other.frame_rate
            && self.port == other.port
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
            .unwrap_or_else(|| !self.video.trim().is_empty())
    }

    #[must_use]
    pub fn has_video(&self) -> bool {
        self.is_enabled() && !self.video.trim().is_empty()
    }
}

fn default_adb() -> String {
    "adb".into()
}

/// Root object of `duts.json`.
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
    /// Per-DUT garbled-byte detection switches; an absent entry means "off"
    /// and an all-off map is omitted so legacy duts.json round-trips unchanged.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub garbled_by_dut: std::collections::BTreeMap<String, GarbledDetection>,
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
    /// Consolidate duplicated legacy USB serial relay definitions into shared relay boards while
    /// preserving original channel assignments.
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

    #[must_use]
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
    /// Per-DUT A2A pre-configuration (omitted when default to keep legacy `duts.json` round-trips
    /// unchanged).
    #[serde(default, skip_serializing_if = "dut_a2a_is_default")]
    pub a2a: DutA2aConfig,
    #[serde(default)]
    pub flash: FlashConfig,
    #[serde(default, skip_serializing_if = "media_is_empty")]
    pub media: DutMediaConfig,
    /// Session heartbeat (omitted when default = disabled; agents may update via `heartbeat.set`).
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
    /// Optional reference capture used to locate the beginning of the next boot cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_log: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub split_logs: bool,
    /// Minimum pairwise similarity required when learning a boot reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_similarity: Option<u8>,
}

pub const DEFAULT_BOOT_SIMILARITY: u8 =
    match u8::from_str_radix(env!("DUTHUB_DEFAULT_BOOT_SIMILARITY"), 10) {
        Ok(percent @ 1..=100) => percent,
        _ => panic!("Default boot similarity must be an integer between 1 and 100"),
    };

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
    /// USB debug channel tool: `adb` | `hdc` (`OpenHarmony`).
    #[serde(
        default = "default_debug_tool",
        skip_serializing_if = "is_default_debug_tool"
    )]
    pub tool: String,
}

/// Skip serializing the default value so example round-trips remain semantically identical
/// (legacy `duts.json` has no `tool` key).
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
    #[must_use]
    pub fn auto_detect(&self) -> bool {
        self.tool != "hdc" && self.mode == "auto"
    }
}

fn default_adb_mode() -> String {
    "disabled".into()
}

/// Control unit binding (JSON tag: `mode`). SNMP PDUs expose only Power and do not emulate Reset
/// or Recovery.
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
    /// Power key channel (Rockchip power key, triggered via a short-circuit pulse).
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
    /// Reference to an OS keyring entry; plaintext secrets are never stored (`docs/SECURITY.md` #5).
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

/// MCP transport (JSON tag: `transport`). Stdio uses no TCP port; HTTP requires a full URL.
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

/// Per-DUT flash configuration snapshot copied from a [`FlashToolPreset`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlashConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub test_args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_preset_parses_without_id() {
        let raw = r#"{"name":"upgrade_tool","executable":"upgrade_tool","testArgs":"--help"}"#;
        let preset: FlashToolPreset = serde_json::from_str(raw).unwrap();
        assert_eq!(preset.id, "");
        assert_eq!(preset.test_args, "--help");
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
        // Omit `presetId` when absent for backward compatibility with older readers.
        let bare = FlashConfig {
            preset_id: None,
            executable: "t".into(),
            test_args: Vec::new(),
        };
        let value = serde_json::to_value(&bare).unwrap();
        assert!(value.get("presetId").is_none());
    }

    #[test]
    fn dut_a2a_defaults_off_and_roundtrips_camel_case() {
        // Legacy `duts.json` without `a2a`: defaults to disabled and is omitted on serialize
        // (keeps example round-trip unchanged).
        let legacy: DutConfig = serde_json::from_value(serde_json::json!({
            "id": "dut-a",
            "name": "RK3576",
            "serial": {"host": "local", "port": "COM8", "baud": 115_200, "format": "8N1"}
        }))
        .unwrap();
        assert!(!legacy.a2a.enabled);
        assert_eq!(legacy.a2a.port, 8790);
        assert_eq!(legacy.a2a.agent_name, "");

        let back = serde_json::to_value(&legacy).unwrap();
        assert!(
            back.get("a2a").is_none(),
            "default a2a section must be omitted: {back}"
        );

        let mut enabled = legacy;
        enabled.a2a.enabled = true;
        enabled.a2a.port = 8791;
        enabled.a2a.agent_name = "RK3576 A2A".into();
        let value = serde_json::to_value(&enabled).unwrap();
        assert_eq!(value["a2a"]["enabled"], serde_json::json!(true));
        assert_eq!(value["a2a"]["agentName"], serde_json::json!("RK3576 A2A"));
        let parsed: DutConfig = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.a2a, enabled.a2a);
    }

    #[test]
    fn dut_a2a_bind_addr_optional_and_camel_case() {
        let base_json = serde_json::json!({
            "id": "dut-a",
            "name": "RK3576",
            "serial": {"host": "local", "port": "COM8", "baud": 115_200, "format": "8N1"}
        });

        // Default is `None`; `bindAddr` is omitted on serialize (whole-section omission unchanged).
        let mut dut: DutConfig = serde_json::from_value(base_json.clone()).unwrap();
        dut.a2a.enabled = true;
        let value = serde_json::to_value(&dut).unwrap();
        assert!(value["a2a"].get("bindAddr").is_none(), "{value}");

        // Explicit loopback address round-trips with camelCase key.
        let mut bound: DutConfig = serde_json::from_value(base_json).unwrap();
        bound.a2a.bind_addr = Some("127.0.0.1".into());
        let value = serde_json::to_value(&bound).unwrap();
        assert_eq!(value["a2a"]["bindAddr"], serde_json::json!("127.0.0.1"));
        let parsed: DutConfig = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.a2a, bound.a2a);
    }

    #[test]
    fn default_highlight_rules_cover_prompt_parts() {
        let rules = default_highlight_rules();
        assert!(
            rules.len() >= 3,
            "default rules must cover user@host, path, and prompt suffix"
        );
        for rule in &rules {
            assert!(
                !rule.regex.trim().is_empty(),
                "default rule regex must not be empty"
            );
            assert!(rule.enabled, "default rule must be enabled");
            assert!(
                rule.color.starts_with('#') && rule.color.len() == 7,
                "color must be #rrggbb: {}",
                rule.color
            );
        }
        // Distinct colors per semantic group: user@host green, path blue, suffix light gray.
        let colors: std::collections::HashSet<&str> =
            rules.iter().map(|rule| rule.color.as_str()).collect();
        assert_eq!(
            colors.len(),
            3,
            "each semantic group should have its own color"
        );
        // Empty/default contract stays unchanged (seeding happens only on load, not roundtrip).
        assert_eq!(
            SerialSettings::default().highlight_rules,
            Vec::<PromptRule>::new()
        );
    }

    #[test]
    fn alert_settings_roundtrips_camel_case_and_defaults() {
        // Legacy `settings.json` without `alerts` defaults to enabled with no suppressions.
        let legacy: AlertSettings = serde_json::from_str("{}").unwrap();
        assert!(legacy.enabled);
        assert_eq!(legacy.suppressed_sources, Vec::<String>::new());
        // Round-trip must emit camelCase keys (`snake_case` would silently fall back to default).
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

    #[test]
    fn global_alerts_ignore_the_legacy_enabled_key() {
        // The global on/off switch is gone: an old `enabled: false` must not
        // silence anything, the suppression list still loads, and re-saving
        // drops the retired key.
        let legacy: GlobalAlertSettings =
            serde_json::from_str(r#"{"enabled":false,"suppressedSources":["relay"]}"#).unwrap();
        assert_eq!(legacy.suppressed_sources, vec!["relay"]);
        let back = serde_json::to_value(&legacy).unwrap();
        assert_eq!(back, serde_json::json!({"suppressedSources": ["relay"]}));
        assert!(
            back.get("enabled").is_none(),
            "retired global enabled key must not be written back: {back}"
        );
        // An empty section is omitted from settings.json entirely (the field is
        // skipped when it equals the default), so old files stay unchanged.
        let empty = GlobalAlertSettings::default();
        assert!(global_alerts_is_default(&empty));
        let raw = serde_json::to_value(crate::store::default_settings()).unwrap();
        assert!(
            raw.get("alerts").is_none(),
            "default alerts section must not be serialized: {raw}"
        );
        let muted = GlobalAlertSettings {
            suppressed_sources: Vec::new(),
            mute_taskbar_alerts: true,
        };
        assert!(!global_alerts_is_default(&muted));
        let serialized = serde_json::to_value(&muted).unwrap();
        assert_eq!(serialized, serde_json::json!({"muteTaskbarAlerts": true}));
        let parsed: GlobalAlertSettings = serde_json::from_value(serialized).unwrap();
        assert!(parsed.mute_taskbar_alerts);
    }

    #[test]
    fn provider_model_extended_config_roundtrips_and_skips_defaults() {
        let minimal = ProviderModel::new("deepseek-flash", true);
        let minimal_json = serde_json::to_value(&minimal).unwrap();
        assert_eq!(
            minimal_json,
            serde_json::json!({ "id": "deepseek-flash", "enabled": true })
        );
        let legacy: ProviderModel =
            serde_json::from_str(r#"{"id":"legacy-m","enabled":true}"#).unwrap();
        assert!(legacy.input_modalities.text);
        assert!(!legacy.input_modalities.supports_vision());

        let rich = ProviderModel {
            id: "GLM-5.3-Flash".into(),
            enabled: true,
            context_window: Some(1_000_000),
            max_output_tokens: Some(128_000),
            input_modalities: ModelInputModalities {
                text: true,
                image: true,
                video: true,
                pdf: true,
            },
            capabilities: ModelCapabilities {
                structured_output: false,
                web_search: true,
                mid_conversation_system: true,
            },
            reasoning_levels: vec!["low".into(), "high".into(), "max".into()],
            reasoning_params: Some(serde_json::json!({"thinking": {"type": "enabled"}})),
        };
        let json = serde_json::to_value(&rich).unwrap();
        assert_eq!(json["contextWindow"], 1_000_000);
        assert_eq!(json["maxOutputTokens"], 128_000);
        assert_eq!(
            json["reasoningLevels"],
            serde_json::json!(["low", "high", "max"])
        );
        let back: ProviderModel = serde_json::from_value(json).unwrap();
        assert_eq!(back, rich);
    }

    #[test]
    fn garbled_detection_enabled_only_serializes_when_true() {
        assert_eq!(
            serde_json::to_value(GarbledDetection { enabled: true }).unwrap(),
            serde_json::json!({"enabled": true})
        );
        assert_eq!(
            serde_json::to_value(GarbledDetection { enabled: false }).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn terminal_scroll_settings_roundtrip_camel_case_and_skip_false_scroll_on_output() {
        let legacy: TerminalSettings = serde_json::from_str("{}").unwrap();
        assert!(!legacy.scroll_on_output);
        assert!(!legacy.jump_to_latest);

        let default_json = serde_json::to_value(&legacy).unwrap();
        assert!(default_json.get("scrollOnOutput").is_none());

        let enabled = TerminalSettings {
            scroll_on_output: true,
            jump_to_latest: true,
            ..TerminalSettings::default()
        };
        let encoded = serde_json::to_value(&enabled).unwrap();
        assert_eq!(encoded["scrollOnOutput"], true);
        assert_eq!(encoded["jumpToLatest"], true);

        let decoded: TerminalSettings = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, enabled);
    }
}
