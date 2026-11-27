//! MCP 运行时桥接：desktop 状态 → `duthub_mcp::McpState`。
//!
//! - HTTP 端点（默认 127.0.0.1:8797/mcp；开启局域网后为 0.0.0.0:8797）后台线程托管。
//! - Workbench 每帧把 DUT 摘要 + 最近日志推入共享快照；快照写入循环约 2s 落盘
//!   工作目录 `runtime/mcp-state.json`（stdio 快照子进程经同源解析读取）。
//! - 审批令牌在 `RuntimeMcpState` 内单次消费。

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use duthub_core::ApprovalManager;
use duthub_core::lease::{ControlLeaseManager, LeaseError, LeasePhase, WriteIdentity};
use duthub_device::DeviceService;
use duthub_device::control::{ControlAction, ControlEntryError, ControlService};
use duthub_device::heartbeat::HeartbeatService;
use duthub_mcp::auth_http::{AccessContext, lease_error_code};
use duthub_mcp::notify::{
    MESSAGE_MAX_CHARS, NotifyLevel, NotifyOutcome, NotifyRequest, TITLE_MAX_CHARS, ThrottleReason,
    UserNotifier,
};
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, ControlStatusDto, DutInfo, DutSummary,
    HeartbeatUpdate, HeartbeatViewDto, LeaseGrantDto, LogLineDto, LogReadResult, McpState,
    ProcessResultDto,
};
use duthub_mcp::transport::ListenerHandle;

use crate::alerts::{AlertCenter, AlertLevel, AlertSource, ReportRejection};

/// `notify_user` burst/rate per authenticated credential.
const CREDENTIAL_BURST: u32 = 2;
const CREDENTIAL_PER_MINUTE: u32 = 3;
/// `notify_user` burst/rate across every credential.
const GLOBAL_BURST: u32 = 4;
const GLOBAL_PER_MINUTE: u32 = 10;
/// Upper bound of per-credential buckets kept in memory.
const MAX_CREDENTIAL_BUCKETS: usize = 128;

/// Discrete token bucket; `refill_per_ms` keeps fractional tokens.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_ms: f64,
    last_ms: Option<u64>,
}

impl TokenBucket {
    fn new(capacity: u32, per_minute: u32) -> Self {
        Self {
            tokens: f64::from(capacity),
            capacity: f64::from(capacity),
            refill_per_ms: f64::from(per_minute) / 60_000.0,
            last_ms: None,
        }
    }

    /// Refill up to `now_ms` and report whether at least one token is free.
    fn has_token(&mut self, now_ms: u64) -> bool {
        let last = self.last_ms.unwrap_or(now_ms);
        #[expect(
            clippy::cast_precision_loss,
            reason = "elapsed ms between rate-limit refills fits well within f64 mantissa"
        )]
        let elapsed = now_ms.saturating_sub(last) as f64;
        self.tokens = (self.tokens + elapsed * self.refill_per_ms).min(self.capacity);
        self.last_ms = Some(now_ms);
        self.tokens >= 1.0
    }

    fn take(&mut self) {
        self.tokens = (self.tokens - 1.0).max(0.0);
    }

    fn is_full(&self) -> bool {
        self.tokens >= self.capacity
    }
}

/// Host-side rate limiter for `notify_user`. A credential can only spend its
/// own bucket, so one chatty Agent cannot starve the others; the global bucket
/// caps the total rate a user's UI can be woken at.
#[derive(Debug)]
struct NotifyRateLimiter {
    per_credential: Mutex<HashMap<String, TokenBucket>>,
    global: Mutex<TokenBucket>,
}

impl NotifyRateLimiter {
    fn new() -> Self {
        Self {
            per_credential: Mutex::new(HashMap::new()),
            global: Mutex::new(TokenBucket::new(GLOBAL_BURST, GLOBAL_PER_MINUTE)),
        }
    }

    /// Consume one token from every applicable bucket, or nothing at all.
    fn try_acquire(&self, credential_id: Option<&str>, now_ms: u64) -> bool {
        let mut global = self
            .global
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let global_ok = global.has_token(now_ms);
        let mut buckets = self
            .per_credential
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.len() > MAX_CREDENTIAL_BUCKETS {
            buckets.retain(|_, bucket| !bucket.is_full());
        }
        let credential_ok = match credential_id {
            Some(id) => buckets
                .entry(id.to_owned())
                .or_insert_with(|| TokenBucket::new(CREDENTIAL_BURST, CREDENTIAL_PER_MINUTE))
                .has_token(now_ms),
            None => true,
        };
        if !(global_ok && credential_ok) {
            return false;
        }
        global.take();
        if let Some(id) = credential_id
            && let Some(bucket) = buckets.get_mut(id)
        {
            bucket.take();
        }
        true
    }
}

/// Live tool configuration synced from `settings.json` into `RuntimeMcpState`.
#[derive(Debug, Clone, Default)]
pub struct RuntimeToolsConfig {
    pub flash_tools: Vec<duthub_core::model::FlashToolPreset>,
    pub adb_executable: String,
}

