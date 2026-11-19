//! 共用设备操作入口（控制单元）：GUI 与 MCP 的 Power/Reset/PowerKey 都经此执行。
//!
//! 控制服务的职责：
//! 1. **授权**：审批令牌在此统一校验并消费（绑定 dut/action/参数）；
//! 2. **绑定**：DUT 必须已配置控制单元（继电器 / SNMP PDU）；
//! 3. **动作能力**：PDU 只支持电源通断；继电器按通道映射校验动作可用；
//! 4. **解析**：把 DUT 配置解析成 [`ControlTarget`]（设备地址 + 驱动 + 通道）。
//!
//! [`duthub_core::ControlBackend`] 驱动只接收已解析的地址与动作，
//! 不接触审批令牌 —— 授权职责不再分散到入口与驱动两处。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::backend::ChannelTestReport;
use duthub_core::lease::{
    ControlAdmission, ControlLeaseManager, LeaseAuthority, WriteCtx, WriteIdentity,
};
use duthub_core::model::{DutConfig, RelayBoard, RelayConfig};
use duthub_core::{
    ApprovalError, ApprovalManager, ControlBackend, ControlTarget, DeviceError, DeviceResult,
};

use crate::relay_watch::{RelayBoardProbe, RelayProbeSample, classify_probe_error};

/// 控制动作（名称与参数串与 MCP `approval.request` 的 action/args 绑定约定一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlAction {
    Power { on: bool },
    Reset,
    PowerKey { long_press: bool },
}

/// 继电器动作完成后的分段标记接收端（日志分割）：参数 `(dut_id, label)`，
/// 由宿主接到 [`crate::DeviceService::log_mark`]。会话未开时静默丢弃（标记是
/// 附属注记，不应让控制动作本身报错）。动作已完成后的结果注记失败不拦截
/// 任何东西——动作的成败由后端结果决定。
pub type LogMarkSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// 供电前分段接收端：与 [`LogMarkSink`] 不同，返回 mark 落盘成败——LOG-5
/// 「分段失败拦截派发」：分段未确认落盘时控制入口不得派发设备动作（不动
/// 设备），失败由宿主实现（[`crate::DeviceService::log_control_boundary`]）
/// 同步返回。
pub type PowerBoundarySink = Arc<dyn Fn(&str, &str) -> DeviceResult<()> + Send + Sync>;

/// 继电器测试动作名：单通道 ON→STATUS→OFF 往返（写动作，走审批闸门）。
pub const ACTION_CHANNEL_TEST: &str = "control.test";

impl ControlAction {
    /// 审批动作名（与 docs/API_CONTRACT.md 一致）。
    pub fn name(&self) -> &'static str {
        match self {
            ControlAction::Power { .. } => "control.power",
            ControlAction::Reset => "control.reset",
            ControlAction::PowerKey { .. } => "control.power_key",
        }
    }

    /// 动作的分段标记文本（`[mark] {label}` 的 label 部分）。
    pub fn mark_label(&self) -> &'static str {
        match self {
            ControlAction::Power { on: true } => "继电器 power ON",
            ControlAction::Power { on: false } => "继电器 power OFF",
            ControlAction::Reset => "继电器 reset",
            ControlAction::PowerKey { long_press: true } => "继电器 power key 长按",
            ControlAction::PowerKey { long_press: false } => "继电器 power key 短按",
        }
    }

    /// 参数绑定串：签发与消费必须产出完全一致的 JSON。
    pub fn args_json(&self) -> String {
        match self {
            ControlAction::Power { on } => serde_json::json!({ "on": on }).to_string(),
            ControlAction::Reset => serde_json::json!({}).to_string(),
            ControlAction::PowerKey { long_press } => {
                serde_json::json!({ "long_press": long_press }).to_string()
            }
        }
    }
}

/// 授权阶段的错误（与 `ApprovalError` 一一对应，供 MCP 映射提示语）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEntryError {
    Approval(ApprovalError),
    Device(DeviceError),
}

impl std::fmt::Display for ControlEntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlEntryError::Approval(err) => write!(f, "{}", err.message()),
            ControlEntryError::Device(err) => write!(f, "{err}"),
        }
    }
}

impl From<DeviceError> for ControlEntryError {
    fn from(err: DeviceError) -> Self {
        ControlEntryError::Device(err)
    }
}

/// 共用设备操作入口。`dut_configs` 由宿主（桌面端每帧刷新）注册，
/// 使入口能在不依赖 UI 状态的情况下解析 DUT 绑定。
pub struct ControlService {
    backend: Option<Arc<dyn ControlBackend>>,
    approval: Arc<ApprovalManager>,
    dut_configs: Mutex<HashMap<String, DutConfig>>,
    /// 分段标记接收端（日志分割）：动作成功/失败后写入 `[mark] …` 行。
    /// OnceLock：宿主在构造后接一次，之后所有动作共用。
    log_sink: OnceLock<LogMarkSink>,
    power_boundary: OnceLock<PowerBoundarySink>,
    power_outcome: OnceLock<LogMarkSink>,
    runtime: crate::RuntimeOwner,
    relay_writes: std::sync::RwLock<()>,
    /// 控制租约复核（方案 M10：approval 校验前插 admit，不通过不消费令牌）。
    /// 与 DeviceService 写入通道共用同一 `ControlLeaseManager` 实例。
    lease: Option<Arc<ControlLeaseManager>>,
    /// 审计（control.execute / control.deny）。
    audit: Option<Arc<AuditSink>>,
}

