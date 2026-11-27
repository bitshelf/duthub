//! Shared device controls used by both the GUI and MCP.
//!
//! The control service does the following.
//! Check and consume an approval matched to the DUT, action, and parameters.
//! Require a configured relay or SNMP PDU.
//! Check each action against the configured channels and backend capabilities.
//! Resolve the binding into a physical `ControlTarget`.
//!
//! The backend receives only resolved targets and actions.
//! The control service owns approval validation.

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

/// Control actions (names and bound argument JSON match MCP approval tokens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlAction {
    Power {
        on: bool,
    },
    PowerCycle {
        off_ms: u64,
    },
    Reset,
    PowerKey {
        long_press: bool,
    },
    /// Hold the `maskrom` relay channel while pulsing reset (or cycling power),
    /// then unconditionally release `maskrom` back to OFF.
    EnterMaskrom {
        hold_ms: u64,
    },
    /// Hold the `recovery` relay channel while pulsing reset (or cycling power),
    /// then unconditionally release `recovery` back to OFF.
    EnterLoader {
        hold_ms: u64,
    },
}

/// Default hold window (ms) for boot-mode strap sequences.
pub const DEFAULT_BOOT_HOLD_MS: u64 = 1_500;
/// Default power-off window (ms) for a power cycle.
pub const DEFAULT_POWER_CYCLE_OFF_MS: u64 = 1_500;
const MIN_BOOT_HOLD_MS: u64 = 200;
const MAX_BOOT_HOLD_MS: u64 = 10_000;
const MIN_POWER_CYCLE_OFF_MS: u64 = 200;
const MAX_POWER_CYCLE_OFF_MS: u64 = 10_000;

/// Receives a log segment marker after a relay action.
/// The host forwards the DUT ID and label to the serial log.
/// Discard the marker when no serial session is open.
/// An annotation failure does not change the backend's result.
pub type LogMarkSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Receives the pre-power segment marker and reports whether it reached disk.
/// A failed marker prevents hardware dispatch.
/// The host implements the synchronous boundary through the device service.
/// The original caller receives the error.
pub type PowerBoundarySink = Arc<dyn Fn(&str, &str) -> DeviceResult<()> + Send + Sync>;

/// Approval action for testing one relay channel with an ON, status, and OFF round trip.
pub const ACTION_CHANNEL_TEST: &str = "control.test";

impl ControlAction {
    /// Approval action name from the API contract.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            ControlAction::Power { .. } => "control.power",
            ControlAction::PowerCycle { .. } => "control.power_cycle",
            ControlAction::Reset => "control.reset",
            ControlAction::PowerKey { .. } => "control.power_key",
            ControlAction::EnterMaskrom { .. } => "control.enter_maskrom",
            ControlAction::EnterLoader { .. } => "control.enter_loader",
        }
    }

    /// Label written in the action's segment marker.
    #[must_use]
    pub fn mark_label(&self) -> &'static str {
        match self {
            ControlAction::Power { on: true } => "继电器 power ON",
            ControlAction::Power { on: false } => "继电器 power OFF",
            ControlAction::PowerCycle { .. } => "继电器 power cycle",
            ControlAction::Reset => "继电器 reset",
            ControlAction::PowerKey { long_press: true } => "继电器 power key 长按",
            ControlAction::PowerKey { long_press: false } => "继电器 power key 短按",
            ControlAction::EnterMaskrom { .. } => "继电器 enter MaskROM",
            ControlAction::EnterLoader { .. } => "继电器 enter Loader",
        }
    }

    /// Issuing and consuming approval must serialize the parameters identically.
    #[must_use]
    pub fn args_json(&self) -> String {
        match self {
            ControlAction::Power { on } => serde_json::json!({ "on": on }).to_string(),
            ControlAction::PowerCycle { off_ms } => {
                serde_json::json!({ "offMs": off_ms }).to_string()
            }
            ControlAction::Reset => serde_json::json!({}).to_string(),
            ControlAction::PowerKey { long_press } => {
                serde_json::json!({ "long_press": long_press }).to_string()
            }
            ControlAction::EnterMaskrom { hold_ms } | ControlAction::EnterLoader { hold_ms } => {
                serde_json::json!({ "holdMs": hold_ms }).to_string()
            }
        }
    }
}