pub struct RuntimeMcpState {
    device: Arc<DeviceService>,
    snapshot: Arc<std::sync::Mutex<duthub_mcp::snapshot::StateSnapshot>>,
    approval: Arc<ApprovalManager>,
    /// Shared device control entry point: authorization, bindings, capabilities, and driver dispatch.
    control: Arc<ControlService>,
    /// Heartbeat service (allows updating command/interval via MCP).
    heartbeat: Arc<HeartbeatService>,
    media_urls: crate::media::MediaUrls,
    /// GUI end of the media-op bridge (`sync_channel(8)` + oneshot replies):
    /// `media_streams` with an `op` enqueues here and the Workbench tick
    /// answers (§5.5). This clone shares one queue with `MediaView`.
    media_ops: crate::media_page::MediaOpSubmitter,
    /// Alert center shared between MCP HTTP threads and the Workbench UI.
    alerts: Arc<std::sync::Mutex<AlertCenter>>,
    /// Work directory (`resolve_work_dir_in`): anchor for logs and skill export.
    work_dir: PathBuf,
    /// Agent access context (auth / leases / audit) shared with `DeviceService`.
    access: Option<Arc<AccessContext>>,
    /// Live flash presets and ADB executable path synced from `settings.json`.
    tools_config: Arc<Mutex<RuntimeToolsConfig>>,
    /// Host-side `notify_user` rate limiting (per credential + global).
    notify_limiter: NotifyRateLimiter,
}

impl RuntimeMcpState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: Arc<DeviceService>,
        snapshot: Arc<std::sync::Mutex<duthub_mcp::snapshot::StateSnapshot>>,
        control: Arc<ControlService>,
        approval: Arc<ApprovalManager>,
        heartbeat: Arc<HeartbeatService>,
        media_urls: crate::media::MediaUrls,
        media_ops: crate::media_page::MediaOpSubmitter,
        alerts: Arc<std::sync::Mutex<AlertCenter>>,
        work_dir: PathBuf,
        access: Option<Arc<AccessContext>>,
        tools_config: Arc<Mutex<RuntimeToolsConfig>>,
    ) -> Self {
        Self {
            device,
            snapshot,
            approval,
            control,
            heartbeat,
            media_urls,
            media_ops,
            alerts,
            work_dir,
            access,
            tools_config,
            notify_limiter: NotifyRateLimiter::new(),
        }
    }

    fn resolve_flash_executable(&self, preset_id: Option<&str>) -> Result<String, String> {
        let cfg = self
            .tools_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let preset = if let Some(wanted) = preset_id.map(str::trim).filter(|s| !s.is_empty()) {
            cfg.flash_tools
                .iter()
                .find(|p| p.id == wanted || p.name.eq_ignore_ascii_case(wanted))
                .ok_or_else(|| format!("flash preset not found: {wanted}"))?
        } else {
            cfg.flash_tools
                .iter()
                .find(|p| {
                    p.executable.to_ascii_lowercase().contains("upgrade_tool")
                        || p.name.to_ascii_lowercase().contains("upgrade_tool")
                })
                .or_else(|| cfg.flash_tools.first())
                .ok_or_else(|| "no flash tool preset is configured in settings.json".to_string())?
        };
        let exe = preset.executable.trim();
        if exe.is_empty() {
            return Err(format!(
                "flash preset {} has no executable configured",
                preset.id
            ));
        }
        Ok(exe.to_owned())
    }

    fn resolve_adb_executable(&self) -> String {
        let cfg = self
            .tools_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        duthub_desktop::tool_resolver::effective_spec(&cfg.adb_executable, "adb")
    }

    fn begin_agent_action(
        lease: Option<&Arc<ControlLeaseManager>>,
    ) -> Result<Option<impl Drop + '_>, String> {
        lease
            .map(|lease| lease.begin_agent_action().map_err(|err| lease_err(&err)))
            .transpose()
    }

    /// 控制动作统一走共用设备操作入口；授权错误带类别码便于 MCP 客户端感知。
    /// Device 错误（动作真正执行失败）同时转弹窗提醒（需求⑤接入点③）；
    /// `ApprovalRequired` 是审批协议的正常环节（客户端据此走 approval.request），
    /// 不算异常、不弹窗。
    fn control_execute(
        &self,
        dut_id: &str,
        action: ControlAction,
        approval_token: &str,
        writer: &WriteIdentity,
    ) -> Result<(), String> {
        let action_label = action.mark_label().to_string();
        self.control
            .execute_by_id_as(dut_id, action, approval_token, writer)
            .map_err(|err| match err {
                ControlEntryError::Approval(approval) => {
                    format!(
                        "ApprovalRequired/{}: {}",
                        code_of(approval),
                        approval.message()
                    )
                }
                ControlEntryError::Device(err) => {
                    if !matches!(err, duthub_core::DeviceError::Lease(_)) {
                        self.report_control_failure(dut_id, &action_label, &err.to_string());
                    }
                    err.to_string()
                }
            })
    }

    /// `继电器写动作（relay_test）失败同样转弹窗提醒；只读查询（relay_status）不算动作`。
    fn relay_test_execute(
        &self,
        dut_id: &str,
        channel: u8,
        approval_token: &str,
        writer: &WriteIdentity,
    ) -> Result<duthub_core::backend::ChannelTestReport, String> {
        self.control
            .channel_test_as(dut_id, channel, approval_token, writer)
            .map_err(|err| match &err {
                ControlEntryError::Device(device_err) => {
                    if !matches!(device_err, duthub_core::DeviceError::Lease(_)) {
                        self.report_control_failure(
                            dut_id,
                            &format!("继电器往返测试 CH{channel}"),
                            &device_err.to_string(),
                        );
                    }
                    err.to_string()
                }
                ControlEntryError::Approval(_) => err.to_string(),
            })
    }

    /// 控制租约四件套的共享实现：`access` 未接线（异常构造）时明确报错。
    fn lease_manager(&self) -> Result<&Arc<ControlLeaseManager>, String> {
        self.access
            .as_ref()
            .map(|a| &a.lease)
            .ok_or_else(|| "控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）".into())
    }

    /// 弹窗提醒入队（去重/抑制/总开关都在 `AlertCenter` 内裁决）。
    fn report_control_failure(&self, dut_id: &str, action: &str, err: &str) {
        let (title, detail) = crate::alerts::control_action_alert_texts(dut_id, action, err);
        let _ = self
            .alerts
            .lock()
            .map(|mut center| center.report_for(dut_id, AlertSource::Control, title, detail));
    }
}

