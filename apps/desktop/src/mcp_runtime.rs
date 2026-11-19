//! MCP 运行时桥接：desktop 状态 → duthub_mcp::McpState。
//!
//! - HTTP 端点（127.0.0.1:8797/mcp）后台线程托管（desktop 启动即开）。
//! - Workbench 每帧把 DUT 摘要 + 最近日志推入共享快照；快照写入循环约 2s 落盘
//!   工作目录 `runtime/mcp-state.json`（stdio 快照子进程经同源解析读取）。
//! - 审批令牌在 RuntimeMcpState 内单次消费。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use duthub_core::ApprovalManager;
use duthub_core::lease::{ControlLeaseManager, LeaseError, LeasePhase, WriteIdentity};
use duthub_device::DeviceService;
use duthub_device::control::{ControlAction, ControlEntryError, ControlService};
use duthub_device::heartbeat::HeartbeatService;
use duthub_mcp::auth_http::{AccessContext, lease_error_code};
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, ControlStatusDto, DutInfo, DutSummary,
    HeartbeatUpdate, HeartbeatViewDto, LeaseGrantDto, LogLineDto, LogReadResult, McpState,
    ProcessResultDto,
};

use crate::alerts::{AlertCenter, AlertSource};

pub struct RuntimeMcpState {
    device: Arc<DeviceService>,
    snapshot: Arc<std::sync::Mutex<duthub_mcp::snapshot::StateSnapshot>>,
    approval: Arc<ApprovalManager>,
    /// 共用设备操作入口：控制动作的授权/绑定/能力校验与驱动派发都收口在此。
    control: Arc<ControlService>,
    /// 心跳引擎（需求⑦）：Agent 经 heartbeat.set 改写命令/间隔。
    heartbeat: Arc<HeartbeatService>,
    media_urls: crate::media::MediaUrls,
    /// 异常弹窗提醒中心（需求⑤）：MCP 侧控制动作失败经此回 GUI 弹窗
    ///（HTTP 线程与 Workbench 共用同一实例）。
    alerts: Arc<std::sync::Mutex<AlertCenter>>,
    /// 工作目录（`resolve_work_dir_in` 同源解析）：skill_export 落盘锚点。
    work_dir: PathBuf,
    /// Agent 接入上下文（认证/租约/审计）：控制权四件套与写工具租约复核的
    /// 唯一租约管理器实例（与 DeviceService 写入通道共享）。
    access: Option<Arc<AccessContext>>,
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
        alerts: Arc<std::sync::Mutex<AlertCenter>>,
        work_dir: PathBuf,
        access: Option<Arc<AccessContext>>,
    ) -> Self {
        Self {
            device,
            snapshot,
            approval,
            control,
            heartbeat,
            media_urls,
            alerts,
            work_dir,
            access,
        }
    }

    /// 控制动作统一走共用设备操作入口；授权错误带类别码便于 MCP 客户端感知。
    /// Device 错误（动作真正执行失败）同时转弹窗提醒（需求⑤接入点③）；
    /// ApprovalRequired 是审批协议的正常环节（客户端据此走 approval.request），
    /// 不算异常、不弹窗。
    fn control_execute(
        &self,
        dut_id: &str,
        action: ControlAction,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        let action_label = action.mark_label().to_string();
        self.control
            .execute_by_id_as(dut_id, action, approval_token, &writer)
            .map_err(|err| match err {
                ControlEntryError::Approval(approval) => {
                    format!(
                        "ApprovalRequired/{}: {}",
                        code_of(&approval),
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

    /// 继电器写动作（relay_test）失败同样转弹窗提醒；只读查询（relay_status）不算动作。
    fn relay_test_execute(
        &self,
        dut_id: &str,
        channel: u8,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<duthub_core::backend::ChannelTestReport, String> {
        self.control
            .channel_test_as(dut_id, channel, approval_token, &writer)
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

    /// 弹窗提醒入队（去重/抑制/总开关都在 AlertCenter 内裁决）。
    fn report_control_failure(&self, dut_id: &str, action: &str, err: &str) {
        let (title, detail) = crate::alerts::control_action_alert_texts(dut_id, action, err);
        let _ = self
            .alerts
            .lock()
            .map(|mut center| center.report_for(dut_id, AlertSource::Control, title, detail));
    }
}

/// 租约错误 → `LeaseRequired/{code}: {message}`（与 server.rs 前缀约定一致）。
fn lease_err(err: &LeaseError) -> String {
    format!("LeaseRequired/{}: {}", lease_error_code(err), err.message())
}

fn code_of(err: &duthub_core::ApprovalError) -> &'static str {
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
                    text: text.to_string(),
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
        duthub_adb::list_local_server_devices()
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
        Err("adb.shell 经 MCP 暂未开放（与 GUI 审批面接线后启用）".into())
    }

    fn control_power(
        &self,
        dut_id: &str,
        on: bool,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_execute(dut_id, ControlAction::Power { on }, approval_token, writer)
    }

    fn control_reset(
        &self,
        dut_id: &str,
        approval_token: &str,
        writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_execute(dut_id, ControlAction::Reset, approval_token, writer)
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
            writer,
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
        self.relay_test_execute(dut_id, channel, approval_token, writer)
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
                cfg.command = command.trim().to_owned();
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
        Err("flash.test 经 MCP 暂未开放（preset_id 按 settings.flashTools[].id 解析与 GUI 接线后启用）".into())
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

/// 启动 MCP HTTP 端点（127.0.0.1:{port}，默认 8797；偏差 8：settings 可配）与
/// 快照落盘循环（后台线程，失败仅打印 stderr）。
pub fn spawn_mcp_runtime(
    state: Arc<dyn duthub_mcp::state::McpState>,
    access: Option<Arc<duthub_mcp::auth_http::AccessContext>>,
    port: u16,
    listeners: Vec<duthub_core::agent_access::AgentListener>,
    snapshot: Arc<Mutex<duthub_mcp::snapshot::StateSnapshot>>,
    data_dir: PathBuf,
    main_bound_port: Arc<OnceLock<u16>>,
) {
    let extra_endpoints = listeners
        .into_iter()
        .map(|listener| (Some(Arc::<str>::from(listener.id)), listener.port));
    for (id, port) in std::iter::once((None, port)).chain(extra_endpoints) {
        let state = state.clone();
        let access = access.clone();
        let main_bound_port = main_bound_port.clone();
        std::thread::spawn(move || {
            let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => listener,
                Err(err) => {
                    eprintln!("[mcp] 绑定 127.0.0.1:{port} 失败: {err}");
                    return;
                }
            };
            if id.is_none()
                && let Ok(addr) = listener.local_addr()
            {
                let _ = main_bound_port.set(addr.port());
            }
            if let Err(err) =
                duthub_mcp::transport::serve_http_listener_blocking(state, access, listener, id)
            {
                eprintln!("[mcp] HTTP 127.0.0.1:{port} 退出: {err}");
            }
        });
    }

    // 快照落盘循环（2s 间隔）
    std::thread::spawn(move || {
        let runtime_dir = data_dir.join("runtime");
        let _ = std::fs::create_dir_all(&runtime_dir);
        let path = runtime_dir.join("mcp-state.json");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let snap = snapshot.lock().unwrap().clone();
            if let Ok(json) = serde_json::to_string(&snap) {
                let _ = std::fs::write(&path, json);
            }
        }
    });
}
