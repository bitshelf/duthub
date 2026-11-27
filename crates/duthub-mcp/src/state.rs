//! MCP 状态源抽象：ServerHandler 只依赖本 trait，不直接触碰设备服务。

use serde::{Deserialize, Serialize};

/// DUT 摘要（dut.list）
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DutSummary {
    pub dut_id: String,
    pub name: String,
    /// demo | device | idle（未打开会话）
    pub source: String,
    pub port: String,
    pub baud: u32,
}

/// DUT 详情（dut.info）
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DutInfo {
    pub dut_id: String,
    pub name: String,
    pub source: String,
    pub serial: SerialBindInfo,
    pub adb_serial: Option<String>,
    pub relay: Option<RelayBrief>,
    pub mcp_label: Option<String>,
    /// 本次会话已接收行数（连接中才有）
    pub session_lines: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SerialBindInfo {
    pub host: String,
    pub port: String,
    pub baud: u32,
    pub format: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RelayChannelsBrief {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maskrom: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_key: Option<u8>,
}

impl RelayChannelsBrief {
    /// Human/machine-readable list of bound relay capabilities for this DUT.
    pub fn capabilities(&self) -> Vec<String> {
        let mut caps = Vec::new();
        if self.power.is_some() {
            caps.push("power".into());
        }
        if self.reset.is_some() {
            caps.push("reset".into());
        }
        if self.recovery.is_some() {
            caps.push("recovery".into());
        }
        if self.maskrom.is_some() {
            caps.push("maskrom".into());
        }
        if self.power_key.is_some() {
            caps.push("power_key".into());
        }
        caps
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RelayBrief {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_outlet: Option<u16>,
    #[serde(default)]
    pub channels: RelayChannelsBrief,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// 终端行（serial.read）
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogLineDto {
    pub seq: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogReadResult {
    pub dut_id: String,
    pub session_id: Option<String>,
    /// 客户端下次续读的游标
    pub last_seq: u64,
    pub lines: Vec<LogLineDto>,
}

/// ADB 设备（adb.list_devices）
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AdbDeviceDto {
    pub serial: String,
    pub state: String,
    pub model: Option<String>,
}

/// 进程执行结果（adb.shell / flash.test）
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessResultDto {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u64,
}

/// 审批校验错误（code 供 MCP 错误信息展示）
pub struct ApprovalErr {
    pub kind: duthub_core::ApprovalError,
}

impl ApprovalErr {
    pub fn code(&self) -> &'static str {
        match self.kind {
            duthub_core::ApprovalError::Missing => "Missing",
            duthub_core::ApprovalError::Unknown => "Unknown",
            duthub_core::ApprovalError::Mismatch => "Mismatch",
        }
    }

    pub fn message(&self) -> &'static str {
        self.kind.message()
    }
}

/// 审批令牌签发结果（approval.request）
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApprovalIssueResult {
    pub token: String,
    pub dut_id: String,
    pub action: String,
    pub expires_in_secs: u64,
}

// ── 心跳检测（heartbeat.get / heartbeat.set，需求⑦） ─────────────────────────

/// 心跳配置 DTO（与 duts.json `heartbeat` 段同构）。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HeartbeatConfigDto {
    pub enabled: bool,
    pub interval_secs: u32,
    pub command: String,
    pub expected_response: Option<String>,
    pub paused_until_unix_ms: Option<u64>,
    pub loss_threshold: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatUpdate {
    /// 一条命令或按换行分隔的 POSIX Shell 命令组。省略时保留现有命令。
    pub command: Option<String>,
    pub interval_secs: Option<u32>,
    /// false 持续停止，直到明确重新启用。
    pub enabled: Option<bool>,
    /// 暂停秒数，0 解除暂停。
    pub pause_secs: Option<u32>,
    /// 期望响应中的字面文本，空字符串清除匹配条件。
    pub expected_response: Option<String>,
}

/// 系统信息 DTO（启发式提取；提不出的字段为 null）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SysInfoDto {
    pub uptime: Option<String>,
    pub mem: Option<String>,
    pub cpu: Option<String>,
    pub hostname: Option<String>,
}

/// 心跳状态 DTO（last_ok=null 表示尚未探测）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HeartbeatStateDto {
    pub last_ok: Option<bool>,
    pub last_at_unix_ms: u64,
    pub loss_count: u32,
    pub last_response: String,
    /// The last attempted probe was blocked by device control.
    pub paused_by_lease: bool,
    pub sysinfo: SysInfoDto,
}

/// heartbeat.get / heartbeat.set 返回视图。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HeartbeatViewDto {
    pub dut_id: String,
    pub config: HeartbeatConfigDto,
    pub state: HeartbeatStateDto,
}

impl HeartbeatViewDto {
    /// 从 core 心跳类型映射（配置 + 最近状态）。
    pub fn new(
        dut_id: &str,
        config: duthub_core::heartbeat::HeartbeatConfig,
        state: duthub_core::heartbeat::HeartbeatState,
    ) -> Self {
        Self {
            dut_id: dut_id.to_string(),
            config: HeartbeatConfigDto {
                enabled: config.enabled,
                interval_secs: config.interval_secs,
                command: config.command,
                expected_response: config.expected_response,
                paused_until_unix_ms: config
                    .paused_until_unix_ms
                    .filter(|until| *until > duthub_core::audit::unix_ms()),
                loss_threshold: config.loss_threshold,
            },
            state: HeartbeatStateDto {
                last_ok: state.last_ok,
                last_at_unix_ms: state.last_at_unix_ms,
                loss_count: state.loss_count,
                last_response: state.last_response,
                paused_by_lease: state.paused_by_lease,
                sysinfo: SysInfoDto {
                    uptime: state.sysinfo.uptime,
                    mem: state.sysinfo.mem,
                    cpu: state.sysinfo.cpu,
                    hostname: state.sysinfo.hostname,
                },
            },
        }
    }
}

// ── 控制租约（dut_acquire/renew/release_control / dut_control_status，方案 §6.2）──

/// 成功授予的租约凭证（dut_acquire_control / dut_renew_control 返回）。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LeaseGrantDto {
    pub lease_id: String,
    pub dut_id: String,
    pub generation: u64,
    pub expires_at_unix_ms: u64,
    pub holder: String,
}