/// Redact host-side directory prefixes from subprocess output so remote Agents
/// never see Windows host paths or usernames while keeping plain filenames clear.
fn redact_host_dir(text: &str, host_dir: &std::path::Path) -> String {
    let raw = host_dir.to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let mut out = text.to_owned();
    for prefix in [
        format!(r"\\?\{stripped}\"),
        format!(r"\\?\{stripped}/"),
        format!(r"{stripped}\"),
        format!("{stripped}/"),
        format!(r"\\?\{stripped}"),
        stripped.to_owned(),
    ] {
        if !prefix.is_empty() {
            let replacement = if prefix.ends_with('\\') || prefix.ends_with('/') {
                "<dut-dir>/"
            } else {
                "<dut-dir>"
            };
            out = out.replace(&prefix, replacement);
        }
    }
    out
}

/// 租约错误 → `LeaseRequired/{code}: {message}`（与 server.rs 前缀约定一致）。
fn lease_err(err: &LeaseError) -> String {
    format!("LeaseRequired/{}: {}", lease_error_code(err), err.message())
}

fn code_of(err: duthub_core::ApprovalError) -> &'static str {
    match err {
        duthub_core::ApprovalError::Missing => "Missing",
        duthub_core::ApprovalError::Unknown => "Unknown",
        duthub_core::ApprovalError::Mismatch => "Mismatch",
    }
}

impl McpState for RuntimeMcpState {
    fn has_media_binding(&self, dut_id: &str) -> Option<bool> {
        let snapshot = self.snapshot.lock().unwrap();
        snapshot
            .duts
            .iter()
            .any(|dut| dut.dut_id == dut_id)
            .then(|| snapshot.media_bindings.iter().any(|id| id == dut_id))
    }

    fn log_archive_dir(&self) -> Option<PathBuf> {
        Some(self.work_dir.clone())
    }

    fn dut_artifact_root(&self) -> Option<PathBuf> {
        self.work_dir.parent().map(std::path::Path::to_path_buf)
    }
    fn media_streams(&self, dut_id: &str) -> std::collections::BTreeMap<String, String> {
        self.media_urls
            .lock()
            .unwrap()
            .get(dut_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Submit one capture op to the GUI queue (§5.5). The request carries the
    /// only oneshot sender; a full or closed queue drops it so the tool maps
    /// the receiver error to `MEDIA_OP_DROPPED`.
    fn media_op(
        &self,
        dut_id: String,
        source: Option<String>,
        op: duthub_core::media_op::MediaOp,
    ) -> Option<duthub_core::media_op::OpReceiver> {
        Some(self.media_ops.submit(dut_id, source, op))
    }

    fn list_duts(&self) -> Vec<DutSummary> {
        let guard = self.snapshot.lock().unwrap();
        guard
            .duts
            .iter()
            .map(|d| DutSummary {
                dut_id: d.dut_id.clone(),
                name: d.name.clone(),
                source: d.source.clone(),
                port: d.serial.port.clone(),
                baud: d.serial.baud,
            })
            .collect()
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        let guard = self.snapshot.lock().unwrap();
        let d = guard.duts.iter().find(|d| d.dut_id == dut_id)?;
        Some(DutInfo {
            dut_id: d.dut_id.clone(),
            name: d.name.clone(),
            source: d.source.clone(),
            serial: d.serial.clone(),
            adb_serial: d.adb_serial.clone(),
            relay: d.relay.clone(),
            mcp_label: d.mcp_label.clone(),
            session_lines: d.lines.last().map(|l| l.seq),
        })
    }

    fn serial_read(&self, dut_id: &str, since_seq: u64, limit: usize) -> Option<LogReadResult> {
        self.device.session_source(dut_id)?;
        let session_id = format!("sess-{dut_id}");
        let (last_seq, envelopes) = self.device.read_serial_page(&session_id, since_seq, limit);
        let lines: Vec<LogLineDto> = envelopes
            .iter()
            .filter_map(|e| match &e.event {
                duthub_core::UiEvent::SerialChunk { text, .. } => Some(LogLineDto {
                    seq: e.seq,
                    text: text.clone(),
                }),
                _ => None,
            })
            .collect();
        Some(LogReadResult {
            dut_id: dut_id.to_string(),
            session_id: Some(session_id),
            last_seq,
            lines,
        })
    }

    // ── 控制租约四件套（方案 §6.2；与写入通道共享同一 ControlLeaseManager）────

    fn dut_acquire_control(
        &self,
        dut_id: &str,
        holder: duthub_core::LeaseHolder,
        wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        let mgr = self.lease_manager()?;
        let wait = if wait_ms > 0 {
            Some(Duration::from_millis(wait_ms))
        } else {
            None
        };
        let grant = mgr
            .acquire(dut_id, &holder, wait)
            .map_err(|e| lease_err(&e))?;
        Ok(LeaseGrantDto {
            lease_id: grant.lease_id,
            dut_id: grant.dut_id,
            generation: grant.generation,
            expires_at_unix_ms: grant.expires_at_unix_ms,
            holder: grant.holder,
        })
    }

    fn dut_renew_control(&self, dut_id: &str, lease_id: &str) -> Result<LeaseGrantDto, String> {
        let grant = self
            .lease_manager()?
            .renew(dut_id, lease_id)
            .map_err(|e| lease_err(&e))?;
        Ok(LeaseGrantDto {
            lease_id: grant.lease_id,
            dut_id: grant.dut_id,
            generation: grant.generation,
            expires_at_unix_ms: grant.expires_at_unix_ms,
            holder: grant.holder,
        })
    }

    fn dut_release_control(&self, dut_id: &str, lease_id: &str) -> Result<(), String> {
        self.lease_manager()?
            .release(dut_id, lease_id)
            .map_err(|e| lease_err(&e))
    }

    fn dut_control_status(&self, dut_id: &str) -> Result<ControlStatusDto, String> {
        let status = self.lease_manager()?.status(dut_id);
        let state = match status.phase {
            LeasePhase::Free => "free",
            LeasePhase::Held => "held",
            LeasePhase::Protected => "protected",
            LeasePhase::NeedsInspection => "needs_inspection",
        };
        Ok(ControlStatusDto {
            dut_id: status.dut_id,
            state: state.to_string(),
            holder: status.holder,
            remaining_secs: status.remaining_secs,
            generation: status.generation,
            queue_len: status.queue_len,
            protected_op: status.protected_op,
        })
    }

    fn serial_write(&self, dut_id: &str, text: &str, writer: WriteIdentity) -> Result<(), String> {
        // 写身份由 server.rs 租约复核构造（归属当前凭据 + generation）；
        // device 写入通道再做 [D]/[E] 双点复核（GUI 优先无条件生效）
        self.device
            .write_line(dut_id, writer, text)
            .map_err(|e| e.to_string())
    }

    fn adb_list(&self) -> Vec<AdbDeviceDto> {
        self.discover_adb_devices().unwrap_or_default()
    }

    fn discover_adb_devices(&self) -> Result<Vec<AdbDeviceDto>, String> {
        let devices = duthub_adb::list_local_server_devices().or_else(|_| {
            let spec = self.resolve_adb_executable();
            duthub_adb::AdbClient::real().devices_l(&spec)
        });
        devices
            .map(|devices| {
                devices
                    .into_iter()
                    .map(|device| AdbDeviceDto {
                        serial: device.serial,
                        state: device.state,
                        model: device.model,
                    })
                    .collect()
            })
            .map_err(|error| error.to_string())
    }

    fn adb_shell(
        &self,
        _dut_id: &str,
        _argv: &[String],
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("adb_shell is deprecated; use adb_exec instead".into())
    }

    fn adb_exec(
        &self,
        dut_id: &str,
        op: &str,
        command: Option<&str>,
        file_id: Option<&str>,
        remote_path: Option<&str>,
        timeout_ms: u64,
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        use duthub_core::process::{ProcessRunner, RealProcessRunner, resolve_executable};

        let lease = self.device.lease_manager();
        let _agent_action = Self::begin_agent_action(lease.as_ref())?;
        let dut = self
            .dut_info(dut_id)
            .ok_or_else(|| "DUT not found".to_string())?;
        let serial = dut
            .adb_serial
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "DUT has no bound ADB serial configured".to_string())?;
        let adb_spec = self.resolve_adb_executable();
        let devices = duthub_adb::AdbClient::real()
            .devices_l(&adb_spec)
            .map_err(|err| format!("ADB discovery failed: {err}"))?;
        let target = devices
            .iter()
            .find(|d| d.serial == serial)
            .ok_or_else(|| format!("bound ADB device {serial} is not connected"))?;
        if !matches!(target.state.as_str(), "device" | "recovery") {
            return Err(format!(
                "bound ADB device {serial} is not ready (state: {})",
                target.state
            ));
        }

        let mut argv: Vec<String> = vec!["-s".into(), serial.to_owned()];
        match op {
            "shell" => {
                let cmd = command
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "command is required for adb_exec(op=\"shell\")".to_string())?;
                argv.push("shell".into());
                argv.push(cmd.to_owned());
            }
            "reboot" => {
                let _ = self.device.log_mark(dut_id, "adb reboot");
                argv.push("reboot".into());
            }
            "reboot_loader" => {
                let _ = self.device.log_mark(dut_id, "adb reboot loader");
                argv.push("reboot".into());
                argv.push("loader".into());
            }
            "logcat" => {
                argv.extend(["logcat".into(), "-d".into(), "-t".into(), "200".into()]);
            }
            "push" => {
                let root = self
                    .dut_artifact_root()
                    .ok_or_else(|| "DUT artifact directory unavailable".to_string())?;
                let fid = file_id
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "fileId is required for adb push".to_string())?;
                let remote = remote_path
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "remotePath is required for adb push".to_string())?;
                let verified_src = duthub_core::dut_artifacts::resolve_owned_file(
                    &root,
                    &dut.dut_id,
                    &dut.name,
                    fid,
                )
                .map_err(|err| format!("file fence rejected push source: {err}"))?;
                argv.extend([
                    "push".into(),
                    verified_src.to_string_lossy().into_owned(),
                    remote.to_owned(),
                ]);
            }
            "pull" => {
                let root = self
                    .dut_artifact_root()
                    .ok_or_else(|| "DUT artifact directory unavailable".to_string())?;
                let fid = file_id
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "fileId is required for adb pull".to_string())?;
                let remote = remote_path
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "remotePath is required for adb pull".to_string())?;
                let verified_dst = duthub_core::dut_artifacts::prepare_owned_destination(
                    &root,
                    &dut.dut_id,
                    &dut.name,
                    fid,
                )
                .map_err(|err| format!("file fence rejected pull destination: {err}"))?;
                argv.extend([
                    "pull".into(),
                    remote.to_owned(),
                    verified_dst.to_string_lossy().into_owned(),
                ]);
            }
            other => return Err(format!("unsupported adb_exec op: {other}")),
        }

        let exe = resolve_executable(&adb_spec).map_err(|err| err.to_string())?;
        let out = RealProcessRunner
            .run(&exe, &argv, Duration::from_millis(timeout_ms))
            .map_err(|err| err.to_string())?;
        let dut_dir = self.dut_artifact_root().map(|root| root.join(&dut.name));
        let (stdout, stderr) = match dut_dir.as_deref() {
            Some(dir) => (
                redact_host_dir(&out.stdout, dir),
                redact_host_dir(&out.stderr, dir),
            ),
            None => (out.stdout, out.stderr),
        };
        Ok(ProcessResultDto {
            exit_code: out.exit_code,
            stdout,
            stderr,
            elapsed_ms: out.elapsed_ms,
        })
    }

    fn control_power(
        &self,
        dut_id: &str,
        on: bool,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_execute(dut_id, ControlAction::Power { on }, approval_token, &writer)
    }

    fn control_reset(
        &self,
        dut_id: &str,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_execute(dut_id, ControlAction::Reset, approval_token, &writer)
    }

    fn control_power_key(
        &self,
        dut_id: &str,
        long_press: bool,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_execute(
            dut_id,
            ControlAction::PowerKey { long_press },
            approval_token,
            &writer,
        )
    }

    fn control_power_cycle(
        &self,
        dut_id: &str,
        off_ms: u64,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_execute(
            dut_id,
            ControlAction::PowerCycle { off_ms },
            approval_token,
            &writer,
        )
    }

    fn control_enter_maskrom(
        &self,
        dut_id: &str,
        hold_ms: u64,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_execute(
            dut_id,
            ControlAction::EnterMaskrom { hold_ms },
            approval_token,
            &writer,
        )
    }

    fn control_enter_loader(
        &self,
        dut_id: &str,
        hold_ms: u64,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_execute(
            dut_id,
            ControlAction::EnterLoader { hold_ms },
            approval_token,
            &writer,
        )
    }

    fn relay_status(&self, dut_id: &str) -> Result<Vec<(u8, bool)>, String> {
        self.control
            .channel_status(dut_id)
            .map_err(|err| err.to_string())
    }

    fn relay_test(
        &self,
        dut_id: &str,
        channel: u8,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<duthub_core::backend::ChannelTestReport, String> {
        self.relay_test_execute(dut_id, channel, approval_token, &writer)
    }

    fn heartbeat_get(&self, dut_id: &str) -> Result<HeartbeatViewDto, String> {
        Ok(HeartbeatViewDto::new(
            dut_id,
            self.heartbeat.config(dut_id),
            self.heartbeat.state_or_default(dut_id),
        ))
    }

    fn heartbeat_set(
        &self,
        dut_id: &str,
        update: HeartbeatUpdate,
    ) -> Result<HeartbeatViewDto, String> {
        let lease = self.device.lease_manager();
        let _agent_action = lease
            .as_ref()
            .map(|lease| lease.begin_agent_action().map_err(|err| lease_err(&err)))
            .transpose()?;
        let applied = self.heartbeat.update_config(dut_id, |mut cfg| {
            if let Some(command) = update.command {
                command.trim().clone_into(&mut cfg.command);
                cfg.enabled = true;
            }
            if let Some(secs) = update.interval_secs {
                cfg.interval_secs = secs;
            }
            if let Some(enabled) = update.enabled {
                cfg.enabled = enabled;
            }
            if let Some(expected) = update.expected_response {
                let expected = expected.trim();
                cfg.expected_response = (!expected.is_empty()).then(|| expected.to_owned());
            }
            if let Some(pause) = update.pause_secs {
                cfg.paused_until_unix_ms = (pause > 0)
                    .then(|| duthub_device::heartbeat::now_unix_ms() + u64::from(pause) * 1000);
            }
            if update.enabled == Some(false) {
                cfg.paused_until_unix_ms = None;
            }
            cfg
        })?;
        Ok(HeartbeatViewDto::new(
            dut_id,
            applied,
            self.heartbeat.state_or_default(dut_id),
        ))
    }

    fn log_mark(&self, dut_id: &str, label: &str) -> Result<(), String> {
        self.device
            .log_mark(dut_id, label)
            .map_err(|e| e.to_string())
    }

    /// 需求⑥：SKILL.md 落盘到 `<工作目录>/skill/duthub/`（与设置页按钮同一生成器）。
    fn skill_export_for_listener(&self, port: Option<u16>) -> Result<String, String> {
        duthub_mcp::skill::export_skill_for_listener(&self.work_dir, port)
            .map(|path| path.display().to_string())
            .map_err(|e| e.to_string())
    }

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("flash_test is deprecated; use dut_control(action=\"flash_list\")".into())
    }

    fn flash_list(&self, preset_id: Option<&str>) -> Result<ProcessResultDto, String> {
        let exe = self.resolve_flash_executable(preset_id)?;
        let out = duthub_flash::FlashTester::real()
            .list_devices(&exe)
            .map_err(|err| err.to_string())?;
        Ok(ProcessResultDto {
            exit_code: out.exit_code,
            stdout: out.stdout,
            stderr: out.stderr,
            elapsed_ms: out.elapsed_ms,
        })
    }

    fn flash_update(
        &self,
        dut_id: &str,
        file_id: &str,
        preset_id: Option<&str>,
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        let lease = self.device.lease_manager();
        let _agent_action = Self::begin_agent_action(lease.as_ref())?;
        let root = self
            .dut_artifact_root()
            .ok_or_else(|| "DUT artifact directory unavailable".to_string())?;
        let dut = self
            .dut_info(dut_id)
            .ok_or_else(|| "DUT not found".to_string())?;
        let verified_image =
            duthub_core::dut_artifacts::resolve_owned_file(&root, &dut.dut_id, &dut.name, file_id)
                .map_err(|err| format!("file fence rejected firmware image: {err}"))?;
        let exe = self.resolve_flash_executable(preset_id)?;
        let _ = self
            .device
            .log_mark(dut_id, &format!("flash start {file_id}"));
        let dut_dir = root.join(&dut.name);
        match duthub_flash::FlashTester::real().upgrade_firmware(&exe, &verified_image) {
            Ok(out) => {
                let status = if out.exit_code == Some(0) {
                    "done"
                } else {
                    "failed"
                };
                let _ = self
                    .device
                    .log_mark(dut_id, &format!("flash {status} {file_id}"));
                Ok(ProcessResultDto {
                    exit_code: out.exit_code,
                    stdout: redact_host_dir(&out.stdout, &dut_dir),
                    stderr: redact_host_dir(&out.stderr, &dut_dir),
                    elapsed_ms: out.elapsed_ms,
                })
            }
            Err(err) => {
                let msg = err.to_string();
                let _ = self
                    .device
                    .log_mark(dut_id, &format!("flash failed {file_id}"));
                self.report_control_failure(dut_id, "flash_update", &msg);
                Err(msg)
            }
        }
    }

    fn flash_reboot(
        &self,
        dut_id: &str,
        preset_id: Option<&str>,
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        let lease = self.device.lease_manager();
        let _agent_action = Self::begin_agent_action(lease.as_ref())?;
        let exe = self.resolve_flash_executable(preset_id)?;
        let _ = self.device.log_mark(dut_id, "flash reboot");
        let out = duthub_flash::FlashTester::real()
            .reboot_device(&exe)
            .map_err(|err| err.to_string())?;
        Ok(ProcessResultDto {
            exit_code: out.exit_code,
            stdout: out.stdout,
            stderr: out.stderr,
            elapsed_ms: out.elapsed_ms,
        })
    }

    fn approval_issue(&self, dut_id: &str, action: &str, args_json: &str) -> ApprovalIssueResult {
        let t = self.approval.issue(dut_id, action, args_json);
        ApprovalIssueResult {
            token: t.token,
            dut_id: t.dut_id,
            action: t.action,
            expires_in_secs: t.expires_in_secs,
        }
    }

    fn approval_validate(
        &self,
        token: &str,
        dut_id: &str,
        action: &str,
        args_json: &str,
    ) -> Result<(), ApprovalErr> {
        self.approval
            .validate(token, dut_id, action, args_json)
            .map_err(|kind| ApprovalErr { kind })
    }
}

