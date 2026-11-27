//! State interface used by MCP tools to access the live runtime or a read-only snapshot.

use serde::{Deserialize, Serialize};

/// DUT summary returned by discovery.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DutSummary {
    pub dut_id: String,
    pub name: String,
    /// Session source: demo, device, or idle.
    pub source: String,
    pub port: String,
    pub baud: u32,
}

/// Detailed DUT information.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DutInfo {
    pub dut_id: String,
    pub name: String,
    pub source: String,
    pub serial: SerialBindInfo,
    pub adb_serial: Option<String>,
    pub relay: Option<RelayBrief>,
    pub mcp_label: Option<String>,
    /// Number of lines received in the current session, if connected.
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
    #[must_use]
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

/// One line returned by the serial read tool.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogLineDto {
    pub seq: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogReadResult {
    pub dut_id: String,
    pub session_id: Option<String>,
    /// Cursor for the next read.
    pub last_seq: u64,
    pub lines: Vec<LogLineDto>,
}

/// ADB device information.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AdbDeviceDto {
    pub serial: String,
    pub state: String,
    pub model: Option<String>,
}

/// Result of an executed device command or flash utility probe.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessResultDto {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u64,
}

/// Approval failure with a machine-readable tool error code.
#[derive(Debug)]
pub struct ApprovalErr {
    pub kind: duthub_core::ApprovalError,
}

impl ApprovalErr {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self.kind {
            duthub_core::ApprovalError::Missing => "Missing",
            duthub_core::ApprovalError::Unknown => "Unknown",
            duthub_core::ApprovalError::Mismatch => "Mismatch",
        }
    }

    #[must_use]
    pub fn message(&self) -> &'static str {
        self.kind.message()
    }
}

/// Result of issuing an approval token.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApprovalIssueResult {
    pub token: String,
    pub dut_id: String,
    pub action: String,
    pub expires_in_secs: u64,
}

// Heartbeat data and operations

/// Heartbeat configuration with the shape stored in the DUT file.
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
    /// A shell command or newline-separated command group. Omit to keep the current command.
    pub command: Option<String>,
    pub interval_secs: Option<u32>,
    /// False disables probes until explicitly enabled again.
    pub enabled: Option<bool>,
    /// Pause duration in seconds. Zero resumes probing.
    pub pause_secs: Option<u32>,
    /// Expected literal response text. An empty string clears the requirement.
    pub expected_response: Option<String>,
}

/// System information extracted from the response. Unknown values are absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SysInfoDto {
    pub uptime: Option<String>,
    pub mem: Option<String>,
    pub cpu: Option<String>,
    pub hostname: Option<String>,
}

/// Latest heartbeat state. An absent result means no probe has completed.
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

/// Heartbeat configuration and latest state returned by the tools.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HeartbeatViewDto {
    pub dut_id: String,
    pub config: HeartbeatConfigDto,
    pub state: HeartbeatStateDto,
}

impl HeartbeatViewDto {
    /// Convert the current core configuration and status to the tool response.
    #[must_use]
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

// Control leases and status

/// Credentials returned by grant and renewal operations.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaseGrantDto {
    pub lease_id: String,
    pub dut_id: String,
    pub generation: u64,
    pub expires_at_unix_ms: u64,
    pub holder: String,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub acquired_at_unix_ms: u64,
}

/// Control status shared by the MCP tool and desktop panel.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControlStatusDto {
    pub dut_id: String,
    /// free | held | protected | `needs_inspection`
    pub state: String,
    pub holder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquired_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupied_secs: Option<u64>,
    pub remaining_secs: Option<u64>,
    pub generation: u64,
    pub queue_len: usize,
    pub protected_op: Option<String>,
}