/// Authorization and execution errors for the shared control entry point.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlEntryError {
    #[error("{0}")]
    Approval(#[from] ApprovalError),
    #[error("{0}")]
    Device(#[from] DeviceError),
}

/// Shared device-control entry point.
/// Registered DUT configurations keep address resolution independent of the UI.
pub struct ControlService {
    backend: Option<Arc<dyn ControlBackend>>,
    approval: Arc<ApprovalManager>,
    dut_configs: Mutex<HashMap<String, DutConfig>>,
    /// Sink for segment markers after control actions.
    /// The host attaches the sink once and all actions share it.
    log_sink: OnceLock<LogMarkSink>,
    power_boundary: OnceLock<PowerBoundarySink>,
    power_outcome: OnceLock<LogMarkSink>,
    runtime: crate::RuntimeOwner,
    relay_writes: std::sync::RwLock<()>,
    /// Check control leases before consuming approval.
    /// Share the lease manager with the serial write path.
    lease: Option<Arc<ControlLeaseManager>>,
    /// Record successful and denied control requests.
    audit: Option<Arc<AuditSink>>,
}

impl std::fmt::Debug for ControlService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlService")
            .field("has_backend", &self.backend.is_some())
            .finish_non_exhaustive()
    }
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

    /// Set the lease manager. The GUI calls `Self::execute_by_id`
    /// and MCP supplies an Agent identity to `Self::execute_by_id_as`.
    #[must_use]
    pub fn with_lease_authority(mut self, lease: Arc<ControlLeaseManager>) -> Self {
        self.lease = Some(lease);
        self
    }

    /// Set the sink for control execution and denials.
    #[must_use]
    pub fn with_audit_sink(mut self, audit: Arc<AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Acquire a temporary GUI lease when needed and release it after the operation.
    /// Agents must supply their lease. Audit denied requests.
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

    /// Release any temporary lease and audit the action outcome.
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

    /// Attach the segment marker sink once. A repeated call returns false.
    pub fn set_log_sink(&self, sink: LogMarkSink) -> bool {
        self.log_sink.set(sink).is_ok()
    }

    pub fn set_power_boundary_sink(&self, sink: PowerBoundarySink) -> bool {
        self.power_boundary.set(sink).is_ok()
    }

    pub fn set_power_outcome_sink(&self, sink: LogMarkSink) -> bool {
        self.power_outcome.set(sink).is_ok()
    }

    /// Persist the pre-power marker through the host callback.
    /// Dispatch only after the marker is committed.
    fn begin_power_boundary(&self, dut_id: &str, on: bool) -> DeviceResult<()> {
        match self.power_boundary.get() {
            Some(sink) => sink(dut_id, if on { "Power ON" } else { "Power OFF" }),
            None => Ok(()),
        }
    }

    /// Record completed and failed actions, including the failure reason when present.
    fn emit_action_mark<T>(&self, dut_id: &str, label: &str, outcome: Result<&T, &DeviceError>) {
        Self::emit_control_mark(self.log_sink.get(), dut_id, label, outcome);
    }

    fn emit_power_outcome<T>(&self, dut_id: &str, label: &str, outcome: Result<&T, &DeviceError>) {
        Self::emit_control_mark(
            self.power_outcome.get().or_else(|| self.log_sink.get()),
            dut_id,
            label,
            outcome,
        );
    }

    fn emit_control_mark<T>(
        sink: Option<&LogMarkSink>,
        dut_id: &str,
        label: &str,
        outcome: Result<&T, &DeviceError>,
    ) {
        if let Some(sink) = sink {
            let text = match outcome {
                Ok(_) => label.to_string(),
                Err(err) => format!("{label} 失败: {err}"),
            };
            sink(dut_id, &text);
        }
    }

    /// Register or update the configuration used for binding and resolution.
    pub fn register_dut(&self, dut: DutConfig) {
        duthub_core::util::lock_unpoisoned(&self.dut_configs).insert(dut.id.clone(), dut);
    }

    pub fn unregister_dut(&self, dut_id: &str) {
        duthub_core::util::lock_unpoisoned(&self.dut_configs).remove(dut_id);
    }

    /// Execute a GUI control action for a DUT ID.
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

    pub(crate) fn execute_confirmed_reset(&self, dut_id: &str) -> Result<(), ControlEntryError> {
        let action = ControlAction::Reset;
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

    pub fn reset_configured(&self, dut_id: &str) -> bool {
        self.backend.is_some()
            && self
                .dut_configs
                .lock()
                .unwrap()
                .get(dut_id)
                .is_some_and(|dut| resolve_target(dut, &ControlAction::Reset).is_ok())
    }

    /// True when the DUT has either Power Supply or Reset control configured.
    pub fn reboot_configured(&self, dut_id: &str) -> bool {
        self.power_configured(dut_id) || self.reset_configured(dut_id)
    }

    /// Execute with an explicit identity. MCP supplies an Agent identity.
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

    /// Resolve binding and capability, verify the backend and lease, then authorize and dispatch.
    ///
    /// A lease denial leaves the approval token available for retry.
    /// An Agent can retry after a GUI holder releases control.
    pub fn execute(
        &self,
        dut: &DutConfig,
        action: ControlAction,
        approval_token: &str,
        writer: &WriteIdentity,
    ) -> Result<(), ControlEntryError> {
        // Resolve the physical target and capability before touching the token.
        let target = resolve_target(dut, &action)?;
        // Check backend readiness before touching the token.
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
        // Check the lease before validating approval.
        let admission = self
            .admit_control(&dut.id, writer)
            .map_err(ControlEntryError::from)?;
        // Consume the token once after matching the DUT, action, and parameters.
        let authorized =
            self.approval
                .validate(approval_token, &dut.id, action.name(), &action.args_json());
        if let Err(err) = authorized {
            // Release the temporary lease if approval validation fails.
            self.finish_control(&dut.id, &admission, action.name(), false);
            return Err(ControlEntryError::Approval(err));
        }
        // Dispatch only the resolved target and action.
        // Record both outcomes with segment markers.
        let label = action.mark_label();
        let action_name = action.name();
        let is_power = matches!(
            action,
            ControlAction::Power { .. } | ControlAction::PowerCycle { .. }
        );
        if let ControlAction::Power { on } = action {
            // If pre-power logging fails, block dispatch, release the lease, and audit the failure.
            // Approval was already consumed, so a retry requires a fresh token.
            if let Err(err) = self.begin_power_boundary(&dut.id, on) {
                self.finish_control(&dut.id, &admission, action_name, false);
                return Err(ControlEntryError::Device(err));
            }
        } else if matches!(action, ControlAction::PowerCycle { .. })
            && let Err(err) = self.begin_power_boundary(&dut.id, false)
        {
            self.finish_control(&dut.id, &admission, action_name, false);
            return Err(ControlEntryError::Device(err));
        }
        let boot_trigger = match action {
            ControlAction::EnterMaskrom { .. } | ControlAction::EnterLoader { .. } => {
                Some(resolve_boot_trigger(dut)?)
            }
            _ => None,
        };
        let outcome = self.runtime.block_on(async move {
            match action {
                ControlAction::Power { on } => backend.power(&target, on).await,
                ControlAction::PowerCycle { off_ms } => {
                    let wait = off_ms.clamp(MIN_POWER_CYCLE_OFF_MS, MAX_POWER_CYCLE_OFF_MS);
                    backend.power(&target, false).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    backend.power(&target, true).await
                }
                ControlAction::Reset => backend.reset(&target).await,
                ControlAction::PowerKey { long_press } => {
                    backend.power_key(&target, long_press).await
                }
                ControlAction::EnterMaskrom { hold_ms }
                | ControlAction::EnterLoader { hold_ms } => {
                    let trigger = boot_trigger.expect("boot trigger pre-resolved");
                    let hold = hold_ms.clamp(MIN_BOOT_HOLD_MS, MAX_BOOT_HOLD_MS);
                    // Energize the boot-strap channel first; if ON fails, still
                    // attempt OFF defensively in case the frame reached the board.
                    let strap_on = backend.power(&target, true).await;
                    let triggered = if strap_on.is_ok() {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        let trig = match trigger {
                            BootTrigger::Reset(reset_target) => backend.reset(&reset_target).await,
                            BootTrigger::PowerCycle(power_target) => {
                                match backend.power(&power_target, false).await {
                                    Ok(()) => {
                                        tokio::time::sleep(std::time::Duration::from_millis(1_000))
                                            .await;
                                        backend.power(&power_target, true).await
                                    }
                                    Err(err) => Err(err),
                                }
                            }
                        };
                        if trig.is_ok() {
                            tokio::time::sleep(std::time::Duration::from_millis(hold)).await;
                        }
                        trig
                    } else {
                        Ok(())
                    };
                    // Unconditionally release the boot-strap channel even when
                    // the reset or power-cycle step fails so the board is never
                    // left latched in MaskROM/Recovery.
                    let strap_off = match backend.power(&target, false).await {
                        Ok(()) => Ok(()),
                        Err(_) => backend.power(&target, false).await,
                    };
                    strap_off.and(strap_on).and(triggered)
                }
            }
        });
        if is_power {
            self.emit_power_outcome(&dut.id, label, outcome.as_ref());
        } else {
            self.emit_action_mark(&dut.id, label, outcome.as_ref());
        }
        self.finish_control(&dut.id, &admission, action_name, outcome.is_ok());
        outcome.map_err(ControlEntryError::from)
    }

    /// Query relay channel status without approval.
    /// PDU returns unsupported. A silent relay board yields an empty list.
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
            // A failed activation marker blocks ON, so no compensating OFF is required.
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
        // Always attempt OFF for a held relay even if its release marker fails.
        // Use the same physical target for activation and release.
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
            self.emit_power_outcome(dut_id, &label, outcome.as_ref());
        } else {
            self.emit_action_mark(dut_id, &label, outcome.as_ref());
        }
        self.finish_control(dut_id, &admission, operation, outcome.is_ok());
        outcome.map_err(ControlEntryError::from)
    }

    /// Energize, check, and release one channel from the GUI.
    /// The single-use approval is bound to the DUT and channel.
    /// Resolve the binding before consuming approval, as in `Self::execute`.
    pub fn channel_test(
        &self,
        dut_id: &str,
        channel: u8,
        approval_token: &str,
    ) -> Result<ChannelTestReport, ControlEntryError> {
        self.channel_test_as(dut_id, channel, approval_token, &WriteIdentity::gui())
    }

    /// Run a relay test with the caller's explicit identity.
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
        // Verify the lease before consuming approval.
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
        // Record the result of the ON, status, and OFF sequence as a segment marker.
        let label = format!("继电器 relay_test CH{channel}");
        self.emit_action_mark(&dut.id, &label, outcome.as_ref());
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