/// `notify_user` bridge: the MCP tool hands a validated notice to the GUI
/// alert center and gets back the outcome it actually observed.
///
/// Rejection order is fixed (`enabled → suppressed → dedup → capacity →
/// rate`) so a switched-off DUT never looks like a suppression, and a
/// dedup rejection never consumes a rate token. Messages and titles are
/// sanitized/truncated again here even though the server already did it: the
/// host must not trust a caller-supplied string to be bounded.
impl UserNotifier for RuntimeMcpState {
    fn notify(&self, request: &NotifyRequest) -> NotifyOutcome {
        let now = crate::alerts::now_ms();
        let dut_id = request.dut_id.clone().unwrap_or_default();
        let Ok(mut center) = self.alerts.lock() else {
            return NotifyOutcome::Disabled;
        };
        if let Err(rejection) = center.check_report(&dut_id, AlertSource::AgentNotice, now) {
            return match rejection {
                ReportRejection::Disabled => NotifyOutcome::Disabled,
                ReportRejection::Suppressed => NotifyOutcome::Suppressed,
                ReportRejection::Duplicate => NotifyOutcome::Throttled(ThrottleReason::Dedup),
                ReportRejection::Capacity => NotifyOutcome::Throttled(ThrottleReason::Capacity),
            };
        }
        if !self
            .notify_limiter
            .try_acquire(request.credential_id.as_deref(), now)
        {
            return NotifyOutcome::Throttled(ThrottleReason::Rate);
        }
        let (title, detail) =
            crate::alerts::agent_notice_alert_texts(request.title.as_deref(), &request.message);
        let level = match request.level {
            NotifyLevel::Info => AlertLevel::Info,
            NotifyLevel::Warning => AlertLevel::Warning,
            NotifyLevel::Error => AlertLevel::Error,
        };
        // The server already sanitized the strings; repeat it here because the
        // host must never enqueue an unbounded/control-character string.
        let title = duthub_mcp::notify::sanitize_notify_text(&title, TITLE_MAX_CHARS);
        let detail = duthub_mcp::notify::sanitize_notify_text(&detail, MESSAGE_MAX_CHARS);
        match center.report_checked_at_for(
            now,
            &dut_id,
            AlertSource::AgentNotice,
            level,
            title,
            detail,
        ) {
            Ok(()) => NotifyOutcome::Queued,
            Err(ReportRejection::Disabled) => NotifyOutcome::Disabled,
            Err(ReportRejection::Suppressed) => NotifyOutcome::Suppressed,
            Err(ReportRejection::Duplicate) => NotifyOutcome::Throttled(ThrottleReason::Dedup),
            Err(ReportRejection::Capacity) => NotifyOutcome::Throttled(ThrottleReason::Capacity),
        }
    }
}