impl ControlService {
    /// Construct a `ControlService` with its own background Tokio runtime.
    pub fn new(backend: Option<Arc<dyn ControlBackend>>, approval: Arc<ApprovalManager>) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("control tokio runtime");
        Self::with_runtime_owner(backend, approval, crate::RuntimeOwner::Owned(runtime))
    }

    /// Construct a `ControlService` sharing an externally owned Tokio runtime handle.
    pub fn with_runtime(
        backend: Option<Arc<dyn ControlBackend>>,
        approval: Arc<ApprovalManager>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self::with_runtime_owner(backend, approval, crate::RuntimeOwner::Shared(handle))
    }

    fn with_runtime_owner(
        backend: Option<Arc<dyn ControlBackend>>,
        approval: Arc<ApprovalManager>,
        runtime: crate::RuntimeOwner,
    ) -> Self {
        Self {
            backend,
            approval,
            dut_configs: Mutex::new(HashMap::new()),
            log_sink: OnceLock::new(),
            power_boundary: OnceLock::new(),
            power_outcome: OnceLock::new(),
            runtime,
            lease: None,
            relay_writes: std::sync::RwLock::new(()),
            audit: None,
        }
    }

    /// 接线控制租约复核（builder）。GUI 按钮路径走 [`Self::execute_by_id`]（Gui
    /// 身份）；MCP 路径走 [`Self::execute_by_id_as`]（Agent 身份）。
    pub fn with_lease_authority(mut self, lease: Arc<ControlLeaseManager>) -> Self {
        self.lease = Some(lease);
        self
    }

    /// 接线审计接收端（control.execute / control.deny）。
    pub fn with_audit_sink(mut self, audit: Arc<AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// [C'] 控制动作租约复核：Gui 无租约时现场获取短租约（执行后释放）；
    /// Agent 凭 lease 通过；拒绝 → 审计 control.deny。
    fn admit_control(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
    ) -> Result<ControlAdmission, DeviceError> {
        let Some(lease) = &self.lease else {
            return Ok(ControlAdmission {
                ctx: None,
                temp_lease: None,
            });
        };
        match lease.admit_control(dut_id, writer) {
            Ok(admission) => Ok(admission),
            Err(err) => {
                if let Some(audit) = &self.audit {
                    audit.record(
                        "control.deny",
                        None,
                        None,
                        Some(dut_id),
                        AuditOutcome::Denied,
                        Some(&err.message()),
                    );
                }
                Err(err.to_device_error())
            }
        }
    }

    /// 临时租约收尾（幂等）+ 执行审计。
    fn finish_control(
        &self,
        dut_id: &str,
        admission: &ControlAdmission,
        action_name: &str,
        ok: bool,
    ) {
        if let (Some(lease), Some(grant)) = (&self.lease, &admission.temp_lease) {
            lease.release_temporary(
                dut_id,
                &WriteCtx {
                    lease_id: grant.lease_id.clone(),
                    generation: grant.generation,
                },
            );
        }
        if let Some(audit) = &self.audit {
            audit.record(
                "control.execute",
                None,
                None,
                Some(dut_id),
                if ok {
                    AuditOutcome::Ok
                } else {
                    AuditOutcome::Error
                },
                Some(action_name),
            );
        }
    }

    /// 挂分段标记接收端（日志分割；重复挂载忽略后者并返回 false）。
    pub fn set_log_sink(&self, sink: LogMarkSink) -> bool {
        self.log_sink.set(sink).is_ok()
    }

    pub fn set_power_boundary_sink(&self, sink: PowerBoundarySink) -> bool {
        self.power_boundary.set(sink).is_ok()
    }

    pub fn set_power_outcome_sink(&self, sink: LogMarkSink) -> bool {
        self.power_outcome.set(sink).is_ok()
    }

    /// 供电前分段：经宿主 sink 同步落盘。失败 → 拦截派发（LOG-5：分段必须
    /// 先于控制指令落盘，未落盘不动设备）。
    fn begin_power_boundary(&self, dut_id: &str, on: bool) -> DeviceResult<()> {
        match self.power_boundary.get() {
            Some(sink) => sink(dut_id, if on { "Power ON" } else { "Power OFF" }),
            None => Ok(()),
        }
    }

    /// 动作收尾：向日志写 `[mark] {label}`（成功/失败同写，失败带原因）。
    fn emit_action_mark(&self, dut_id: &str, label: &str, outcome: Result<(), &DeviceError>) {
        self.emit_control_mark(self.log_sink.get(), dut_id, label, outcome);
    }

    fn emit_power_outcome(&self, dut_id: &str, label: &str, outcome: Result<(), &DeviceError>) {
        self.emit_control_mark(
            self.power_outcome.get().or_else(|| self.log_sink.get()),
            dut_id,
            label,
            outcome,
        );
    }

    fn emit_control_mark(
        &self,
        sink: Option<&LogMarkSink>,
        dut_id: &str,
        label: &str,
        outcome: Result<(), &DeviceError>,
    ) {
        if let Some(sink) = sink {
            let text = match outcome {
                Ok(()) => label.to_string(),
                Err(err) => format!("{label} 失败: {err}"),
            };
            sink(dut_id, &text);
        }
    }

    /// 注册/更新 DUT 配置（入口据此校验绑定与解析地址）。
    pub fn register_dut(&self, dut: DutConfig) {
        self.dut_configs.lock().unwrap().insert(dut.id.clone(), dut);
    }

    pub fn unregister_dut(&self, dut_id: &str) {
        self.dut_configs.lock().unwrap().remove(dut_id);
    }

    /// 按 DUT ID 执行（GUI 按钮路径，Gui 身份：人类在场）。
    pub fn execute_by_id(
        &self,
        dut_id: &str,
        action: ControlAction,
        approval_token: &str,
    ) -> Result<(), ControlEntryError> {
        self.execute_by_id_as(dut_id, action, approval_token, &WriteIdentity::gui())
    }

    pub(crate) fn execute_confirmed_power(
        &self,
        dut_id: &str,
        on: bool,
    ) -> Result<(), ControlEntryError> {
        let action = ControlAction::Power { on };
        let approval = self
            .approval
            .issue(dut_id, action.name(), &action.args_json());
        self.execute_by_id(dut_id, action, &approval.token)
    }

    pub fn power_configured(&self, dut_id: &str) -> bool {
        self.backend.is_some()
            && self
                .dut_configs
                .lock()
                .unwrap()
                .get(dut_id)
                .is_some_and(|dut| resolve_target(dut, &ControlAction::Power { on: true }).is_ok())
    }

    /// 按 DUT ID 执行（显式身份）：MCP 写路径传 `WriteIdentity::agent(...)`。
    pub fn execute_by_id_as(
        &self,
        dut_id: &str,
        action: ControlAction,
        approval_token: &str,
        writer: &WriteIdentity,
    ) -> Result<(), ControlEntryError> {
        let dut = self
            .dut_configs
            .lock()
            .unwrap()
            .get(dut_id)
            .cloned()
            .ok_or(DeviceError::NotFound)?;
        self.execute(&dut, action, approval_token, writer)
    }

    /// 共用入口主流程：绑定/能力 → 后端就绪 → **租约复核** → 授权 → 派发已解析动作。
    ///
    /// 租约复核在 approval 校验之前（M10：admit 不通过不消费令牌——GUI 忙时
    /// Agent 重试不烧审批令牌）。
    pub fn execute(
        &self,
        dut: &DutConfig,
        action: ControlAction,
        approval_token: &str,
        writer: &WriteIdentity,
    ) -> Result<(), ControlEntryError> {
        // 1) 绑定 + 动作能力校验 + 地址/通道解析（失败不消费令牌）
        let target = resolve_target(dut, &action)?;
        // 2) 驱动就绪校验（同样不消费令牌）
        let backend = self
            .backend
            .clone()
            .ok_or(ControlEntryError::Device(DeviceError::Unsupported))?;
        let _agent_action = match (&self.lease, writer) {
            (Some(lease), WriteIdentity::Agent { .. }) => Some(
                lease
                    .begin_agent_action()
                    .map_err(|err| err.to_device_error())?,
            ),
            _ => None,
        };
        let _relay_write = self
            .relay_writes
            .try_read()
            .map_err(|_| DeviceError::Busy)?;
        // 3) 租约复核（M10：不通过不消费令牌 → 先 admit 后 validate）
        let admission = self
            .admit_control(&dut.id, writer)
            .map_err(ControlEntryError::from)?;
        // 4) 授权：令牌绑定 dut + action + 参数，单次消费
        let authorized =
            self.approval
                .validate(approval_token, &dut.id, action.name(), &action.args_json());
        if let Err(err) = authorized {
            // 令牌未消费成功 → 立即释放临时租约，不留悬挂占用
            self.finish_control(&dut.id, &admission, action.name(), false);
            return Err(ControlEntryError::Approval(err));
        }
        // 5) 派发：驱动接收已解析的设备地址与动作；
        //    成功/失败都写分段标记（日志分割，B2）
        let label = action.mark_label();
        let action_name = action.name();
        let is_power = matches!(action, ControlAction::Power { .. });
        if let ControlAction::Power { on } = action {
            // LOG-5：分段未确认落盘 → 拦截派发（不动设备），回收本次租约并
            // 登记失败审计。令牌已在 validate 消费，重试需重新申请审批。
            if let Err(err) = self.begin_power_boundary(&dut.id, on) {
                self.finish_control(&dut.id, &admission, action_name, false);
                return Err(ControlEntryError::Device(err));
            }
        }
        let outcome = self.runtime.block_on(async move {
            match action {
                ControlAction::Power { on } => backend.power(&target, on).await,
                ControlAction::Reset => backend.reset(&target).await,
                ControlAction::PowerKey { long_press } => {
                    backend.power_key(&target, long_press).await
                }
            }
        });
        if is_power {
            self.emit_power_outcome(&dut.id, label, outcome.as_ref().map(|_| ()));
        } else {
            self.emit_action_mark(&dut.id, label, outcome.as_ref().map(|_| ()));
        }
        self.finish_control(&dut.id, &admission, action_name, outcome.is_ok());
        outcome.map_err(ControlEntryError::from)
    }

    /// 继电器逐通道状态查询（只读，无需审批）：解析 DUT 继电器节点后派发后端。
    /// 仅继电器类后端支持（PDU 报 Unsupported）；板卡不回答 STATUS 时返回空表。
    pub fn channel_status(&self, dut_id: &str) -> Result<Vec<(u8, bool)>, ControlEntryError> {
        let dut = self
            .dut_configs
            .lock()
            .unwrap()
            .get(dut_id)
            .cloned()
            .ok_or(DeviceError::NotFound)?;
        let node = relay_node(&dut)?;
        let backend = self
            .backend
            .clone()
            .ok_or(ControlEntryError::Device(DeviceError::Unsupported))?;
        self.runtime
            .block_on(async move { backend.channel_status(&node).await })
            .map_err(ControlEntryError::from)
    }

    /// Single-channel read-only probe for the board-level offline monitor.
    ///
    /// This is deliberately not a control action: no approval token, no control
    /// lease, no `[mark]` line, no DUT state change. The shared relay write gate
    /// is taken with `try_read`, so a press-and-hold test (which holds
    /// `try_write` for its whole duration) makes the probe `Gated` immediately
    /// instead of queueing behind it. Backend failures are classified by
    /// [`classify_probe_error`]; only a vanished port or a silent board counts
    /// as a fault.
    pub fn probe_board_status(&self, node: &str, channel: u8) -> RelayProbeSample {
        if node.trim().is_empty() {
            return RelayProbeSample::Unsupported("继电器节点（串口）未配置".into());
        }
        let Some(backend) = self.backend.clone() else {
            return RelayProbeSample::Unsupported("控制后端未接线".into());
        };
        let Ok(_read_gate) = self.relay_writes.try_read() else {
            return RelayProbeSample::Gated("继电器按住测试进行中".into());
        };
        match self
            .runtime
            .block_on(async move { backend.probe_channel(node, channel).await })
        {
            Ok(Some(on)) => RelayProbeSample::Answered(on),
            Ok(None) => RelayProbeSample::Silent,
            Err(error) => classify_probe_error(&error),
        }
    }

    /// Upper bound a press-and-hold waits for the relay write gate.
    ///
    /// The board monitor holds the read gate for one STATUS round-trip
    /// (≤300 ms on a healthy board). A press that lands in that window waits
    /// instead of failing with `Busy`; a wedged probe (30 s port timeout)
    /// still gives up and reports `Busy` after this bound, so the UI is never
    /// stuck behind a dead port.
    const RELAY_WRITE_GATE_WAIT: std::time::Duration = std::time::Duration::from_millis(1_000);

    /// Acquire the exclusive relay write gate, waiting briefly for an
    /// in-flight board probe.
    ///
    /// Safety/ordering is unchanged: the gate is still exclusive for the whole
    /// hold, only the acquisition retries for a bounded time.
    fn acquire_relay_write_gate(&self) -> Result<std::sync::RwLockWriteGuard<'_, ()>, DeviceError> {
        let deadline = std::time::Instant::now() + Self::RELAY_WRITE_GATE_WAIT;
        loop {
            match self.relay_writes.try_write() {
                Ok(guard) => return Ok(guard),
                Err(_) if std::time::Instant::now() >= deadline => {
                    return Err(DeviceError::Busy);
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }

    /// Local press-and-hold test. Dropping the release sender always schedules OFF
    /// for the same physical target, even if the original DUT binding changes.
    pub fn hold_channel(
        &self,
        dut: &DutConfig,
        channel: u8,
        mut released: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), ControlEntryError> {
        let _exclusive = self.acquire_relay_write_gate()?;
        let dut_id = dut.id.as_str();
        let address = relay_node(dut)?;
        let RelayConfig::UsbSerial {
            channels, driver, ..
        } = &dut.relay
        else {
            return Err(DeviceError::Unsupported.into());
        };
        if channel == 0
            || ![
                channels.power,
                channels.reset,
                channels.power_key,
                channels.maskrom,
            ]
            .into_iter()
            .flatten()
            .any(|assigned| assigned == channel)
        {
            return Err(DeviceError::Invalid("通道未绑定到可测试的 DUT 功能".into()).into());
        }
        let backend = self.backend.as_ref().ok_or(DeviceError::Unsupported)?;
        let _handoff = self
            .lease
            .as_ref()
            .map(|lease| lease.serial_io().try_read().map_err(|_| DeviceError::Busy))
            .transpose()?;
        let admission = self.admit_control(dut_id, &WriteIdentity::gui())?;
        let operation = "control.hold_test";
        if !matches!(
            released.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ) {
            self.finish_control(dut_id, &admission, operation, true);
            return Ok(());
        }
        // The relay write gate owns this hold. A relay transport failure must
        // not move the separate serial session into DUT-wide inspection.
        let target = ControlTarget {
            address,
            driver: driver.clone(),
            channel: u16::from(channel),
        };
        let supply = channels.power == Some(channel);
        if supply {
            // LOG-5：闭合分段未落盘 → 不派发（未动设备，无需 OFF 补偿）。
            if let Err(err) = self.begin_power_boundary(dut_id, true) {
                self.finish_control(dut_id, &admission, operation, false);
                return Err(err.into());
            }
        }
        let closed = self.runtime.block_on(backend.power(&target, true));
        if closed.is_ok() {
            let _ = released.blocking_recv();
        }
        // A failed ON can still have reached the relay. Always attempt OFF.
        // 释放路径的分段失败不拦截 OFF：按住中的继电器必须始终尝试断开
        //（红线：断开与闭合使用同一物理目标）。
        if supply {
            let _ = self.begin_power_boundary(dut_id, false);
        }
        let opened = self.runtime.block_on(async {
            match backend.power(&target, false).await {
                Ok(()) => Ok(()),
                Err(_) => backend.power(&target, false).await,
            }
        });
        let outcome = opened.and(closed);
        let label = format!("继电器按住测试 CH{channel}");
        if supply {
            self.emit_power_outcome(dut_id, &label, outcome.as_ref().map(|_| ()));
        } else {
            self.emit_action_mark(dut_id, &label, outcome.as_ref().map(|_| ()));
        }
        self.finish_control(dut_id, &admission, operation, outcome.is_ok());
        outcome.map_err(ControlEntryError::from)
    }

    /// 继电器单通道 ON→STATUS→OFF 往返测试（写动作，GUI 按钮路径，Gui 身份）。
    /// 授权走 [`ACTION_CHANNEL_TEST`] 闸门：令牌绑定 dut + 参数 `{"channel": N}`，
    /// 单次消费；绑定/节点校验失败不消费令牌（与 [`Self::execute`] 同序）。
    pub fn channel_test(
        &self,
        dut_id: &str,
        channel: u8,
        approval_token: &str,
    ) -> Result<ChannelTestReport, ControlEntryError> {
        self.channel_test_as(dut_id, channel, approval_token, &WriteIdentity::gui())
    }

    /// 继电器单通道往返测试（显式身份）：MCP relay_test 传 Agent 身份。
    pub fn channel_test_as(
        &self,
        dut_id: &str,
        channel: u8,
        approval_token: &str,
        writer: &WriteIdentity,
    ) -> Result<ChannelTestReport, ControlEntryError> {
        let dut = self
            .dut_configs
            .lock()
            .unwrap()
            .get(dut_id)
            .cloned()
            .ok_or(DeviceError::NotFound)?;
        let node = relay_node(&dut)?;
        let backend = self
            .backend
            .clone()
            .ok_or(ControlEntryError::Device(DeviceError::Unsupported))?;
        let args_json = serde_json::json!({ "channel": channel }).to_string();
        let _agent_action = match (&self.lease, writer) {
            (Some(lease), WriteIdentity::Agent { .. }) => Some(
                lease
                    .begin_agent_action()
                    .map_err(|err| err.to_device_error())?,
            ),
            _ => None,
        };
        let _relay_write = self
            .relay_writes
            .try_read()
            .map_err(|_| DeviceError::Busy)?;
        // 租约复核在 approval 校验之前（不通过不消费令牌，同 execute）
        let admission = self
            .admit_control(&dut.id, writer)
            .map_err(ControlEntryError::from)?;
        let authorized =
            self.approval
                .validate(approval_token, &dut.id, ACTION_CHANNEL_TEST, &args_json);
        if let Err(err) = authorized {
            self.finish_control(&dut.id, &admission, ACTION_CHANNEL_TEST, false);
            return Err(ControlEntryError::Approval(err));
        }
        let outcome = self
            .runtime
            .block_on(async move { backend.channel_test(&node, channel).await });
        // 往返测试（relay_test）同样挂分段标记：ON→STATUS→OFF 的结论进日志
        let label = format!("继电器 relay_test CH{channel}");
        self.emit_action_mark(&dut.id, &label, outcome.as_ref().map(|_| ()));
        self.finish_control(&dut.id, &admission, ACTION_CHANNEL_TEST, outcome.is_ok());
        outcome.map_err(ControlEntryError::from)
    }
}

/// Board-level probe adapter for the relay offline monitor.
///
/// The monitor watches boards, not DUT sessions, so there is no DUT to ask for
/// a channel. The UI tick computes, per board, the first channel bound by any
/// DUT and hands it over with [`Self::set_channels`]. A board that no DUT
/// bound a channel on cannot be probed: there is no channel number to read, so
/// it is `Unsupported` (→ `Unobservable`) rather than a guessed fault.
pub struct ControlRelayProbe {
    control: Arc<ControlService>,
    channels: Mutex<HashMap<String, u8>>,
}

impl ControlRelayProbe {
    pub fn new(control: Arc<ControlService>) -> Self {
        Self {
            control,
            channels: Mutex::new(HashMap::new()),
        }
    }

    /// Replace the board → channel map (idempotent; the monitor sync calls it
    /// on every tick).
    pub fn set_channels(&self, channels: &[(String, u8)]) {
        let mut map = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.clear();
        map.extend(
            channels
                .iter()
                .map(|(board_id, channel)| (board_id.clone(), *channel)),
        );
    }
}

impl RelayBoardProbe for ControlRelayProbe {
    fn probe_board(&self, board: &RelayBoard) -> RelayProbeSample {
        // Copy the channel out and drop the lock before probing: the probe may
        // block for the backend reply timeout and `set_channels` must stay
        // instant while it runs.
        let channel = {
            let map = self
                .channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.get(&board.id).copied()
        };
        let Some(channel) = channel else {
            return RelayProbeSample::Unsupported("板卡未分配继电器通道".into());
        };
        self.control.probe_board_status(&board.node, channel)
    }
}

/// 解析继电器串口节点（逐通道状态/测试动作的目标地址）。
/// PDU 不提供逐通道 STATUS 查询 → Unsupported（不伪造）。
fn relay_node(dut: &DutConfig) -> DeviceResult<String> {
    match &dut.relay {
        RelayConfig::Disabled | RelayConfig::UsbBoard { .. } => {
            Err(DeviceError::Invalid("DUT 未配置可用的控制单元绑定".into()))
        }
        RelayConfig::UsbSerial { node, .. } => {
            if node.trim().is_empty() {
                return Err(DeviceError::Invalid("继电器节点（串口）未配置".into()));
            }
            Ok(node.clone())
        }
        RelayConfig::SnmpPdu { .. } => Err(DeviceError::Unsupported),
    }
}

/// 绑定 + 动作能力校验 + 目标解析。SNMP PDU 只提供电源通断（不伪造 Reset/PowerKey）。
pub fn resolve_target(dut: &DutConfig, action: &ControlAction) -> DeviceResult<ControlTarget> {
    match &dut.relay {
        RelayConfig::Disabled | RelayConfig::UsbBoard { .. } => {
            Err(DeviceError::Invalid("DUT 未配置可用的控制单元绑定".into()))
        }
        RelayConfig::UsbSerial {
            node,
            driver,
            channels,
            ..
        } => {
            if node.trim().is_empty() {
                return Err(DeviceError::Invalid("继电器节点（串口）未配置".into()));
            }
            let channel = match action {
                ControlAction::Power { .. } => channels
                    .power
                    .ok_or_else(|| DeviceError::Invalid("未配置电源通道".into()))?,
                ControlAction::Reset => channels
                    .reset
                    .ok_or_else(|| DeviceError::Invalid("未配置复位通道".into()))?,
                ControlAction::PowerKey { .. } => channels
                    .power_key
                    .ok_or_else(|| DeviceError::Invalid("未配置电源按键通道".into()))?,
            };
            Ok(ControlTarget {
                address: node.clone(),
                driver: if driver.is_empty() {
                    "usb-serial-relay".into()
                } else {
                    driver.clone()
                },
                channel: u16::from(channel),
            })
        }
        RelayConfig::SnmpPdu { snmp, outlets } => match action {
            ControlAction::Power { .. } => {
                if snmp.address.trim().is_empty() {
                    return Err(DeviceError::Invalid("SNMP PDU 地址未配置".into()));
                }
                Ok(ControlTarget {
                    address: snmp.address.clone(),
                    driver: "snmp-pdu".into(),
                    channel: outlets.power,
                })
            }
            // PDU 不伪造 Reset / PowerKey（docs/DATA_MODEL.md）
            ControlAction::Reset | ControlAction::PowerKey { .. } => Err(DeviceError::Unsupported),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use duthub_core::model::{
        FlashConfig, McpConfig, PduOutlets, RelayChannels, SerialConfig, SnmpConfig,
    };
    use std::sync::Mutex as StdMutex;

    /// 记录型假驱动：断言驱动只收到已解析的地址与动作，从不接触令牌。
    struct MockBackend {
        calls: StdMutex<Vec<String>>,
        fail_power: Option<bool>,
        /// channel_status 按地址返回的通道表（未配置的地址报 Unsupported）。
        status_map: StdMutex<HashMap<String, Vec<(u8, bool)>>>,
        /// channel_test 按「地址:通道」返回的报告（未配置的键报 Unsupported）。
        test_reports: StdMutex<HashMap<String, ChannelTestReport>>,
    }

    impl MockBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: StdMutex::new(Vec::new()),
                fail_power: None,
                status_map: StdMutex::new(HashMap::new()),
                test_reports: StdMutex::new(HashMap::new()),
            })
        }

        fn with_status(self: Arc<Self>, address: &str, channels: Vec<(u8, bool)>) -> Arc<Self> {
            self.status_map
                .lock()
                .unwrap()
                .insert(address.to_string(), channels);
            self
        }

        fn with_test_report(
            self: Arc<Self>,
            address: &str,
            channel: u8,
            report: ChannelTestReport,
        ) -> Arc<Self> {
            self.test_reports
                .lock()
                .unwrap()
                .insert(format!("{address}:{channel}"), report);
            self
        }
    }

    #[async_trait]
    impl ControlBackend for MockBackend {
        async fn power(&self, target: &ControlTarget, on: bool) -> DeviceResult<()> {
            self.calls.lock().unwrap().push(format!(
                "power addr={} driver={} ch={} on={on}",
                target.address, target.driver, target.channel
            ));
            if self.fail_power == Some(on) {
                Err(DeviceError::Disconnected)
            } else {
                Ok(())
            }
        }

        async fn reset(&self, target: &ControlTarget) -> DeviceResult<()> {
            self.calls.lock().unwrap().push(format!(
                "reset addr={} driver={} ch={}",
                target.address, target.driver, target.channel
            ));
            Ok(())
        }

        async fn power_key(&self, target: &ControlTarget, long_press: bool) -> DeviceResult<()> {
            self.calls.lock().unwrap().push(format!(
                "power_key addr={} driver={} ch={} long={long_press}",
                target.address, target.driver, target.channel
            ));
            Ok(())
        }

        async fn channel_status(&self, address: &str) -> DeviceResult<Vec<(u8, bool)>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("channel_status addr={address}"));
            self.status_map
                .lock()
                .unwrap()
                .get(address)
                .cloned()
                .ok_or(DeviceError::Unsupported)
        }

        async fn channel_test(
            &self,
            address: &str,
            channel: u8,
        ) -> DeviceResult<ChannelTestReport> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("channel_test addr={address} ch={channel}"));
            self.test_reports
                .lock()
                .unwrap()
                .get(&format!("{address}:{channel}"))
                .cloned()
                .ok_or(DeviceError::Unsupported)
        }
    }

    fn dut_with_relay(id: &str) -> DutConfig {
        DutConfig {
            id: id.into(),
            name: "RK3576".into(),
            serial: SerialConfig {
                host: "local".into(),
                port: "COM3".into(),
                baud: 115_200,
                format: "8N1".into(),
                reference_log: None,
                reference_similarity: None,
                split_logs: false,
            },
            adb: Default::default(),
            relay: RelayConfig::UsbSerial {
                host: "local".into(),
                node: "COM7".into(),
                driver: "CH340".into(),
                channels: RelayChannels {
                    power: Some(4),
                    reset: Some(1),
                    recovery: None,
                    maskrom: None,
                    power_key: Some(2),
                },
            },
            mcp: McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8787/mcp".into(),
            },
            a2a: Default::default(),
            flash: FlashConfig::default(),
            media: Default::default(),
            heartbeat: Default::default(),
        }
    }

    fn pdu_dut(id: &str) -> DutConfig {
        let mut dut = dut_with_relay(id);
        dut.relay = RelayConfig::SnmpPdu {
            snmp: SnmpConfig {
                address: "192.168.1.100".into(),
                version: "3".into(),
                credential_ref: "keyring:pdu-1".into(),
            },
            outlets: PduOutlets { power: 3 },
        };
        dut
    }

    fn service(backend: Option<Arc<dyn ControlBackend>>) -> (ControlService, Arc<ApprovalManager>) {
        let approval = Arc::new(ApprovalManager::new());
        (ControlService::new(backend, approval.clone()), approval)
    }

    #[test]
    fn missing_token_rejected_without_reaching_driver() {
        let backend = MockBackend::new();
        let (svc, _approval) = service(Some(backend.clone()));
        svc.register_dut(dut_with_relay("dut-a"));
        let err = svc
            .execute_by_id("dut-a", ControlAction::Power { on: true }, "")
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Approval(ApprovalError::Missing)
        ));
        assert!(
            backend.calls.lock().unwrap().is_empty(),
            "无令牌不得触达驱动"
        );
    }

    #[test]
    fn manual_control_blocks_agent_relay_without_consuming_approval() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        let manager = Arc::new(ControlLeaseManager::new(None));
        let svc = svc.with_lease_authority(manager.clone());
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        manager.set_manual_control(true).unwrap();
        assert!(matches!(
            svc.execute_by_id_as(
                "dut-a",
                ControlAction::Power { on: true },
                &token.token,
                &WriteIdentity::agent(None, None)
            ),
            Err(ControlEntryError::Device(DeviceError::Lease(_)))
        ));
        assert!(backend.calls.lock().unwrap().is_empty());
        svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap();
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn relay_hold_changes_state_only_on_press_and_release() {
        let backend = MockBackend::new();
        let (svc, _) = service(Some(backend.clone()));
        let manager = Arc::new(ControlLeaseManager::new(None));
        let svc = Arc::new(svc.with_lease_authority(manager.clone()));
        let dut = dut_with_relay("dut-press");
        let (release, released) = tokio::sync::oneshot::channel();
        let worker = std::thread::spawn(move || svc.hold_channel(&dut, 1, released));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while backend.calls.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["power addr=COM7 driver=CH340 ch=1 on=true"]
        );
        assert!(manager.set_manual_control(true).is_err());
        drop(release);
        worker.join().unwrap().unwrap();
        assert_eq!(
            *backend.calls.lock().unwrap(),
            [
                "power addr=COM7 driver=CH340 ch=1 on=true",
                "power addr=COM7 driver=CH340 ch=1 on=false",
            ]
        );
    }

    #[test]
    fn relay_test_does_not_interrupt_serial_input_on_success_or_failure() {
        for failure in [Some(false), Some(true), None] {
            let mut backend = MockBackend::new();
            Arc::get_mut(&mut backend).unwrap().fail_power = failure;
            let (control, _) = service(Some(backend.clone()));
            let lease = Arc::new(ControlLeaseManager::new(None));
            let control = Arc::new(control.with_lease_authority(lease.clone()));
            let dut = dut_with_relay("dut-serial-and-relay");
            control.register_dut(dut.clone());
            let dir = std::env::temp_dir().join(format!(
                "duthub-relay-serial-{}-{failure:?}",
                std::process::id()
            ));
            let device =
                crate::DeviceService::custom(128, dir, 0).with_lease_authority(lease.clone());
            let session = device
                .open_session(&dut.id, &dut.serial.port, 115_200, crate::SessionKind::Demo)
                .unwrap();
            let gui = lease
                .acquire(&dut.id, &duthub_core::lease::LeaseHolder::Gui, None)
                .unwrap();
            let (release, released) = tokio::sync::oneshot::channel();
            let target = dut.clone();
            let worker_control = control.clone();
            let worker =
                std::thread::spawn(move || worker_control.hold_channel(&target, 1, released));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while backend.calls.lock().unwrap().is_empty() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if failure != Some(true) {
                device
                    .write_bytes(&dut.id, WriteIdentity::gui(), b"serial-during-relay\r")
                    .unwrap();
                assert!(matches!(
                    control.execute_by_id(&dut.id, ControlAction::Reset, ""),
                    Err(ControlEntryError::Device(DeviceError::Busy))
                ));
            }
            drop(release);
            assert_eq!(worker.join().unwrap().is_err(), failure.is_some());
            device
                .write_bytes(&dut.id, WriteIdentity::gui(), b"serial-after-relay\r")
                .unwrap_or_else(|err| {
                    panic!("relay outcome {failure:?} blocked keyboard input: {err}")
                });
            let retained = lease
                .acquire(&dut.id, &duthub_core::lease::LeaseHolder::Gui, None)
                .unwrap();
            assert_eq!(
                retained.lease_id, gui.lease_id,
                "relay test changed the serial lease"
            );
            while !device.read_log(&session, 0, 128).iter().any(|event| {
                matches!(&event.event, duthub_core::ipc::UiEvent::SerialChunk { text, .. }
                    if text.contains("Executed (demo): serial-after-relay"))
            }) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "serial actor did not echo input"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn token_mismatch_rejected_without_reaching_driver() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        svc.register_dut(dut_with_relay("dut-a"));
        // 令牌签给 dut-b / power=false，与本次操作不匹配
        let token = approval.issue("dut-b", "control.power", r#"{"on":false}"#);
        let err = svc
            .execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Approval(ApprovalError::Mismatch)
        ));
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn valid_power_reaches_driver_with_resolved_target() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap();
        let calls = backend.calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec!["power addr=COM7 driver=CH340 ch=4 on=true".to_string()]
        );
        // 单次消费：同一令牌重放必须拒绝
        assert!(matches!(
            svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token),
            Err(ControlEntryError::Approval(ApprovalError::Unknown))
        ));
    }

    #[test]
    fn reset_and_power_key_use_their_channels() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        svc.register_dut(dut_with_relay("dut-a"));
        let reset_token = approval.issue("dut-a", "control.reset", "{}");
        svc.execute_by_id("dut-a", ControlAction::Reset, &reset_token.token)
            .unwrap();
        let key_token = approval.issue("dut-a", "control.power_key", r#"{"long_press":true}"#);
        svc.execute_by_id(
            "dut-a",
            ControlAction::PowerKey { long_press: true },
            &key_token.token,
        )
        .unwrap();
        let calls = backend.calls.lock().unwrap().clone();
        assert!(calls.contains(&"reset addr=COM7 driver=CH340 ch=1".to_string()));
        assert!(calls.contains(&"power_key addr=COM7 driver=CH340 ch=2 long=true".to_string()));
    }

    #[test]
    fn unbound_dut_and_missing_channel_fail_before_authorization() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        let mut dut = dut_with_relay("dut-b");
        dut.relay = RelayConfig::Disabled;
        svc.register_dut(dut);
        let token = approval.issue("dut-b", "control.power", r#"{"on":true}"#);
        let err = svc
            .execute_by_id("dut-b", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Device(DeviceError::Invalid(_))
        ));
        // 绑定缺失时令牌不被消费，修正配置后可重试
        assert!(
            approval
                .validate(&token.token, "dut-b", "control.power", r#"{"on":true}"#)
                .is_ok()
        );

        let mut dut = dut_with_relay("dut-c");
        if let RelayConfig::UsbSerial { channels, .. } = &mut dut.relay {
            channels.reset = None;
        }
        svc.register_dut(dut);
        let token = approval.issue("dut-c", "control.reset", "{}");
        assert!(matches!(
            svc.execute_by_id("dut-c", ControlAction::Reset, &token.token),
            Err(ControlEntryError::Device(DeviceError::Invalid(_)))
        ));
    }

    #[test]
    fn pdu_allows_power_only() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        svc.register_dut(pdu_dut("dut-pdu"));
        let token = approval.issue("dut-pdu", "control.power", r#"{"on":false}"#);
        svc.execute_by_id("dut-pdu", ControlAction::Power { on: false }, &token.token)
            .unwrap();
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["power addr=192.168.1.100 driver=snmp-pdu ch=3 on=false".to_string()]
        );
        // PDU 不伪造 Reset / PowerKey
        let token = approval.issue("dut-pdu", "control.reset", "{}");
        assert!(matches!(
            svc.execute_by_id("dut-pdu", ControlAction::Reset, &token.token),
            Err(ControlEntryError::Device(DeviceError::Unsupported))
        ));
        assert!(
            backend
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|c| c.starts_with("power"))
        );
    }

    #[test]
    fn missing_backend_reports_unsupported_without_consuming_token() {
        let (svc, approval) = service(None);
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        let err = svc
            .execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Device(DeviceError::Unsupported)
        ));
        // 后端未接线不消费令牌
        assert!(
            approval
                .validate(&token.token, "dut-a", "control.power", r#"{"on":true}"#)
                .is_ok()
        );
    }

    // ── 继电器测试（channel_status 只读 / channel_test 走 control.test 闸门） ────

    fn roundtrip_report() -> ChannelTestReport {
        ChannelTestReport {
            before: Some(false),
            on_ok: true,
            off_ok: true,
            after: Some(false),
        }
    }

    #[test]
    fn channel_status_resolves_node_without_approval() {
        let backend = MockBackend::new().with_status("COM7", vec![(1, false), (4, true)]);
        let (svc, _approval) = service(Some(backend.clone()));
        svc.register_dut(dut_with_relay("dut-a"));
        let states = svc.channel_status("dut-a").unwrap();
        assert_eq!(states, vec![(1, false), (4, true)]);
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["channel_status addr=COM7".to_string()]
        );
    }

    #[test]
    fn channel_status_rejects_unbound_pdu_and_unknown_dut() {
        let backend = MockBackend::new();
        let (svc, _approval) = service(Some(backend.clone()));
        svc.register_dut(dut_with_relay("dut-a"));
        let mut disabled = dut_with_relay("dut-off");
        disabled.relay = RelayConfig::Disabled;
        svc.register_dut(disabled);
        svc.register_dut(pdu_dut("dut-pdu"));

        assert!(matches!(
            svc.channel_status("dut-off"),
            Err(ControlEntryError::Device(DeviceError::Invalid(_)))
        ));
        assert!(matches!(
            svc.channel_status("dut-pdu"),
            Err(ControlEntryError::Device(DeviceError::Unsupported))
        ));
        assert!(matches!(
            svc.channel_status("no-such-dut"),
            Err(ControlEntryError::Device(DeviceError::NotFound))
        ));
        assert!(
            backend.calls.lock().unwrap().is_empty(),
            "拒绝路径不得触达后端"
        );
    }

    #[test]
    fn channel_test_requires_control_test_token_bound_to_channel() {
        let backend = MockBackend::new().with_test_report("COM7", 1, roundtrip_report());
        let (svc, approval) = service(Some(backend.clone()));
        svc.register_dut(dut_with_relay("dut-a"));

        // 无令牌 → Missing，不触达后端
        assert!(matches!(
            svc.channel_test("dut-a", 1, ""),
            Err(ControlEntryError::Approval(ApprovalError::Missing))
        ));
        // 令牌签给别的动作/参数 → Mismatch
        let wrong = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        assert!(matches!(
            svc.channel_test("dut-a", 1, &wrong.token),
            Err(ControlEntryError::Approval(ApprovalError::Mismatch))
        ));
        // 正确令牌（dut + control.test + {"channel":1}）→ 放行
        let token = approval.issue("dut-a", ACTION_CHANNEL_TEST, r#"{"channel":1}"#);
        let report = svc.channel_test("dut-a", 1, &token.token).unwrap();
        assert_eq!(report, roundtrip_report());
        // 单次消费：重放拒绝且不再触达后端
        assert!(matches!(
            svc.channel_test("dut-a", 1, &token.token),
            Err(ControlEntryError::Approval(ApprovalError::Unknown))
        ));
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["channel_test addr=COM7 ch=1".to_string()]
        );
    }

    #[test]
    fn channel_test_channel_mismatch_rejected() {
        let backend = MockBackend::new().with_test_report("COM7", 2, roundtrip_report());
        let (svc, approval) = service(Some(backend));
        svc.register_dut(dut_with_relay("dut-a"));
        // 令牌绑定 channel=1，请求 channel=2 → 参数绑定校验拒绝
        let token = approval.issue("dut-a", ACTION_CHANNEL_TEST, r#"{"channel":1}"#);
        assert!(matches!(
            svc.channel_test("dut-a", 2, &token.token),
            Err(ControlEntryError::Approval(ApprovalError::Mismatch))
        ));
    }

    #[test]
    fn channel_test_unbound_dut_does_not_consume_token() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend));
        let mut dut = dut_with_relay("dut-b");
        dut.relay = RelayConfig::Disabled;
        svc.register_dut(dut);
        let token = approval.issue("dut-b", ACTION_CHANNEL_TEST, r#"{"channel":1}"#);
        assert!(matches!(
            svc.channel_test("dut-b", 1, &token.token),
            Err(ControlEntryError::Device(DeviceError::Invalid(_)))
        ));
        // 绑定缺失：令牌不消费，修正配置后可重试
        assert!(
            approval
                .validate(
                    &token.token,
                    "dut-b",
                    ACTION_CHANNEL_TEST,
                    r#"{"channel":1}"#
                )
                .is_ok()
        );
    }

    // ── 分段标记挂钩（日志分割 B2：动作成功/失败后自动 [mark] 行） ─────────────

    /// 全路径失败的假驱动（失败标记测试用）。
    struct FailingBackend;

    #[async_trait]
    impl ControlBackend for FailingBackend {
        async fn power(&self, _target: &ControlTarget, _on: bool) -> DeviceResult<()> {
            Err(DeviceError::Invalid("继电器无应答".into()))
        }
        async fn reset(&self, _target: &ControlTarget) -> DeviceResult<()> {
            Err(DeviceError::Disconnected)
        }
        async fn power_key(&self, _target: &ControlTarget, _long_press: bool) -> DeviceResult<()> {
            Err(DeviceError::Disconnected)
        }
        async fn channel_status(&self, _address: &str) -> DeviceResult<Vec<(u8, bool)>> {
            Err(DeviceError::Disconnected)
        }
        async fn channel_test(
            &self,
            _address: &str,
            _channel: u8,
        ) -> DeviceResult<ChannelTestReport> {
            Err(DeviceError::Disconnected)
        }
    }

    type MarkRecorder = Arc<StdMutex<Vec<(String, String)>>>;

    fn attach_sink(svc: &ControlService) -> MarkRecorder {
        let marks: MarkRecorder = Arc::new(StdMutex::new(Vec::new()));
        let sink = marks.clone();
        assert!(
            svc.set_log_sink(Arc::new(move |dut, label| sink
                .lock()
                .unwrap()
                .push((dut.to_string(), label.to_string())))),
            "首次挂载必须成功"
        );
        marks
    }

    #[test]
    fn successful_power_emits_mark_and_second_sink_is_ignored() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        let marks = attach_sink(&svc);
        assert!(
            !svc.set_log_sink(Arc::new(|_, _| ())),
            "重复挂载应被忽略（OnceLock 语义）"
        );
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap();
        assert_eq!(
            *marks.lock().unwrap(),
            vec![("dut-a".to_string(), "继电器 power ON".to_string())],
            "成功动作必须写 [mark] 标记"
        );
        // OFF 用不同标记文本
        let token = approval.issue("dut-a", "control.power", r#"{"on":false}"#);
        svc.execute_by_id("dut-a", ControlAction::Power { on: false }, &token.token)
            .unwrap();
        let marks = marks.lock().unwrap();
        assert_eq!(marks[1].1, "继电器 power OFF");
    }

    #[test]
    fn failed_action_emits_mark_with_reason() {
        let (svc, approval) = service(Some(Arc::new(FailingBackend)));
        let marks = attach_sink(&svc);
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        assert!(
            svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
                .is_err()
        );
        let marks = marks.lock().unwrap();
        assert_eq!(marks.len(), 1, "失败动作也要写标记");
        assert!(
            marks[0].1.starts_with("继电器 power ON 失败: "),
            "失败标记必须带原因：{}",
            marks[0].1
        );
    }

    #[test]
    fn approval_rejection_does_not_emit_mark() {
        let backend = MockBackend::new();
        let (svc, _approval) = service(Some(backend.clone()));
        let marks = attach_sink(&svc);
        svc.register_dut(dut_with_relay("dut-a"));
        // 无令牌：授权闸门拒绝，未触达驱动 → 不算动作，无标记
        assert!(
            svc.execute_by_id("dut-a", ControlAction::Reset, "")
                .is_err()
        );
        assert!(marks.lock().unwrap().is_empty(), "授权拒绝不产生分段标记");
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn reset_power_key_and_relay_test_emit_their_marks() {
        let backend = MockBackend::new().with_test_report("COM7", 1, roundtrip_report());
        let (svc, approval) = service(Some(backend));
        let marks = attach_sink(&svc);
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.reset", "{}");
        svc.execute_by_id("dut-a", ControlAction::Reset, &token.token)
            .unwrap();
        let token = approval.issue("dut-a", "control.power_key", r#"{"long_press":true}"#);
        svc.execute_by_id(
            "dut-a",
            ControlAction::PowerKey { long_press: true },
            &token.token,
        )
        .unwrap();
        let token = approval.issue("dut-a", ACTION_CHANNEL_TEST, r#"{"channel":1}"#);
        svc.channel_test("dut-a", 1, &token.token).unwrap();
        let labels: Vec<String> = marks
            .lock()
            .unwrap()
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        assert_eq!(
            labels,
            vec![
                "继电器 reset".to_string(),
                "继电器 power key 长按".to_string(),
                "继电器 relay_test CH1".to_string(),
            ]
        );
    }

    // ── LOG-5：供电前分段失败拦截派发 ──────────────────────────────────────

    /// 供电前分段未落盘 → execute 不得派发（不动设备）；拦截只作用于 Power，
    /// 非电源动作不经供电边界、正常派发。
    #[test]
    fn failed_power_boundary_intercepts_dispatch() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        assert!(
            svc.set_power_boundary_sink(Arc::new(|_, _| {
                Err(DeviceError::Invalid("会话日志分段数已达上限".into()))
            })),
            "首次挂载必须成功"
        );
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        let err = svc
            .execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(
            matches!(err, ControlEntryError::Device(DeviceError::Invalid(_))),
            "分段失败必须以 Device 错误上报：{err:?}"
        );
        assert!(
            backend.calls.lock().unwrap().is_empty(),
            "分段失败不得触达驱动（不动设备）"
        );
        // 非电源动作不经供电边界：分段失败不影响其派发
        let token = approval.issue("dut-a", "control.reset", "{}");
        svc.execute_by_id("dut-a", ControlAction::Reset, &token.token)
            .unwrap();
        assert_eq!(backend.calls.lock().unwrap().len(), 1, "Reset 必须正常派发");
    }

    /// 按住测试的分段语义：闭合分段失败 → 不派发 ON（未动设备，无 OFF 补偿）；
    /// 释放分段失败不拦截 OFF——按住中的继电器必须始终尝试断开（红线：断开
    /// 与闭合使用同一物理目标）。
    #[test]
    fn hold_channel_boundary_failure_intercepts_on_but_never_off() {
        // 场景一：闭合分段失败 → Err 且驱动零调用
        let backend = MockBackend::new();
        let (svc, _approval) = service(Some(backend.clone()));
        assert!(svc.set_power_boundary_sink(Arc::new(|_, _| {
            Err(DeviceError::Invalid("闭合分段失败".into()))
        })));
        let dut = dut_with_relay("dut-hold-on");
        let (_release, released) = tokio::sync::oneshot::channel();
        let err = svc.hold_channel(&dut, 4, released).unwrap_err();
        assert!(
            matches!(err, ControlEntryError::Device(DeviceError::Invalid(_))),
            "闭合分段失败必须上报 Device 错误：{err:?}"
        );
        assert!(
            backend.calls.lock().unwrap().is_empty(),
            "闭合分段失败不得触达驱动"
        );

        // 场景二：仅释放分段失败 → ON 与 OFF 都必须派发
        let backend = MockBackend::new();
        let (svc, _approval) = service(Some(backend.clone()));
        assert!(svc.set_power_boundary_sink(Arc::new(|_, label| {
            if label == "Power OFF" {
                Err(DeviceError::Invalid("释放分段失败".into()))
            } else {
                Ok(())
            }
        })));
        let svc = Arc::new(svc);
        let dut = dut_with_relay("dut-hold-off");
        let (release, released) = tokio::sync::oneshot::channel();
        let worker = std::thread::spawn(move || svc.hold_channel(&dut, 4, released));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while backend.calls.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "闭合未派发");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["power addr=COM7 driver=CH340 ch=4 on=true"]
        );
        drop(release);
        worker.join().unwrap().unwrap();
        assert_eq!(
            *backend.calls.lock().unwrap(),
            [
                "power addr=COM7 driver=CH340 ch=4 on=true",
                "power addr=COM7 driver=CH340 ch=4 on=false",
            ]
        );
    }
}