/// DUT 控制权状态快照（dut_control_status 返回；GUI 面板同源）。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ControlStatusDto {
    pub dut_id: String,
    /// free | held | protected | needs_inspection
    pub state: String,
    pub holder: Option<String>,
    pub remaining_secs: Option<u64>,
    pub generation: u64,
    pub queue_len: usize,
    pub protected_op: Option<String>,
}

/// MCP 状态源。实现方负责把工具调用桥接到设备服务 / 快照文件。
pub trait McpState: Send + Sync + 'static {
    fn media_streams(&self, _dut_id: &str) -> std::collections::BTreeMap<String, String> {
        Default::default()
    }

    fn has_media_binding(&self, _dut_id: &str) -> Option<bool> {
        None
    }

    /// Submit one capture operation (`media_streams` with an `op`) to the
    /// desktop bridge and return the receiver the tool awaits.
    ///
    /// The implementing side creates the request/receiver pair
    /// (`MediaOpRequest::channel`), enqueues the request and hands the receiver
    /// back, so the oneshot `Sender` travels only queue → GUI and can never be
    /// cloned or answered twice. `None` means "this state source has no capture
    /// capability" (stdio snapshot, evidence server, desktop not running) and
    /// the tool answers `MEDIA_CAPTURE_UNAVAILABLE` without waiting. The default
    /// implementation does exactly that.
    fn media_op(
        &self,
        _dut_id: String,
        _source: Option<String>,
        _op: duthub_core::media_op::MediaOp,
    ) -> Option<duthub_core::media_op::OpReceiver> {
        None
    }

    fn list_duts(&self) -> Vec<DutSummary>;
    fn dut_info(&self, dut_id: &str) -> Option<DutInfo>;

    // ── 控制租约四件套（方案 §6.2；快照模式默认拒绝——仅桌面 HTTP 运行时支持）──

    /// 获取独占控制租约（INV-01：每 DUT 至多 1 个）。`wait_ms > 0` 时进入 FIFO
    /// 等待队列（超时 → Busy）。授权（写类 Grant）由 ServerHandler 校验。
    fn dut_acquire_control(
        &self,
        _dut_id: &str,
        _holder: duthub_core::LeaseHolder,
        _wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        Err("控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// 续租（TTL 重置；generation 不变）。过期/撤销 → LeaseRequired/LEASE_EXPIRED。
    fn dut_renew_control(&self, _dut_id: &str, _lease_id: &str) -> Result<LeaseGrantDto, String> {
        Err("控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// 释放租约（持有者本人；Protected 期拒 → DUT_PROTECTED）。
    fn dut_release_control(&self, _dut_id: &str, _lease_id: &str) -> Result<(), String> {
        Err("控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// 控制权状态快照（只读）。
    fn dut_control_status(&self, _dut_id: &str) -> Result<ControlStatusDto, String> {
        Err("控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// 读取会话日志（seq > since_seq，最多 limit 条）
    fn serial_read(&self, dut_id: &str, since_seq: u64, limit: usize) -> Option<LogReadResult>;
    fn log_archive_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    fn dut_artifact_root(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// 向会话写入一行（租约复核在 ServerHandler 完成，identity 透传给
    /// device 写入通道做 [D]/[E] 双点复核）
    fn serial_write(
        &self,
        dut_id: &str,
        text: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String>;

    fn adb_list(&self) -> Vec<AdbDeviceDto>;
    fn discover_adb_devices(&self) -> Result<Vec<AdbDeviceDto>, String> {
        Ok(self.adb_list())
    }
    fn supports_adb_shell(&self) -> bool {
        false
    }
    /// `adb -s <serial> shell <argv…>`（argv-only，需审批令牌 + 租约）
    fn adb_shell(
        &self,
        dut_id: &str,
        argv: &[String],
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<ProcessResultDto, String>;

    /// 控制动作（Power/Reset/PowerKey）：审批令牌（高危动作二次确认）+ 租约身份
    /// 都**透传**给共用设备操作入口
    /// （duthub-device::control::ControlService）统一校验授权、绑定与动作能力；
    /// 驱动只接收已解析的设备地址与动作，不在 MCP 层或驱动层重复授权。
    fn control_power(
        &self,
        dut_id: &str,
        on: bool,
        approval_token: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String>;
    fn control_reset(
        &self,
        dut_id: &str,
        approval_token: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String>;
    /// 电源按键脉冲（继电器短接 power key 通道后释放）
    fn control_power_key(
        &self,
        dut_id: &str,
        long_press: bool,
        approval_token: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String>;

    /// Atomic power cycle (`power OFF → wait → power ON`).
    fn control_power_cycle(
        &self,
        _dut_id: &str,
        _off_ms: u64,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("control.power_cycle is unavailable on this state source".into())
    }

    /// Atomic MaskROM entry (`maskrom ON → reset/power_cycle → hold → maskrom OFF`).
    fn control_enter_maskrom(
        &self,
        _dut_id: &str,
        _hold_ms: u64,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("control.enter_maskrom is unavailable on this state source".into())
    }

    /// Atomic Loader/Recovery entry (`recovery ON → reset/power_cycle → hold → recovery OFF`).
    fn control_enter_loader(
        &self,
        _dut_id: &str,
        _hold_ms: u64,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("control.enter_loader is unavailable on this state source".into())
    }

    /// 继电器逐通道状态查询（只读，无需审批）。返回应答 STATUS 的
    /// `(通道号, 是否吸合)`；Err = DUT 未配置继电器 / 后端不支持。
    fn relay_status(&self, _dut_id: &str) -> Result<Vec<(u8, bool)>, String> {
        Err("relay.status 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// 继电器单通道 ON→STATUS→OFF 往返测试（写动作，走 control.test 审批闸门，
    /// 令牌与租约身份透传给共用设备操作入口统一校验并消费）。
    fn relay_test(
        &self,
        _dut_id: &str,
        _channel: u8,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<duthub_core::backend::ChannelTestReport, String> {
        Err("relay.test 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// 心跳查询（需求⑦）：当前配置 + 最近一次状态（含系统信息提取）。
    fn heartbeat_get(&self, _dut_id: &str) -> Result<HeartbeatViewDto, String> {
        Err("heartbeat.get 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// 心跳设置（需求⑦）：Agent 改写心跳命令/间隔并启用/停用。
    /// 返回设置后的完整视图。
    fn heartbeat_set(
        &self,
        _dut_id: &str,
        _update: HeartbeatUpdate,
    ) -> Result<HeartbeatViewDto, String> {
        Err("heartbeat.set 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// 自定义分段标记（日志分割）：向 DUT 会话日志写 `[mark] {label}` 行。
    fn log_mark(&self, _dut_id: &str, _label: &str) -> Result<(), String> {
        Err("log_mark 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// 导出 SKILL.md 到 `<工作目录>/skill/duthub/`（需求⑥）：返回写入路径。
    /// 工作目录由实现方经 `duthub_core::store::resolve_work_dir_in` 同源解析。
    fn skill_export(&self) -> Result<String, String> {
        Err("skill.export 经此状态源不可用".into())
    }
    fn skill_export_for_listener(&self, _port: Option<u16>) -> Result<String, String> {
        self.skill_export()
    }

    /// 烧录工具只读测试（白名单参数；preset_id = settings.flashTools[].id 稳定预设 id）
    fn flash_test(&self, preset_id: &str) -> Result<ProcessResultDto, String>;

    /// List connected Rockusb/MaskROM/Loader devices using a host-configured flash preset.
    fn flash_list(&self, _preset_id: Option<&str>) -> Result<ProcessResultDto, String> {
        Err("flash_list is unavailable on this state source".into())
    }

    /// Flash a firmware image strictly inside the DUT's owned directory using a
    /// host-configured flash preset (`upgrade_tool uf <verified_dut_image>`).
    fn flash_update(
        &self,
        _dut_id: &str,
        _file_id: &str,
        _preset_id: Option<&str>,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("flash_update is unavailable on this state source".into())
    }

    /// Reboot a connected Rockusb/Loader device back to normal boot (`upgrade_tool rd`).
    fn flash_reboot(
        &self,
        _dut_id: &str,
        _preset_id: Option<&str>,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("flash_reboot is unavailable on this state source".into())
    }

    /// Bound-DUT ADB debugging action (`shell` | `reboot` | `reboot_loader` | `logcat` | `push` | `pull`).
    /// File transfer paths are strictly confined to the DUT's owned directory.
    #[allow(clippy::too_many_arguments)]
    fn adb_exec(
        &self,
        _dut_id: &str,
        _op: &str,
        _command: Option<&str>,
        _file_id: Option<&str>,
        _remote_path: Option<&str>,
        _timeout_ms: u64,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("adb_exec is unavailable on this state source".into())
    }

    /// 签发审批令牌（单次有效，5 分钟过期）
    fn approval_issue(&self, dut_id: &str, action: &str, args_json: &str) -> ApprovalIssueResult;
    /// 校验并消费审批令牌
    fn approval_validate(
        &self,
        token: &str,
        dut_id: &str,
        action: &str,
        args_json: &str,
    ) -> Result<(), ApprovalErr>;
}