/// MCP 运行时监听器句柄集合（F1/T16）：主端点 + 附加/每 DUT 端点。
///
/// `spawn_mcp_runtime` 返回本结构；Workbench 持有并在退出时 [`Self::shutdown`]，
/// 先发停止信号再 join 服务线程，确保端口/线程在进程退出前释放。
#[derive(Default)]
pub struct McpRuntimeHandles {
    snapshot_writer: Option<SnapshotWriter>,
    pub main: Option<ListenerHandle>,
    pub extras: Vec<ListenerHandle>,
    /// W4a：绑定/启动失败记录 `(port, reason)`。bind 失败不再只有
    /// eprintln —— Agent 接入页与连接管理 MCP 卡据此显示实际监听与失败
    /// 原因，避免「配了端口却静默没监听」。
    pub failures: Vec<(u16, String)>,
}

impl McpRuntimeHandles {
    /// 关闭全部 HTTP 监听器（每个最多等待 500ms；超时后端口仍会随线程退出释放）。
    pub fn shutdown(&mut self) {
        if let Some(writer) = self.snapshot_writer.take() {
            writer.shutdown();
        }
        let timeout = Duration::from_millis(500);
        if let Some(handle) = self.main.take() {
            let _ = handle.shutdown_and_join_with_timeout(timeout);
        }
        for handle in self.extras.drain(..) {
            let _ = handle.shutdown_and_join_with_timeout(timeout);
        }
    }