impl std::fmt::Debug for ControlRelayProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlRelayProbe").finish_non_exhaustive()
    }
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

/// Resolve the relay port for status queries and test commands.
/// An SNMP PDU has no equivalent per-channel status operation.
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

/// Trigger used to reboot the board while a boot-strap pin (`maskrom` / `recovery`)
/// is held active.
enum BootTrigger {
    Reset(ControlTarget),
    PowerCycle(ControlTarget),
}

fn configured_channel_names(channels: &duthub_core::model::RelayChannels) -> String {
    let mut names = Vec::new();
    if let Some(ch) = channels.power {
        names.push(format!("power=CH{ch}"));
    }
    if let Some(ch) = channels.reset {
        names.push(format!("reset=CH{ch}"));
    }
    if let Some(ch) = channels.recovery {
        names.push(format!("recovery=CH{ch}"));
    }
    if let Some(ch) = channels.maskrom {
        names.push(format!("maskrom=CH{ch}"));
    }
    if let Some(ch) = channels.power_key {
        names.push(format!("powerKey=CH{ch}"));
    }
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

fn resolve_boot_trigger(dut: &DutConfig) -> DeviceResult<BootTrigger> {
    let RelayConfig::UsbSerial {
        node,
        driver,
        channels,
        ..
    } = &dut.relay
    else {
        return Err(DeviceError::Unsupported);
    };
    let resolved_driver = if driver.is_empty() {
        "usb-serial-relay".to_owned()
    } else {
        driver.clone()
    };
    if let Some(ch) = channels.reset {
        return Ok(BootTrigger::Reset(ControlTarget {
            address: node.clone(),
            driver: resolved_driver,
            channel: u16::from(ch),
        }));
    }
    if let Some(ch) = channels.power {
        return Ok(BootTrigger::PowerCycle(ControlTarget {
            address: node.clone(),
            driver: resolved_driver,
            channel: u16::from(ch),
        }));
    }
    Err(DeviceError::Invalid(format!(
        "Neither reset nor power relay channel is configured for boot-mode entry (configured: {})",
        configured_channel_names(channels)
    )))
}

/// Validate the binding and capability, then resolve the physical target.
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
                ControlAction::Power { .. } | ControlAction::PowerCycle { .. } => channels
                    .power
                    .ok_or_else(|| DeviceError::Invalid("未配置电源通道".into()))?,
                ControlAction::Reset => channels
                    .reset
                    .ok_or_else(|| DeviceError::Invalid("未配置复位通道".into()))?,
                ControlAction::PowerKey { .. } => channels
                    .power_key
                    .ok_or_else(|| DeviceError::Invalid("未配置电源按键通道".into()))?,
                ControlAction::EnterMaskrom { .. } => {
                    let _ = resolve_boot_trigger(dut)?;
                    channels.maskrom.ok_or_else(|| {
                        DeviceError::Invalid(format!(
                            "MaskROM relay channel is not configured on this DUT (configured: {})",
                            configured_channel_names(channels)
                        ))
                    })?
                }
                ControlAction::EnterLoader { .. } => {
                    let _ = resolve_boot_trigger(dut)?;
                    channels.recovery.ok_or_else(|| {
                        DeviceError::Invalid(format!(
                            "Recovery/Loader relay channel is not configured on this DUT (configured: {})",
                            configured_channel_names(channels)
                        ))
                    })?
                }
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
            ControlAction::Power { .. } | ControlAction::PowerCycle { .. } => {
                if snmp.address.trim().is_empty() {
                    return Err(DeviceError::Invalid("SNMP PDU 地址未配置".into()));
                }
                Ok(ControlTarget {
                    address: snmp.address.clone(),
                    driver: "snmp-pdu".into(),
                    channel: outlets.power,
                })
            }
            // The SNMP PDU supports power switching only.
            ControlAction::Reset
            | ControlAction::PowerKey { .. }
            | ControlAction::EnterMaskrom { .. }
            | ControlAction::EnterLoader { .. } => Err(DeviceError::Unsupported),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use duthub_core::HeartbeatConfig;
    use duthub_core::model::{
        AdbConfig, DutA2aConfig, DutMediaConfig, FlashConfig, McpConfig, PduOutlets, RelayChannels,
        SerialConfig, SnmpConfig,
    };
    use std::sync::Mutex as StdMutex;

    /// Recording backend that receives the resolved address and action without the token.
    struct MockBackend {
        calls: StdMutex<Vec<String>>,
        fail_power: Option<bool>,
        /// Configured channel states by address. Unknown addresses return unsupported.
        status_map: StdMutex<HashMap<String, Vec<(u8, bool)>>>,
        /// Configured test reports by address and channel. Unknown targets return unsupported.
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
            adb: AdbConfig::default(),
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
            a2a: DutA2aConfig::default(),
            flash: FlashConfig::default(),
            media: DutMediaConfig::default(),
            heartbeat: HeartbeatConfig::default(),
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
            "missing token must not reach the driver"
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
        // The token authorizes a different DUT and power state.
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
        // Reject repeated use of the same token.
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
        // A missing binding leaves the token available after configuration is corrected.
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
        // The PDU provides neither reset nor power-key operations.
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
        // A missing backend leaves approval unconsumed.
        assert!(
            approval
                .validate(&token.token, "dut-a", "control.power", r#"{"on":true}"#)
                .is_ok()
        );
    }

    // Read-only relay status and approval-gated relay testing.

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
            "rejected calls must not reach the backend"
        );
    }

    #[test]
    fn channel_test_requires_control_test_token_bound_to_channel() {
        let backend = MockBackend::new().with_test_report("COM7", 1, roundtrip_report());
        let (svc, approval) = service(Some(backend.clone()));
        svc.register_dut(dut_with_relay("dut-a"));

        // A missing token must be rejected before dispatch.
        assert!(matches!(
            svc.channel_test("dut-a", 1, ""),
            Err(ControlEntryError::Approval(ApprovalError::Missing))
        ));
        // A token for other arguments or actions must be rejected.
        let wrong = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        assert!(matches!(
            svc.channel_test("dut-a", 1, &wrong.token),
            Err(ControlEntryError::Approval(ApprovalError::Mismatch))
        ));
        // A matching DUT, action, and channel permit dispatch.
        let token = approval.issue("dut-a", ACTION_CHANNEL_TEST, r#"{"channel":1}"#);
        let report = svc.channel_test("dut-a", 1, &token.token).unwrap();
        assert_eq!(report, roundtrip_report());
        // A reused token must not reach the backend.
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
        // Approval for channel 1 does not authorize channel 2.
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
        // Preserve the token when the binding is missing.
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

    // Log markers for completed and failed relay actions.

    /// Backend that fails every action to exercise failure markers.
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
            "initial sink registration must succeed"
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
            "duplicate registration must be ignored (OnceLock semantics)"
        );
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap();
        assert_eq!(
            *marks.lock().unwrap(),
            vec![("dut-a".to_string(), "继电器 power ON".to_string())],
            "successful action must emit a [mark] entry"
        );
        // OFF has a distinct marker label.
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
        assert_eq!(marks.len(), 1, "failed action must also emit a mark");
        assert!(
            marks[0].1.starts_with("继电器 power ON 失败: "),
            "failure mark must include reason: {}",
            marks[0].1
        );
    }

    #[test]
    fn approval_rejection_does_not_emit_mark() {
        let backend = MockBackend::new();
        let (svc, _approval) = service(Some(backend.clone()));
        let marks = attach_sink(&svc);
        svc.register_dut(dut_with_relay("dut-a"));
        // A missing token never reaches the driver or writes an execution marker.
        assert!(
            svc.execute_by_id("dut-a", ControlAction::Reset, "")
                .is_err()
        );
        assert!(
            marks.lock().unwrap().is_empty(),
            "approval denial must not emit a mark"
        );
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

    // Pre-power logging errors must block hardware dispatch.

    /// Power switching requires a committed pre-power marker.
    /// Other actions bypass that boundary.
    #[test]
    fn failed_power_boundary_intercepts_dispatch() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        assert!(
            svc.set_power_boundary_sink(Arc::new(|_, _| {
                Err(DeviceError::Invalid("会话日志分段数已达上限".into()))
            })),
            "initial sink registration must succeed"
        );
        svc.register_dut(dut_with_relay("dut-a"));
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        let err = svc
            .execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(
            matches!(err, ControlEntryError::Device(DeviceError::Invalid(_))),
            "segment failure must report a Device error: {err:?}"
        );
        assert!(
            backend.calls.lock().unwrap().is_empty(),
            "segment failure must not reach the driver"
        );
        // A failing pre-power boundary does not block a different action.
        let token = approval.issue("dut-a", "control.reset", "{}");
        svc.execute_by_id("dut-a", ControlAction::Reset, &token.token)
            .unwrap();
        assert_eq!(
            backend.calls.lock().unwrap().len(),
            1,
            "Reset must still dispatch normally"
        );
    }

    /// A failed activation marker prevents ON and needs no release.
    /// A failed release marker still allows OFF.
    /// Activation and release use the same physical target.
    #[test]
    fn hold_channel_boundary_failure_intercepts_on_but_never_off() {
        // Fail the activation marker before calling the backend.
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
            "activation segment failure must report a Device error: {err:?}"
        );
        assert!(
            backend.calls.lock().unwrap().is_empty(),
            "activation segment failure must not reach the driver"
        );

        // With only the release marker failing, dispatch both ON and OFF.
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
            assert!(
                std::time::Instant::now() < deadline,
                "activation was not dispatched"
            );
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

    #[test]
    fn enter_maskrom_and_loader_always_release_strap_channel_off() {
        let backend = MockBackend::new();
        let (svc, approval) = service(Some(backend.clone()));
        let mut dut = dut_with_relay("dut-boot");
        if let RelayConfig::UsbSerial {
            ref mut channels, ..
        } = dut.relay
        {
            channels.maskrom = Some(5);
            channels.recovery = Some(6);
        }
        svc.register_dut(dut.clone());

        // 1) Normal MaskROM entry: maskrom ON -> reset -> maskrom OFF.
        let action = ControlAction::EnterMaskrom { hold_ms: 200 };
        let token = approval.issue(&dut.id, action.name(), &action.args_json());
        svc.execute_by_id(&dut.id, action, &token.token).unwrap();
        assert_eq!(
            *backend.calls.lock().unwrap(),
            [
                "power addr=COM7 driver=CH340 ch=5 on=true",
                "reset addr=COM7 driver=CH340 ch=1",
                "power addr=COM7 driver=CH340 ch=5 on=false",
            ]
        );

        // 2) Normal Loader entry: recovery ON -> reset -> recovery OFF.
        backend.calls.lock().unwrap().clear();
        let action = ControlAction::EnterLoader { hold_ms: 200 };
        let token = approval.issue(&dut.id, action.name(), &action.args_json());
        svc.execute_by_id(&dut.id, action, &token.token).unwrap();
        assert_eq!(
            *backend.calls.lock().unwrap(),
            [
                "power addr=COM7 driver=CH340 ch=6 on=true",
                "reset addr=COM7 driver=CH340 ch=1",
                "power addr=COM7 driver=CH340 ch=6 on=false",
            ]
        );

        // 3) Unbound maskrom channel (only reset wired) fails clearly without touching hardware.
        backend.calls.lock().unwrap().clear();
        let mut reset_only = dut_with_relay("dut-reset-only");
        if let RelayConfig::UsbSerial {
            ref mut channels, ..
        } = reset_only.relay
        {
            channels.power = None;
            channels.power_key = None;
            channels.maskrom = None;
            channels.recovery = None;
            channels.reset = Some(1);
        }
        svc.register_dut(reset_only.clone());
        let action = ControlAction::EnterMaskrom { hold_ms: 200 };
        let token = approval.issue(&reset_only.id, action.name(), &action.args_json());
        let err = svc
            .execute_by_id(&reset_only.id, action, &token.token)
            .unwrap_err()
            .to_string();
        assert!(err.contains("reset=CH1"), "{err}");
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}