/// The host bridges tool calls to device services or a persisted snapshot.
pub trait McpState: Send + Sync + 'static {
    fn media_streams(&self, _dut_id: &str) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::new()
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

    // Live control leases are available through desktop HTTP only.

    /// Acquire an exclusive lease, optionally waiting in FIFO order.
    /// The server verifies the required grant before calling this method.
    fn dut_acquire_control(
        &self,
        _dut_id: &str,
        _holder: duthub_core::LeaseHolder,
        _wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        Err("控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// Acquire an exclusive lease while registering the caller's session name.
    fn dut_acquire_control_with_session(
        &self,
        dut_id: &str,
        holder: duthub_core::LeaseHolder,
        _session_name: &str,
        wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        self.dut_acquire_control(dut_id, holder, wait_ms)
    }

    /// Reset the lease TTL without advancing its generation.
    fn dut_renew_control(&self, _dut_id: &str, _lease_id: &str) -> Result<LeaseGrantDto, String> {
        Err("控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// Release a lease owned by the caller. Protected operations reject release.
    fn dut_release_control(&self, _dut_id: &str, _lease_id: &str) -> Result<(), String> {
        Err("控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// Return a read-only control status snapshot.
    fn dut_control_status(&self, _dut_id: &str) -> Result<ControlStatusDto, String> {
        Err("控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// Read up to the limit of log entries after the supplied sequence number.
    fn serial_read(&self, dut_id: &str, since_seq: u64, limit: usize) -> Option<LogReadResult>;
    /// Return the latest rendered VT100 screen text for this DUT, if available.
    fn serial_screen(&self, _dut_id: &str) -> Option<String> {
        None
    }
    fn log_archive_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    fn dut_artifact_root(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// Write one line to the session.
    /// The device rechecks the server-supplied identity at admission and execution.
    fn serial_write(
        &self,
        dut_id: &str,
        text: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String>;
    /// Write raw control/keyboard bytes without line-ending normalization.
    fn serial_write_raw(
        &self,
        dut_id: &str,
        bytes: &[u8],
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        let text = std::str::from_utf8(bytes).map_err(|_| {
            "raw non-UTF-8 serial write is unavailable on this state source".to_string()
        })?;
        self.serial_write(dut_id, text, writer)
    }

    fn adb_list(&self) -> Vec<AdbDeviceDto>;
    fn discover_adb_devices(&self) -> Result<Vec<AdbDeviceDto>, String> {
        Ok(self.adb_list())
    }
    fn supports_adb_shell(&self) -> bool {
        false
    }
    /// Run an ADB shell command as an argument vector with approval and lease checks.
    fn adb_shell(
        &self,
        dut_id: &str,
        argv: &[String],
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<ProcessResultDto, String>;

    /// Pass the request identity and approval to the shared control service.
    /// It verifies approval, the DUT binding, and the backend's capabilities.
    /// The driver receives only the physical target and action.
    /// Device authorization is handled before dispatch.
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
    /// Pulse and release the relay channel connected to the DUT power key.
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

    /// Atomic `MaskROM` entry (`maskrom ON → reset/power_cycle → hold → maskrom OFF`).
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

    /// Query relay channel states without an approval token.
    /// Return channels that responded or an error if the backend cannot perform the query.
    fn relay_status(&self, _dut_id: &str) -> Result<Vec<(u8, bool)>, String> {
        Err("relay.status 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// Energize, verify, and release one relay channel.
    /// The control service validates the lease and consumes the test approval token.
    fn relay_test(
        &self,
        _dut_id: &str,
        _channel: u8,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<duthub_core::backend::ChannelTestReport, String> {
        Err("relay.test 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// Return heartbeat configuration, latest status, and extracted system information.
    fn heartbeat_get(&self, _dut_id: &str) -> Result<HeartbeatViewDto, String> {
        Err("heartbeat.get 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// Update heartbeat command, interval, or enablement.
    /// Return the resulting configuration and status.
    fn heartbeat_set(
        &self,
        _dut_id: &str,
        _update: HeartbeatUpdate,
    ) -> Result<HeartbeatViewDto, String> {
        Err("heartbeat.set 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// Add a segment marker to the DUT session log.
    fn log_mark(&self, _dut_id: &str, _label: &str) -> Result<(), String> {
        Err("log_mark 经此状态源不可用（仅桌面运行时支持）".into())
    }

    /// Export the skill under the work directory and return its path.
    /// The host resolves that directory using the shared configuration rules.
    fn skill_export(&self) -> Result<String, String> {
        Err("skill.export 经此状态源不可用".into())
    }
    fn skill_export_for_listener(&self, _port: Option<u16>) -> Result<String, String> {
        self.skill_export()
    }

    /// Run the allowlisted read-only probe for the specified flash preset ID.
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
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors MCP adb_exec tool schema parameters directly"
    )]
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

    /// Issue an approval token valid for one use within five minutes.
    fn approval_issue(&self, dut_id: &str, action: &str, args_json: &str) -> ApprovalIssueResult;
    /// Validate and consume an approval token.
    fn approval_validate(
        &self,
        token: &str,
        dut_id: &str,
        action: &str,
        args_json: &str,
    ) -> Result<(), ApprovalErr>;
}