    /// W4a：某端口的失败原因（UI 展示用）；未失败返回 None。
    pub fn failure_for_port(&self, port: u16) -> Option<&str> {
        self.failures
            .iter()
            .find(|(failed, _)| *failed == port)
            .map(|(_, reason)| reason.as_str())
    }
}

struct SnapshotWriter {
    stop: mpsc::Sender<()>,
    finished: mpsc::Receiver<()>,
}

impl SnapshotWriter {
    fn spawn(
        snapshot: Arc<Mutex<duthub_mcp::snapshot::StateSnapshot>>,
        path: PathBuf,
        interval: Duration,
    ) -> std::io::Result<Self> {
        let (stop, commands) = mpsc::channel();
        let (finished, completion) = mpsc::channel();
        std::thread::Builder::new()
            .name("duthub-mcp-snapshot".into())
            .spawn(move || {
                let mut last_digest: Option<String> = None;
                while commands.recv_timeout(interval) == Err(mpsc::RecvTimeoutError::Timeout) {
                    let current = snapshot.lock().unwrap().clone();
                    // Skip SSD writes when snapshot content (excluding timestamp) is unchanged.
                    let content_digest = snapshot_content_digest(&current);
                    if last_digest.as_deref() == Some(content_digest.as_str()) {
                        continue;
                    }
                    if let Err(error) = duthub_core::store::save_json_atomic(&path, &current) {
                        eprintln!("[mcp] writing snapshot {}: {error}", path.display());
                    } else {
                        last_digest = Some(content_digest);
                    }
                }
                let _ = finished.send(());
            })?;
        Ok(Self {
            stop,
            finished: completion,
        })
    }

    fn shutdown(self) {
        let _ = self.stop.send(());
        if self.finished.recv_timeout(Duration::from_millis(500))
            == Err(mpsc::RecvTimeoutError::Timeout)
        {
            eprintln!("[mcp] snapshot writer did not finish before the shutdown deadline");
        }
    }
}

fn snapshot_content_digest(snapshot: &duthub_mcp::snapshot::StateSnapshot) -> String {
    let mut normalized = snapshot.clone();
    normalized.ts_unix_ms = 0;
    let bytes = serde_json::to_vec(&normalized).unwrap_or_default();
    duthub_core::util::sha256_hex(&bytes)
}

/// `待启动端点：(is_main`, port, listener id, bound DUT id)。
type EndpointSpec = (bool, u16, Option<Arc<str>>, Option<Arc<str>>);

/// 启动 MCP HTTP 端点（默认 `127.0.0.1:{port}`；`lan_requested` 时绑 `0.0.0.0`，
/// 同一端口同时接受回环与局域网地址）与快照落盘循环（后台线程，失败仅打印 stderr）。
/// 局域网绑定在认证关闭时 fail closed，回退 127.0.0.1。
/// 主端点与各附加监听器的 [`ListenerHandle`] 随返回值交给调用方持有（T16）。
/// W4a：绑定/启动失败记录进 [`McpRuntimeHandles::failures`]，不再静默丢失。
#[allow(clippy::too_many_arguments)] // 与 RuntimeMcpState::new 同理由：运行时装配参数由 Workbench 一次性注入
pub fn spawn_mcp_runtime(
    state: &Arc<dyn duthub_mcp::state::McpState>,
    notifier: Option<&Arc<dyn duthub_mcp::notify::UserNotifier>>,
    access: Option<&Arc<duthub_mcp::auth_http::AccessContext>>,
    port: u16,
    lan_requested: bool,
    listeners: Vec<duthub_core::agent_access::AgentListener>,
    snapshot: Arc<Mutex<duthub_mcp::snapshot::StateSnapshot>>,
    data_dir: &std::path::Path,
    main_bound_endpoint: &OnceLock<SocketAddr>,
) -> McpRuntimeHandles {
    let auth_enabled = access.is_some_and(|ctx| ctx.auth.settings().enabled);
    let lan_access = lan_requested && auth_enabled;
    if lan_requested && !auth_enabled {
        eprintln!("[mcp] 已配置局域网访问但认证未开启：回退 127.0.0.1（fail closed）");
    }
    let bind_ip = if lan_access {
        Ipv4Addr::UNSPECIFIED
    } else {
        Ipv4Addr::LOCALHOST
    };
    let endpoints: Vec<EndpointSpec> = std::iter::once((true, port, None, None))
        .chain(listeners.into_iter().map(|listener| {
            (
                false,
                listener.port,
                Some(Arc::<str>::from(listener.id)),
                listener.dut_id.map(Arc::<str>::from),
            )
        }))
        .collect();
    let mut handles = McpRuntimeHandles::default();
    for (is_main, port, listener_id, listener_dut) in endpoints {
        let bind_addr = SocketAddr::from((bind_ip, port));
        let listener = match std::net::TcpListener::bind(bind_addr) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("[mcp] 绑定 {bind_addr} 失败: {error}");
                handles
                    .failures
                    .push((port, format!("绑定 {bind_addr} 失败: {error}")));
                continue;
            }
        };
        if is_main && let Ok(addr) = listener.local_addr() {
            let _ = main_bound_endpoint.set(duthub_mcp::transport::advertised_endpoint(addr));
        }
        match duthub_mcp::transport::spawn_http_listener_on(
            listener,
            state.clone(),
            access.cloned(),
            listener_id,
            listener_dut,
            notifier.cloned(),
        ) {
            Ok(handle) => {
                if is_main {
                    handles.main = Some(handle);
                } else {
                    handles.extras.push(handle);
                }
            }
            Err(error) => {
                eprintln!("[mcp] 启动 {bind_addr} 失败: {error}");
                handles
                    .failures
                    .push((port, format!("启动 HTTP 服务失败: {error}")));
            }
        }
    }

    let path = data_dir.join("runtime").join("mcp-state.json");
    match SnapshotWriter::spawn(snapshot, path, Duration::from_secs(2)) {
        Ok(writer) => handles.snapshot_writer = Some(writer),
        Err(error) => eprintln!("[mcp] starting snapshot writer: {error}"),
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::NotifyRateLimiter;

    #[test]
    fn credential_bucket_allows_burst_then_refills() {
        let limiter = NotifyRateLimiter::new();
        assert!(limiter.try_acquire(Some("cred-a"), 0));
        assert!(limiter.try_acquire(Some("cred-a"), 0));
        assert!(
            !limiter.try_acquire(Some("cred-a"), 0),
            "3/min with burst 2 must refuse the third immediate call"
        );
        // 3/min = one token every 20s.
        assert!(!limiter.try_acquire(Some("cred-a"), 19_000));
        assert!(limiter.try_acquire(Some("cred-a"), 20_000));
    }

    #[test]
    fn one_credential_exhaustion_does_not_starve_another() {
        let limiter = NotifyRateLimiter::new();
        assert!(limiter.try_acquire(Some("cred-a"), 0));
        assert!(limiter.try_acquire(Some("cred-a"), 0));
        assert!(!limiter.try_acquire(Some("cred-a"), 0));
        assert!(
            limiter.try_acquire(Some("cred-b"), 0),
            "credential B keeps its own bucket"
        );
    }

    #[test]
    fn global_bucket_caps_the_total_rate() {
        let limiter = NotifyRateLimiter::new();
        for credential in ["c1", "c2"] {
            assert!(limiter.try_acquire(Some(credential), 0));
            assert!(limiter.try_acquire(Some(credential), 0));
        }
        assert!(
            !limiter.try_acquire(Some("c3"), 0),
            "the global burst of 4 is exhausted before c3's first call"
        );
        assert!(limiter.try_acquire(Some("c3"), 24_000), "10/min refills");
    }

    #[test]
    fn local_admin_uses_only_the_global_bucket() {
        let limiter = NotifyRateLimiter::new();
        for _ in 0..4 {
            assert!(limiter.try_acquire(None, 0));
        }
        assert!(!limiter.try_acquire(None, 0));
    }
}
