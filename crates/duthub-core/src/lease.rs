//! 控制租约（`docs/SECURITY.md`，SPEC §6）。
//!
//! 每 DUT 至多 1 个有效租约（INV-01）；TTL 默认 120s + renew/release；
//! generation 单调递增 fencing（INV-04：acquire/force_takeover +1，renew 不变）；
//! FIFO 等待队列可取消/超时（上限 8）；不可中断操作期间 Protected（INV-05）；
//! 进程重启全部作废（INV-07：纯内存态，旧 `lease_id` → `UnknownLease`）。
//!
//! [`ControlLeaseManager`] 同时实现 [`LeaseAuthority`]：写入通道（duthub-device
//! `DeviceService`）与控制入口（`ControlService`）在派发前经 admit 复核——
//! 这是 GUI 优先与 INV-01 的无条件执行点（SPEC §6.5 唯一 Actor）。
//!
//! 等待实现为 200ms 轮询（偏差 9：core 不引入 tokio；队列深度 ≤8 无性能问题）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::audit::{AuditOutcome, AuditSink, unix_ms};

/// 默认租约 TTL（SPEC §6.1：120s）。
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(120);
/// FIFO 等待队列上限（方案 §5.2）。
pub const QUEUE_CAP: usize = 8;
/// 等待轮询间隔（偏差 9）。
const WAIT_POLL: Duration = Duration::from_millis(200);

/// 高熵不透明租约句柄（SPEC §6.3：句柄 ≠ 权限；审计允许记录）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LeaseId(String);

impl LeaseId {
    fn new() -> Self {
        Self(format!("lease-{}", uuid::Uuid::new_v4()))
    }
}

impl std::fmt::Display for LeaseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 租约持有者（服务端确认的身份，不接受客户端自报，SPEC §7）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum LeaseHolder {
    /// GUI 交互路径（人类在场：会话打开即自动持有）
    Gui,
    Agent {
        agent_name: String,
        credential_id: String,
    },
}

impl LeaseHolder {
    /// 展示/Busy 消息用标签。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            LeaseHolder::Gui => "GUI（人类在场）".into(),
            LeaseHolder::Agent { agent_name, .. } => format!("Agent {agent_name}"),
        }
    }
}

/// 成功授予的租约凭证。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseGrant {
    pub lease_id: String,
    pub dut_id: String,
    pub generation: u64,
    pub expires_at_unix_ms: u64,
    /// 持有者标签（GUI / Agent 名）。
    pub holder: String,
}

/// Control lease errors (business layer of SPEC §6.4; HTTP 401 is handled at the transport layer).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    /// Target DUT is held by another principal.
    #[error("DUT 控制权被 {holder} 持有（等待队列 {queue_len}）")]
    Busy { holder: String, queue_len: usize },
    /// Non-interruptible operation in progress (e.g., firmware flash).
    #[error("DUT 处于保护状态（{op} 进行中），不接受写入与控制交接")]
    Protected { op: String },
    /// Outcome of previous protected operation is unknown; manual confirmation required.
    #[error("DUT 待人工检查（上次保护操作结果未知），确认后方可继续")]
    NeedsInspection,
    /// Lease exists but has expired.
    #[error("控制租约已过期，请重新获取")]
    Expired,
    /// Lease does not exist (expired, revoked, or process restarted).
    #[error("控制租约不存在（已过期/撤销或进程重启）")]
    UnknownLease,
    /// Caller does not own the referenced lease.
    #[error("控制租约与请求者不匹配")]
    NotHolder,
    /// Caller is not authorized to acquire a lease.
    #[error("未授权获取控制租约")]
    Unauthorized,
    /// Stale fencing generation.
    #[error("写入代数过期（fencing 拒绝；当前代数 {current}）")]
    Generation { current: u64 },
    /// Write attempted without a lease while authentication is enabled.
    #[error("需要先获取控制租约（dut_acquire_control）")]
    LeaseRequired,
    /// FIFO wait timed out while another holder kept the lease.
    #[error("等待控制权超时：租约仍被他人持有（等待队列 {queue_len}）")]
    WaitTimeout { queue_len: usize },
    /// FIFO fairness rejection: other waiters are already queued ahead.
    #[error(
        "DUT 控制权空闲但已有 {queue_len} 个请求在等待队列中，不接受插队（可携带 wait=true 排队等待）"
    )]
    QueuedAhead { queue_len: usize },
}

impl LeaseError {
    #[must_use]
    pub fn message(&self) -> String {
        self.to_string()
    }

    /// Maps a lease error into [`crate::error::DeviceError::Lease`] for GUI/MCP presentation.
    #[must_use]
    pub fn to_device_error(&self) -> crate::error::DeviceError {
        crate::error::DeviceError::Lease(self.message())
    }
}

/// 写入者身份（写入通道 admit 的判型输入）。
///
/// GUI 不携带 `lease_id：GUI` 租约由 `DeviceService` 自动持有（会话打开即取），
/// 管理器进程内确认 Gui 持有即放行（方案 §5.3「未取则现场 acquire」）。
/// Agent 按 SPEC 携带 (`lease_id`, generation)；过渡期（auth 关）允许不带租约
/// （偏差 7），GUI 租约在场仍必拒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteIdentity {
    Gui,
    Agent {
        lease_id: Option<String>,
        generation: Option<u64>,
    },
    /// 心跳 / 内部注记（Agent 控制期静默，Protected 期拒绝）
    System,
    /// 审批复核通过的写动作（P1-014 器件层前置）：一次性审批令牌在 MCP 层
    /// 校验后以该身份进入 admit。无租约要求——Free 放行（fence ctx 由
    /// `admit_queued_write` 按纪元补空租约上下文，与 auth 关时 Agent 无租约
    /// 路径同型）；任何在场租约（含 GUI）、人工接管、保护/待检查一律拒。
    Approval,
}

impl WriteIdentity {
    #[must_use]
    pub fn gui() -> Self {
        WriteIdentity::Gui
    }

    #[must_use]
    pub fn agent(lease_id: Option<String>, generation: Option<u64>) -> Self {
        WriteIdentity::Agent {
            lease_id,
            generation,
        }
    }

    #[must_use]
    pub fn system() -> Self {
        WriteIdentity::System
    }

    #[must_use]
    pub fn approval() -> Self {
        WriteIdentity::Approval
    }
}

/// 命令携带的租约上下文：入口 admit `成功时捕获（lease_id` + generation），
/// 会话 task actor 在写串口前最后一刻复核（SPEC §6.5.1 唯一 Actor 双点复核）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteCtx {
    pub lease_id: String,
    pub generation: u64,
}

/// 控制动作的 admit 结果：`temp_lease` 非空表示为本次动作现场获取的短租约，
/// 调用方执行完（无论成败）必须 [`LeaseAuthority::release_temporary`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlAdmission {
    pub ctx: Option<WriteCtx>,
    pub temp_lease: Option<LeaseGrant>,
}

/// 写入通道 / 控制入口的租约复核接口（duthub-device 消费）。
pub trait LeaseAuthority: Send + Sync {
    fn serial_io(&self) -> &tokio::sync::RwLock<()>;
    /// 写入复核（[D] 入口点）：Ok(None) = 无租约要求（过渡/系统/审批路径）；
    /// Ok(Some(ctx)) = 命令须携带 ctx 供 actor 复核；Err = 拒绝写入。
    fn admit_write(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
    ) -> Result<Option<WriteCtx>, LeaseError>;

    /// 控制动作复核：与 `admit_write` 同规则；Free 时 GUI 现场获取短租约
    /// （执行后释放，方案 §7.1）。
    fn admit_control(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
    ) -> Result<ControlAdmission, LeaseError>;

    /// [E] actor 复核：写串口前最后一刻验证 (`lease_id`, generation) 仍有效。
    fn validate_ctx(&self, dut_id: &str, ctx: &WriteCtx) -> Result<(), LeaseError>;

    /// 释放临时租约（控制动作收尾；幂等——已过期/已释放时静默）。
    fn release_temporary(&self, dut_id: &str, ctx: &WriteCtx);
}

/// 租约阶段（status 工具 / GUI 面板用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeasePhase {
    Free,
    Held,
    Protected,
    NeedsInspection,
}

/// DUT 控制权状态快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseStatus {
    pub dut_id: String,
    pub phase: LeasePhase,
    pub holder: Option<String>,
    pub generation: u64,
    pub remaining_secs: Option<u64>,
    pub queue_len: usize,
    pub protected_op: Option<String>,
}

#[derive(Debug)]
struct HeldLease {
    lease_id: String,
    holder: LeaseHolder,
    /// 队列授予标记（0 = 现场获取）
    request_id: u64,
    expires_at_ms: u64,
    revoked: bool,
}

#[derive(Debug)]
struct Waiter {
    request_id: u64,
    holder: LeaseHolder,
    deadline_ms: u64,
}

#[derive(Debug, Default)]
struct DutState {
    generation: u64,
    lease: Option<HeldLease>,
    protected_op: Option<String>,
    inspection: bool,
    queue: VecDeque<Waiter>,
    cancelled_waiters: HashSet<u64>,
}

/// 控制租约管理器：每 DUT 单租约 + FIFO 等待 + generation fencing + Protected。
///
/// 纯内存态（INV-07：进程重启全部作废）；审计事件经 [`AuditSink`]（脱敏）。
pub struct ControlLeaseManager {
    states: Mutex<HashMap<String, DutState>>,
    manual_epoch: AtomicU64,
    device_action: tokio::sync::RwLock<()>,
    ttl_ms: AtomicU64,
    gui_session_bound: bool,
    access: std::sync::OnceLock<Arc<crate::agent_access::AgentAccessRuntime>>,
    wait_poll: Duration,
    auth_enabled: AtomicBool,
    request_seq: AtomicU64,
    audit: Option<Arc<AuditSink>>,
}

impl std::fmt::Debug for ControlLeaseManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlLeaseManager")
            .field("ttl_ms", &self.ttl_ms.load(Ordering::Relaxed))
            .field("gui_session_bound", &self.gui_session_bound)
            .field("auth_enabled", &self.auth_enabled.load(Ordering::Relaxed))
            .field("manual_control", &self.manual_control())
            .finish_non_exhaustive()
    }
}

impl ControlLeaseManager {
    /// 产品构造：TTL 120s、轮询 200ms、审计可选。
    #[must_use]
    pub fn new(audit: Option<Arc<AuditSink>>) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            manual_epoch: AtomicU64::new(0),
            device_action: tokio::sync::RwLock::new(()),
            ttl_ms: AtomicU64::new(u64::try_from(DEFAULT_LEASE_TTL.as_millis()).unwrap_or(u64::MAX)),
            gui_session_bound: false,
            access: std::sync::OnceLock::new(),
            wait_poll: WAIT_POLL,
            auth_enabled: AtomicBool::new(false),
            request_seq: AtomicU64::new(0),
            audit,
        }
    }

    #[must_use]
    pub fn with_ttl(self, ttl: Duration) -> Self {
        self.set_ttl(ttl);
        self
    }

    /// Device services pair GUI leases with the lifetime of an open serial session.
    #[must_use]
    pub fn with_gui_session_lifetime(mut self) -> Self {
        self.gui_session_bound = true;
        self
    }

    pub fn set_ttl(&self, ttl: Duration) {
        self.ttl_ms.store(
            u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX).max(1),
            Ordering::Relaxed,
        );
    }

    pub fn set_agent_access(&self, access: Arc<crate::agent_access::AgentAccessRuntime>) {
        let _ = self.access.set(access);
    }

    fn holder_enabled(&self, holder: &LeaseHolder) -> bool {
        match (holder, self.access.get()) {
            (LeaseHolder::Agent { credential_id, .. }, Some(access)) => {
                access.lease_credential_enabled(credential_id)
            }
            _ => true,
        }
    }

    /// 测试构造：缩短轮询间隔保证等待用例的确定性。
    #[must_use]
    pub fn with_wait_poll(mut self, poll: Duration) -> Self {
        self.wait_poll = poll;
        self
    }

    /// auth 总开关联动（Agent 无租约写放行与否，偏差 7）。GUI 保存设置后热更新。
    pub fn set_auth_enabled(&self, enabled: bool) {
        self.auth_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn manual_control(&self) -> bool {
        self.manual_epoch.load(Ordering::Acquire) & 1 != 0
    }

    fn manual_busy() -> LeaseError {
        LeaseError::Busy {
            holder: "人工接管".into(),
            queue_len: 0,
        }
    }

    pub fn set_manual_control(&self, enabled: bool) -> Result<(), LeaseError> {
        let _action = self
            .device_action
            .try_write()
            .map_err(|_| LeaseError::Protected {
                op: "设备写入或控制操作".into(),
            })?;
        let mut states = crate::util::lock_unpoisoned(&self.states);
        if self.manual_control() == enabled {
            return Ok(());
        }
        if enabled {
            if let Some(op) = states.values().find_map(|state| state.protected_op.clone()) {
                return Err(LeaseError::Protected { op });
            }
            for (dut_id, state) in states.iter_mut() {
                state
                    .queue
                    .retain(|waiter| waiter.holder == LeaseHolder::Gui);
                if state
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.holder != LeaseHolder::Gui)
                {
                    state.lease = None;
                    state.generation = state.generation.saturating_add(1);
                    self.grant_to_queue_head(state, dut_id, unix_ms());
                }
            }
        }
        self.manual_epoch.fetch_add(1, Ordering::Release);
        self.audit_lease(
            "lease.manual",
            Some(&LeaseHolder::Gui),
            "",
            AuditOutcome::Ok,
            if enabled {
                "人工接管已启用"
            } else {
                "人工接管已解除"
            },
        );
        Ok(())
    }

    /// Keep a synchronous Agent operation and the manual-mode transition mutually exclusive.
    pub fn begin_agent_action(&self) -> Result<tokio::sync::RwLockReadGuard<'_, ()>, LeaseError> {
        let action = self
            .device_action
            .try_read()
            .map_err(|_| Self::manual_busy())?;
        if self.manual_control() {
            return Err(Self::manual_busy());
        }
        Ok(action)
    }

    /// Carry a fence through the serial queue even for previously unleased writes.
    /// 审批（Approval）身份同样经此补 fence `ctx：admit_write` 放行为 Ok(None)
    /// 时按当前纪元包装空租约 WriteCtx（P1-014 器件层前置）。
    pub fn admit_queued_write(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
    ) -> Result<Option<WriteCtx>, LeaseError> {
        let epoch = self.manual_epoch.load(Ordering::Acquire);
        self.admit_write(dut_id, writer).map(|ctx| {
            ctx.or_else(|| {
                Some(WriteCtx {
                    lease_id: String::new(),
                    generation: epoch,
                })
            })
        })
    }

    fn audit_lease(
        &self,
        action: &str,
        holder: Option<&LeaseHolder>,
        dut: &str,
        result: AuditOutcome,
        detail: &str,
    ) {
        let Some(audit) = &self.audit else { return };
        let (agent, credential_id) = match holder {
            Some(LeaseHolder::Agent {
                agent_name,
                credential_id,
            }) => (Some(agent_name.as_str()), Some(credential_id.as_str())),
            _ => (None, None),
        };
        audit.record(
            action,
            agent,
            credential_id,
            Some(dut),
            result,
            Some(detail),
        );
    }

    /// 获取租约：Free 即授（generation+1）；GUI 重复获取幂等返回现有凭证；
    /// 被他人持有 → Busy（`wait=Some(dur)` 时入 FIFO 队列轮询等待，超时 Busy）。
    pub fn acquire(
        &self,
        dut_id: &str,
        holder: &LeaseHolder,
        wait: Option<Duration>,
    ) -> Result<LeaseGrant, LeaseError> {
        let deadline_ms = wait.map(|w| {
            unix_ms().saturating_add(u64::try_from(w.as_millis()).unwrap_or(u64::MAX))
        });
        let request_id = deadline_ms
            .map_or(0, |_| self.request_seq.fetch_add(1, Ordering::Relaxed) + 1);
        loop {
            let now = unix_ms();
            let poll = {
                let mut states = crate::util::lock_unpoisoned(&self.states);
                let st = states.entry(dut_id.to_string()).or_default();
                if st.cancelled_waiters.remove(&request_id) {
                    return Err(LeaseError::Busy {
                        holder: st
                            .lease
                            .as_ref()
                            .map_or_else(|| "—".into(), |held| held.holder.label()),
                        queue_len: st.queue.len(),
                    });
                }
                if self.manual_control() && *holder != LeaseHolder::Gui {
                    return Err(Self::manual_busy());
                }
                if !self.holder_enabled(holder) {
                    Self::remove_waiter(st, request_id);
                    return Err(LeaseError::Unauthorized);
                }
                self.expire_if_due(st, dut_id, now);
                // Protected / 待检查：一律拒（SPEC §6.2 状态机；已排队者同样失败）
                if let Some(op) = &st.protected_op {
                    return Err(LeaseError::Protected { op: op.clone() });
                }
                if st.inspection {
                    return Err(LeaseError::NeedsInspection);
                }
                if let Some(held) = &st.lease {
                    if held.request_id == request_id && request_id != 0 {
                        if held.revoked {
                            return Err(LeaseError::Unauthorized);
                        }
                        // 轮询发现队列授予已落地 → 返回凭证
                        return Ok(Self::grant_view(dut_id, held, st.generation));
                    }
                    if *holder == LeaseHolder::Gui && held.holder == LeaseHolder::Gui {
                        // 幂等：同一持有者重复 acquire（GUI 自动路径）
                        return Ok(Self::grant_view(dut_id, held, st.generation));
                    }
                    let busy = LeaseError::Busy {
                        holder: held.holder.label(),
                        queue_len: st.queue.len(),
                    };
                    if deadline_ms.is_none() {
                        return Err(busy);
                    }
                    // 排队：上限检查 + FIFO 登记
                    if st.queue.len() >= QUEUE_CAP {
                        return Err(busy);
                    }
                    if !st.queue.iter().any(|w| w.request_id == request_id) {
                        st.queue.push_back(Waiter {
                            request_id,
                            holder: holder.clone(),
                            deadline_ms: deadline_ms.unwrap_or(now),
                        });
                        self.audit_lease(
                            "lease.queue",
                            Some(holder),
                            dut_id,
                            AuditOutcome::Ok,
                            &format!("排队等待（第 {} 位）", st.queue.len()),
                        );
                    }
                } else if st.queue.iter().any(|w| &w.holder != holder) {
                    // FIFO 公平：队列中有其他等待者时，新请求不得插队
                    // （P2-019：队列状态不是持有者名，用专用变体表述）
                    let busy = LeaseError::QueuedAhead {
                        queue_len: st.queue.len(),
                    };
                    if deadline_ms.is_none() {
                        return Err(busy);
                    }
                    if st.queue.len() >= QUEUE_CAP {
                        return Err(busy);
                    }
                    if !st.queue.iter().any(|w| w.request_id == request_id) {
                        st.queue.push_back(Waiter {
                            request_id,
                            holder: holder.clone(),
                            deadline_ms: deadline_ms.unwrap_or(now),
                        });
                    }
                } else {
                    // Free：直接授予（generation +1）
                    return Ok(self.grant_free(st, dut_id, holder, now, 0));
                }
                // 等待截止检查（锁外休眠）
                let deadline = deadline_ms.unwrap_or(now);
                if now >= deadline {
                    Self::remove_waiter(st, request_id);
                    self.audit_lease(
                        "lease.queue",
                        Some(holder),
                        dut_id,
                        AuditOutcome::Denied,
                        "等待超时",
                    );
                    return Err(LeaseError::WaitTimeout {
                        queue_len: st.queue.len(),
                    });
                }
                self.wait_poll
                    .min(Duration::from_millis(deadline.saturating_sub(now) + 1))
            };
            std::thread::sleep(poll);
        }
    }

    /// 续租（TTL 重置；generation 不变，SPEC §6.5）。过期 → Expired；
    /// 句柄不匹配 / 不存在 → UnknownLease；Protected 期拒绝。
    pub fn renew(&self, dut_id: &str, lease_id: &str) -> Result<LeaseGrant, LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return Err(LeaseError::UnknownLease);
        };
        let now = unix_ms();
        if let Some(op) = &st.protected_op {
            return Err(LeaseError::Protected { op: op.clone() });
        }
        if st.inspection {
            return Err(LeaseError::NeedsInspection);
        }
        let current_id = st.lease.as_ref().map(|h| h.lease_id.clone());
        match current_id {
            Some(id) if id == lease_id => {
                let expired = st.lease.as_ref().is_some_and(|h| now > h.expires_at_ms);
                if expired {
                    self.expire_if_due(st, dut_id, now);
                    return Err(LeaseError::Expired);
                }
                let holder = st
                    .lease
                    .as_ref()
                    .map(|h| h.holder.clone())
                    .expect("current_id matched active lease");
                if !self.holder_enabled(&holder) {
                    return Err(LeaseError::Unauthorized);
                }
                let held = st.lease.as_mut().expect("current_id matched active lease");
                if held.revoked {
                    return Err(LeaseError::Unauthorized);
                }
                if self.gui_session_bound && holder == LeaseHolder::Gui {
                    return Ok(Self::grant_view(dut_id, held, st.generation));
                }
                let ttl = self.ttl_ms.load(Ordering::Relaxed);
                held.expires_at_ms = now.saturating_add(ttl);
                self.audit_lease(
                    "lease.renew",
                    Some(&holder),
                    dut_id,
                    AuditOutcome::Ok,
                    &format!("续租至 +{ttl}ms"),
                );
                Ok(Self::grant_view(dut_id, held, st.generation))
            }
            _ => Err(LeaseError::UnknownLease),
        }
    }

    /// 释放（持有者本人；Protected 期拒绝——失败路径不允许交接，SPEC §6.4）。
    pub fn release(&self, dut_id: &str, lease_id: &str) -> Result<(), LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return Err(LeaseError::UnknownLease);
        };
        let now = unix_ms();
        if let Some(op) = &st.protected_op {
            return Err(LeaseError::Protected { op: op.clone() });
        }
        if st.inspection {
            return Err(LeaseError::NeedsInspection);
        }
        match &st.lease {
            Some(held) if held.lease_id == lease_id => {
                if held.revoked || !self.holder_enabled(&held.holder) {
                    return Err(LeaseError::Unauthorized);
                }
                let holder = held.holder.clone();
                st.lease = None;
                self.audit_lease(
                    "lease.release",
                    Some(&holder),
                    dut_id,
                    AuditOutcome::Ok,
                    "持有者释放租约",
                );
                self.grant_to_queue_head(st, dut_id, now);
                Ok(())
            }
            Some(_) => Err(LeaseError::NotHolder),
            None => Err(LeaseError::UnknownLease),
        }
    }

    /// MCP 写工具的归属 + 代数复核（方案 §6.2 [B]→[C] 合一）：租约必须在场、
    /// 句柄匹配、持有者是当前凭据的 Agent → 返回当前 generation 供命令携带。
    /// GUI 持有 / 他人凭据 → NotHolder；无租约 → `UnknownLease`。
    pub fn validate_agent_lease(
        &self,
        dut_id: &str,
        lease_id: &str,
        credential_id: &str,
    ) -> Result<u64, LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        if self.manual_control() {
            return Err(Self::manual_busy());
        }
        let Some(st) = states.get_mut(dut_id) else {
            return Err(LeaseError::UnknownLease);
        };
        let now = unix_ms();
        self.expire_if_due(st, dut_id, now);
        if let Some(op) = &st.protected_op {
            return Err(LeaseError::Protected { op: op.clone() });
        }
        if st.inspection {
            return Err(LeaseError::NeedsInspection);
        }
        match &st.lease {
            Some(held) if held.lease_id == lease_id => {
                if held.revoked || !self.holder_enabled(&held.holder) {
                    return Err(LeaseError::Unauthorized);
                }
                let owned = matches!(
                    &held.holder,
                    LeaseHolder::Agent { credential_id: cred, .. } if cred == credential_id
                );
                if owned {
                    Ok(st.generation)
                } else {
                    Err(LeaseError::NotHolder)
                }
            }
            Some(_) => Err(LeaseError::NotHolder),
            None => Err(LeaseError::UnknownLease),
        }
    }

    /// 按持有者释放（GUI 会话关闭自动释放；无租约时静默成功）。
    pub fn release_for_holder(&self, dut_id: &str, holder: &LeaseHolder) {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return;
        };
        let now = unix_ms();
        if st.protected_op.is_some() || st.inspection {
            return; // 保护/待检查期不交接（租约随 Protected 结束流程清理）
        }
        let matches = st.lease.as_ref().is_some_and(|held| &held.holder == holder);
        if matches {
            st.lease = None;
            self.audit_lease(
                "lease.release",
                Some(holder),
                dut_id,
                AuditOutcome::Ok,
                "会话关闭自动释放",
            );
            self.grant_to_queue_head(st, dut_id, now);
        }
    }

    pub fn revoke_credential(&self, credential_id: &str) {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let now = unix_ms();
        let belongs_to = |holder: &LeaseHolder| matches!(holder, LeaseHolder::Agent { credential_id: id, .. } if id == credential_id);
        for (dut_id, state) in states.iter_mut() {
            state.queue.retain(|waiter| !belongs_to(&waiter.holder));
            if let Some(held) = state.lease.as_mut().filter(|held| belongs_to(&held.holder)) {
                held.revoked = true;
                self.audit_lease(
                    "lease.revoke",
                    Some(&held.holder),
                    dut_id,
                    AuditOutcome::Ok,
                    "凭据停用或撤销",
                );
                self.expire_if_due(state, dut_id, now);
            }
        }
    }

    /// 状态快照（惰性过期在此触发；GUI 控制权面板 / `dut_control_status` 数据源）。
    pub fn status(&self, dut_id: &str) -> LeaseStatus {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            let (phase, holder) = if self.manual_control() {
                (LeasePhase::Held, Some("人工接管".into()))
            } else {
                (LeasePhase::Free, None)
            };
            return LeaseStatus {
                dut_id: dut_id.to_string(),
                phase,
                holder,
                generation: 0,
                remaining_secs: None,
                queue_len: 0,
                protected_op: None,
            };
        };
        let now = unix_ms();
        self.expire_if_due(st, dut_id, now);
        let (phase, holder, remaining) = if st.protected_op.is_some() {
            (
                LeasePhase::Protected,
                st.lease.as_ref().map(|h| h.holder.label()),
                None,
            )
        } else if st.inspection {
            (LeasePhase::NeedsInspection, None, None)
        } else if self.manual_control() {
            (LeasePhase::Held, Some("人工接管".into()), None)
        } else {
            match &st.lease {
                Some(held) => {
                    // GUI 自动租约无 TTL（P3-023）：剩余时间不可报，显示层取“-”
                    let remaining = if held.holder == LeaseHolder::Gui {
                        None
                    } else {
                        Some(held.expires_at_ms.saturating_sub(now) / 1000)
                    };
                    (LeasePhase::Held, Some(held.holder.label()), remaining)
                }
                None => (LeasePhase::Free, None, None),
            }
        };
        LeaseStatus {
            dut_id: dut_id.to_string(),
            phase,
            holder,
            generation: st.generation,
            remaining_secs: remaining,
            queue_len: st.queue.len(),
            protected_op: st.protected_op.clone(),
        }
    }

    /// [D]/[E] 共用验证：句柄 + 代数都匹配当前持有租约 → Ok(generation)。
    pub fn validate(
        &self,
        dut_id: &str,
        lease_id: &str,
        generation: u64,
    ) -> Result<u64, LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return Err(LeaseError::UnknownLease);
        };
        let now = unix_ms();
        self.expire_if_due(st, dut_id, now);
        if let Some(op) = &st.protected_op {
            return Err(LeaseError::Protected { op: op.clone() });
        }
        if st.inspection {
            return Err(LeaseError::NeedsInspection);
        }
        match &st.lease {
            Some(held) if held.lease_id == lease_id => {
                if held.revoked || !self.holder_enabled(&held.holder) {
                    return Err(LeaseError::Unauthorized);
                }
                if held.expires_at_ms < now {
                    self.expire_if_due(st, dut_id, now);
                    return Err(LeaseError::Expired);
                }
                if st.generation == generation {
                    Ok(st.generation)
                } else {
                    Err(LeaseError::Generation {
                        current: st.generation,
                    })
                }
            }
            Some(_) => Err(LeaseError::NotHolder),
            None => Err(LeaseError::UnknownLease),
        }
    }

    /// 进入保护态（不可中断操作期间：acquire/renew/release/写入全拒，INV-05）。
    /// 要求当前有租约（Held → Protected，SPEC §6.2 状态机）。
    pub fn enter_protected(&self, dut_id: &str, op: &str) -> Result<(), LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let st = states.entry(dut_id.to_string()).or_default();
        let now = unix_ms();
        self.expire_if_due(st, dut_id, now);
        if st.protected_op.is_some() {
            return Err(LeaseError::Protected {
                op: st.protected_op.clone().unwrap_or_default(),
            });
        }
        let Some(held) = &st.lease else {
            return Err(LeaseError::UnknownLease);
        };
        let holder = held.holder.clone();
        st.protected_op = Some(op.to_string());
        self.audit_lease(
            "lease.protected.enter",
            Some(&holder),
            dut_id,
            AuditOutcome::Ok,
            &format!("进入保护态：{op}"),
        );
        Ok(())
    }

    /// 退出保护态：`outcome_ok=true` → Free；false（结果未知）→ `NeedsInspection`，
    /// 经 [`Self::resolve_inspection`] 人工确认后回 Free。
    pub fn exit_protected(&self, dut_id: &str, outcome_ok: bool) -> Result<(), LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return Err(LeaseError::UnknownLease);
        };
        let now = unix_ms();
        let Some(op) = st.protected_op.take() else {
            return Err(LeaseError::UnknownLease);
        };
        st.lease = None; // 两种出口都回 Free（SPEC §6.2：Protected 不回 Held）
        self.audit_lease(
            "lease.protected.exit",
            None,
            dut_id,
            if outcome_ok {
                AuditOutcome::Ok
            } else {
                AuditOutcome::Error
            },
            &format!(
                "退出保护态：{op}（结果 {}）",
                if outcome_ok { "成功" } else { "未知" }
            ),
        );
        if outcome_ok {
            self.grant_to_queue_head(st, dut_id, now);
        } else {
            st.inspection = true;
        }
        Ok(())
    }

    /// 人工确认检查结果 → Free（SPEC §6.2 `NeedsInspection` --人工确认--> Free）。
    pub fn resolve_inspection(&self, dut_id: &str, safe: bool) {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return;
        };
        if st.inspection && safe {
            st.inspection = false;
            let now = unix_ms();
            self.audit_lease(
                "lease.protected.exit",
                None,
                dut_id,
                AuditOutcome::Ok,
                "人工确认完成，恢复可用",
            );
            self.grant_to_queue_head(st, dut_id, now);
        }
    }

    /// GUI 强制接管（SPEC §6.7）：撤销旧租约（generation+1）→ 直接授予 GUI。
    /// 旧持有者下一次写入在 admit/actor 复核被拒。Protected/待检查期不可接管。
    pub fn force_takeover(&self, dut_id: &str) -> Result<LeaseGrant, LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let st = states.entry(dut_id.to_string()).or_default();
        let now = unix_ms();
        self.expire_if_due(st, dut_id, now);
        if let Some(op) = &st.protected_op {
            return Err(LeaseError::Protected { op: op.clone() });
        }
        if st.inspection {
            return Err(LeaseError::NeedsInspection);
        }
        if let Some(held) = &st.lease {
            self.audit_lease(
                "lease.takeover",
                Some(&held.holder),
                dut_id,
                AuditOutcome::Ok,
                &format!("GUI 强制接管，撤销 {}", held.holder.label()),
            );
        }
        st.lease = None;
        st.queue.clear(); // 接管优先于等待队列（人类在场优先）
        self.audit_lease(
            "lease.queue",
            None,
            dut_id,
            AuditOutcome::Ok,
            "强制接管清空等待队列",
        );
        Ok(self.grant_free(st, dut_id, &LeaseHolder::Gui, now, 0))
    }

    /// 取消排队等待（MCP `dut_acquire_control` 可取消）。
    pub fn cancel_wait(&self, dut_id: &str, request_id: u64) -> bool {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return false;
        };
        let removed = Self::remove_waiter(st, request_id);
        if removed {
            if st.cancelled_waiters.len() >= 128 && st.queue.is_empty() {
                st.cancelled_waiters.clear();
            }
            st.cancelled_waiters.insert(request_id);
            self.audit_lease(
                "lease.cancel",
                None,
                dut_id,
                AuditOutcome::Ok,
                &format!("取消等待 request={request_id}"),
            );
        }
        removed
    }

    /// 当前排队等待的请求 id（GUI 面板 / MCP 取消入口用）。
    pub fn pending_wait_requests(&self, dut_id: &str) -> Vec<u64> {
        let states = crate::util::lock_unpoisoned(&self.states);
        states
            .get(dut_id)
            .map_or_default(|st| st.queue.iter().map(|w| w.request_id).collect())
    }

    fn remove_waiter(st: &mut DutState, request_id: u64) -> bool {
        let before = st.queue.len();
        st.queue.retain(|w| w.request_id != request_id);
        before != st.queue.len()
    }

    /// 惰性过期：租约到期 → Free（审计）→ 队首等待者按 FIFO 授予。
    fn expire_if_due(&self, st: &mut DutState, dut_id: &str, now: u64) {
        if st.protected_op.is_some() || st.inspection {
            return; // 保护/待检查期不惰性过期（exit_protected 统一清理）
        }
        if st.lease.as_ref().is_some_and(|held| held.revoked) {
            // In-flight device actions hold the read side. Keep the old lease fenced until they finish.
            if let Ok(_barrier) = self.device_action.try_write() {
                st.lease = None;
                self.grant_to_queue_head(st, dut_id, now);
            }
            return;
        }
        let expired = st
            .lease
            .as_ref()
            .is_some_and(|held| now > held.expires_at_ms);
        if expired {
            let holder = st.lease.as_ref().map(|h| h.holder.clone());
            st.lease = None;
            let label = holder
                .as_ref().map_or_else(|| "?".into(), LeaseHolder::label);
            self.audit_lease(
                "lease.expire",
                holder.as_ref(),
                dut_id,
                AuditOutcome::Ok,
                &format!("TTL 到期，释放 {label}"),
            );
            self.grant_to_queue_head(st, dut_id, now);
        }
    }

    /// Free → 队首有效等待者授予（deadline 已过的跳过并审计）。
    fn grant_to_queue_head(&self, st: &mut DutState, dut_id: &str, now: u64) {
        while let Some(head) = st.queue.front() {
            if !self.holder_enabled(&head.holder) {
                st.queue.pop_front();
                continue;
            }
            if head.deadline_ms < now {
                let skipped = st.queue.pop_front().expect("queue front was just inspected");
                self.audit_lease(
                    "lease.queue",
                    Some(&skipped.holder),
                    dut_id,
                    AuditOutcome::Denied,
                    "等待截止已过，跳过",
                );
                continue;
            }
            let head = st.queue.pop_front().expect("queue front was just inspected");
            self.grant_free(st, dut_id, &head.holder, now, head.request_id);
            return;
        }
    }

    /// 现场授予（generation +1；`request_id` 非零 = 队列授予）。
    fn grant_free(
        &self,
        st: &mut DutState,
        dut_id: &str,
        holder: &LeaseHolder,
        now: u64,
        request_id: u64,
    ) -> LeaseGrant {
        st.generation = st.generation.saturating_add(1);
        let lease_id = LeaseId::new();
        // GUI 自动租约无 TTL（P3-023 双保险）：expire_if_due 已按持有者跳过，
        // expires_at 取 u64::MAX 让到期比较/剩余时间同样不会触发
        let expires_at = if self.gui_session_bound && *holder == LeaseHolder::Gui {
            u64::MAX
        } else {
            now.saturating_add(self.ttl_ms.load(Ordering::Relaxed))
        };
        st.lease = Some(HeldLease {
            lease_id: lease_id.to_string(),
            holder: holder.clone(),
            request_id,
            expires_at_ms: expires_at,
            revoked: false,
        });
        self.audit_lease(
            "lease.acquire",
            Some(holder),
            dut_id,
            AuditOutcome::Ok,
            &format!(
                "授予 {} generation={}（队列授予={})",
                lease_id,
                st.generation,
                request_id != 0
            ),
        );
        LeaseGrant {
            lease_id: lease_id.to_string(),
            dut_id: dut_id.to_string(),
            generation: st.generation,
            expires_at_unix_ms: expires_at,
            holder: holder.label(),
        }
    }

    fn grant_view(dut_id: &str, held: &HeldLease, generation: u64) -> LeaseGrant {
        LeaseGrant {
            lease_id: held.lease_id.clone(),
            dut_id: dut_id.to_string(),
            generation,
            expires_at_unix_ms: held.expires_at_ms,
            holder: held.holder.label(),
        }
    }
}

impl LeaseAuthority for ControlLeaseManager {
    fn serial_io(&self) -> &tokio::sync::RwLock<()> {
        &self.device_action
    }
    fn admit_write(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
    ) -> Result<Option<WriteCtx>, LeaseError> {
        match writer {
            WriteIdentity::System => {
                let mut states = crate::util::lock_unpoisoned(&self.states);
                if self.manual_control() {
                    return Err(Self::manual_busy());
                }
                let st = states.entry(dut_id.to_string()).or_default();
                let now = unix_ms();
                self.expire_if_due(st, dut_id, now);
                if let Some(op) = &st.protected_op {
                    return Err(LeaseError::Protected { op: op.clone() });
                }
                if st.inspection {
                    return Err(LeaseError::NeedsInspection);
                }
                match &st.lease {
                    // Agent 控制期心跳静默（方案 §5.3 admit 表）
                    Some(held) if held.holder != LeaseHolder::Gui => Err(LeaseError::Busy {
                        holder: held.holder.label(),
                        queue_len: st.queue.len(),
                    }),
                    _ => Ok(None),
                }
            }
            WriteIdentity::Gui => {
                let mut states = crate::util::lock_unpoisoned(&self.states);
                let st = states.entry(dut_id.to_string()).or_default();
                let now = unix_ms();
                self.expire_if_due(st, dut_id, now);
                if let Some(op) = &st.protected_op {
                    return Err(LeaseError::Protected { op: op.clone() });
                }
                if st.inspection {
                    return Err(LeaseError::NeedsInspection);
                }
                match &st.lease {
                    Some(held) if held.holder == LeaseHolder::Gui => Ok(Some(WriteCtx {
                        lease_id: held.lease_id.clone(),
                        generation: st.generation,
                    })),
                    Some(held) => Err(LeaseError::Busy {
                        holder: held.holder.label(),
                        queue_len: st.queue.len(),
                    }),
                    None => {
                        // Free：GUI 现场获取（人类在场自动租约，方案 §5.3 admit 表）
                        let grant = self.grant_free(st, dut_id, &LeaseHolder::Gui, now, 0);
                        Ok(Some(WriteCtx {
                            lease_id: grant.lease_id,
                            generation: grant.generation,
                        }))
                    }
                }
            }
            WriteIdentity::Agent {
                lease_id,
                generation,
            } => {
                let mut states = crate::util::lock_unpoisoned(&self.states);
                if self.manual_control() {
                    return Err(Self::manual_busy());
                }
                let st = states.entry(dut_id.to_string()).or_default();
                let now = unix_ms();
                self.expire_if_due(st, dut_id, now);
                if let Some(op) = &st.protected_op {
                    return Err(LeaseError::Protected { op: op.clone() });
                }
                if st.inspection {
                    return Err(LeaseError::NeedsInspection);
                }
                // 持有者判定（GUI 优先无条件生效，偏差 7）：任何在场租约都拒绝
                // 非持有者的 Agent 写——包括无租约的过渡形态
                let held = st
                    .lease
                    .as_ref()
                    .map(|h| (h.lease_id.clone(), h.holder.label()));
                match held {
                    Some((held_id, holder_label)) => {
                        if lease_id.as_deref() == Some(held_id.as_str()) {
                            if st.lease.as_ref().is_some_and(|held| {
                                held.revoked || !self.holder_enabled(&held.holder)
                            }) {
                                return Err(LeaseError::Unauthorized);
                            }
                            // 本人是持有者：代数必须匹配（fencing）
                            let Some(expected_gen) = generation.as_ref() else {
                                return Err(LeaseError::Busy {
                                    holder: holder_label,
                                    queue_len: st.queue.len(),
                                });
                            };
                            if st.generation != *expected_gen {
                                return Err(LeaseError::Generation {
                                    current: st.generation,
                                });
                            }
                            Ok(Some(WriteCtx {
                                lease_id: held_id,
                                generation: *expected_gen,
                            }))
                        } else {
                            Err(LeaseError::Busy {
                                holder: holder_label,
                                queue_len: st.queue.len(),
                            })
                        }
                    }
                    None => match lease_id {
                        // Free 且带句柄：句柄已不存在（过期/撤销/重启）
                        Some(_) => Err(LeaseError::UnknownLease),
                        // Free 且无租约：auth 关 → 过渡放行（偏差 7）；auth 开 → 拒
                        None => {
                            if self.auth_enabled.load(Ordering::Relaxed) {
                                Err(LeaseError::LeaseRequired)
                            } else {
                                Ok(None)
                            }
                        }
                    },
                }
            }
            WriteIdentity::Approval => {
                let mut states = crate::util::lock_unpoisoned(&self.states);
                if self.manual_control() {
                    return Err(Self::manual_busy());
                }
                let st = states.entry(dut_id.to_string()).or_default();
                let now = unix_ms();
                self.expire_if_due(st, dut_id, now);
                if let Some(op) = &st.protected_op {
                    return Err(LeaseError::Protected { op: op.clone() });
                }
                if st.inspection {
                    return Err(LeaseError::NeedsInspection);
                }
                match &st.lease {
                    // 审批写不是租约持有者：任何在场租约（含 GUI 自动租约）都拒，
                    // Busy 指向当前持有者（GUI 优先无条件生效）
                    Some(held) => Err(LeaseError::Busy {
                        holder: held.holder.label(),
                        queue_len: st.queue.len(),
                    }),
                    // Free：放行。返回 Ok(None)（无租约要求路径），纪元 fence 由
                    // admit_queued_write 补空租约 WriteCtx（与 auth 关时 Agent
                    // 无租约路径同型；actor 侧 validate_ctx 兼容空 lease_id）
                    None => Ok(None),
                }
            }
        }
    }

    fn admit_control(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
    ) -> Result<ControlAdmission, LeaseError> {
        match writer {
            WriteIdentity::Gui => {
                let mut states = crate::util::lock_unpoisoned(&self.states);
                let st = states.entry(dut_id.to_string()).or_default();
                let now = unix_ms();
                self.expire_if_due(st, dut_id, now);
                if let Some(op) = &st.protected_op {
                    return Err(LeaseError::Protected { op: op.clone() });
                }
                if st.inspection {
                    return Err(LeaseError::NeedsInspection);
                }
                match &st.lease {
                    Some(held) if held.holder == LeaseHolder::Gui => Ok(ControlAdmission {
                        ctx: Some(WriteCtx {
                            lease_id: held.lease_id.clone(),
                            generation: st.generation,
                        }),
                        temp_lease: None,
                    }),
                    Some(held) => Err(LeaseError::Busy {
                        holder: held.holder.label(),
                        queue_len: st.queue.len(),
                    }),
                    None => {
                        // 无 GUI 租约：现场获取短租约，执行后释放（方案 §7.1）
                        let grant = self.grant_free(st, dut_id, &LeaseHolder::Gui, now, 0);
                        Ok(ControlAdmission {
                            ctx: Some(WriteCtx {
                                lease_id: grant.lease_id.clone(),
                                generation: grant.generation,
                            }),
                            temp_lease: Some(grant),
                        })
                    }
                }
            }
            other => {
                // Agent / System：与写入同规则（System 只在 Protected 拒）
                let ctx = self.admit_write(dut_id, other)?;
                Ok(ControlAdmission {
                    ctx,
                    temp_lease: None,
                })
            }
        }
    }

    fn validate_ctx(&self, dut_id: &str, ctx: &WriteCtx) -> Result<(), LeaseError> {
        if ctx.lease_id.is_empty() {
            let epoch = self.manual_epoch.load(Ordering::Acquire);
            return if epoch == ctx.generation && epoch & 1 == 0 {
                Ok(())
            } else {
                Err(Self::manual_busy())
            };
        }
        self.validate(dut_id, &ctx.lease_id, ctx.generation)
            .map(|_| ())
    }

    fn release_temporary(&self, dut_id: &str, ctx: &WriteCtx) {
        // 幂等：租约已过期/已被接管时静默忽略
        let _ = self.release(dut_id, &ctx.lease_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_control_fences_existing_and_unleased_agent_writes() {
        let manager = manager();
        let agent = LeaseHolder::Agent {
            agent_name: "fixture".into(),
            credential_id: "fixture-id".into(),
        };
        let grant = manager.acquire("dut-a", &agent, None).unwrap();
        let previous_lease = WriteIdentity::agent(Some(grant.lease_id), Some(grant.generation));
        let queued = manager
            .admit_queued_write("dut-b", &WriteIdentity::agent(None, None))
            .unwrap()
            .unwrap();
        let in_flight = manager.begin_agent_action().unwrap();
        assert!(matches!(
            manager.set_manual_control(true),
            Err(LeaseError::Protected { .. })
        ));
        drop(in_flight);

        manager.set_manual_control(true).unwrap();
        assert!(manager.manual_control());
        assert!(manager.acquire("dut-b", &agent, None).is_err());
        assert!(manager.admit_write("dut-a", &previous_lease).is_err());
        assert!(
            manager
                .admit_write("dut-b", &WriteIdentity::system())
                .is_err()
        );
        assert!(manager.validate_ctx("dut-b", &queued).is_err());
        assert!(manager.admit_write("dut-a", &WriteIdentity::gui()).is_ok());

        manager.set_manual_control(false).unwrap();
        assert!(manager.admit_write("dut-a", &previous_lease).is_err());
        assert!(manager.validate_ctx("dut-b", &queued).is_err());
        assert!(
            manager
                .admit_write("dut-b", &WriteIdentity::agent(None, None))
                .is_ok()
        );
    }

    fn manager() -> ControlLeaseManager {
        ControlLeaseManager::new(Some(AuditSink::memory())).with_ttl(Duration::from_secs(120))
    }

    #[test]
    fn acquire_is_exclusive_and_generation_monotonic() {
        let m = manager();
        let g1 = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        assert_eq!(g1.generation, 1);
        assert!(g1.lease_id.starts_with("lease-"));
        // INV-01：第二个持有者必须 Busy，且消息带持有者
        let err = m
            .acquire(
                "dut",
                &LeaseHolder::Agent {
                    agent_name: "bot".into(),
                    credential_id: "cred".into(),
                },
                None,
            )
            .unwrap_err();
        match err {
            LeaseError::Busy { holder, .. } => assert!(holder.contains("GUI"), "{holder}"),
            other => panic!("期望 Busy，得到 {other:?}"),
        }
        // renew 不变代数
        let g1r = m.renew("dut", &g1.lease_id).unwrap();
        assert_eq!(g1r.generation, 1);
        assert!(g1r.expires_at_unix_ms >= g1.expires_at_unix_ms);
        // release → 再次 acquire → generation+1（INV-04 基础）
        m.release("dut", &g1.lease_id).unwrap();
        let g2 = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        assert_eq!(g2.generation, 2);
        assert_ne!(g2.lease_id, g1.lease_id);
        // 旧代数句柄验证：新租约在场 → 旧句柄 NotHolder（不是本人）
        assert!(matches!(
            m.validate("dut", &g1.lease_id, 1),
            Err(LeaseError::NotHolder)
        ));
    }

    #[test]
    fn ttl_expiry_is_lazy_and_renew_of_expired_is_expired() {
        let m =
            ControlLeaseManager::new(Some(AuditSink::memory())).with_ttl(Duration::from_millis(60));
        // P3-023 后 GUI 租约不做 TTL 过期：本用例改用 Agent 持有者验证 TTL
        // 惰性过期机制（对照组），GUI 免过期行为见 gui_lease_does_not_expire。
        let g = m
            .acquire(
                "dut",
                &LeaseHolder::Agent {
                    agent_name: "bot".into(),
                    credential_id: "cred".into(),
                },
                None,
            )
            .unwrap();
        let status_held = m.status("dut");
        assert_eq!(status_held.phase, LeasePhase::Held);
        std::thread::sleep(Duration::from_millis(90));
        // 先 renew：租约还在（惰性过期未触发）→ 明确的 Expired（区分 UnknownLease）
        assert_eq!(m.renew("dut", &g.lease_id), Err(LeaseError::Expired));
        // status 触发惰性过期 → Free
        let status = m.status("dut");
        assert_eq!(status.phase, LeasePhase::Free);
        // 过期清理后同一句柄写入复核 → UnknownLease
        assert_eq!(
            m.validate("dut", &g.lease_id, g.generation),
            Err(LeaseError::UnknownLease)
        );
    }

    /// P3-023：GUI 自动租约不做 TTL 过期——推进到过期点之后 status 仍 Held，
    /// Agent acquire 仍 Busy（「GUI 在场→拒 Agent」防线不随 120s TTL 失效）。
    #[test]
    fn gui_lease_does_not_expire_and_keeps_blocking_agent() {
        let m = ControlLeaseManager::new(Some(AuditSink::memory()))
            .with_ttl(Duration::from_millis(60))
            .with_gui_session_lifetime();
        let grant = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        // 双保险：GUI 授予的到期时刻为 u64::MAX
        assert_eq!(grant.expires_at_unix_ms, u64::MAX);
        std::thread::sleep(Duration::from_millis(90));
        // 每帧 status 轮询触发的惰性清理不得清掉 GUI 租约
        let status = m.status("dut");
        assert_eq!(status.phase, LeasePhase::Held);
        assert_eq!(status.holder.as_deref(), Some("GUI（人类在场）"));
        assert_eq!(status.remaining_secs, None);
        // Agent acquire 得 Busy（防线在 TTL 之外仍然生效）
        let err = m
            .acquire(
                "dut",
                &LeaseHolder::Agent {
                    agent_name: "bot".into(),
                    credential_id: "cred".into(),
                },
                None,
            )
            .unwrap_err();
        assert!(matches!(err, LeaseError::Busy { .. }), "{err:?}");
        // GUI 写入复核（admit）仍放行
        assert!(m.admit_write("dut", &WriteIdentity::gui()).is_ok());
        // 释放路径不变：GUI 释放后恢复 Free
        m.release_for_holder("dut", &LeaseHolder::Gui);
        assert_eq!(m.status("dut").phase, LeasePhase::Free);
    }

    #[test]
    fn fifo_queue_grants_in_order() {
        let m = Arc::new(manager().with_wait_poll(Duration::from_millis(10)));
        let g1 = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        let agent_a = LeaseHolder::Agent {
            agent_name: "bot-a".into(),
            credential_id: "cred".into(),
        };
        let agent_b = LeaseHolder::Agent {
            agent_name: "bot-b".into(),
            credential_id: "cred".into(),
        };
        // 两个等待者：a 先 b 后（FIFO）
        let h = {
            let m = m.clone();
            let holder = agent_a.clone();
            std::thread::spawn(move || m.acquire("dut", &holder, Some(Duration::from_millis(1500))))
        };
        std::thread::sleep(Duration::from_millis(60));
        let h2 = {
            let m = m.clone();
            let holder = agent_b.clone();
            std::thread::spawn(move || m.acquire("dut", &holder, Some(Duration::from_millis(1500))))
        };
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(m.status("dut").queue_len, 2);
        m.release("dut", &g1.lease_id).unwrap();
        // FIFO：先释放授予先排队者
        let ga = h.join().unwrap().unwrap();
        assert_eq!(ga.holder, "Agent bot-a");
        m.release("dut", &ga.lease_id).unwrap();
        let gb = h2.join().unwrap().unwrap();
        assert_eq!(gb.holder, "Agent bot-b");
        assert!(ga.generation < gb.generation, "两次授予代数必须递增");
    }

    #[test]
    fn wait_timeout_returns_wait_timeout_and_cancel_removes_waiter() {
        let m = Arc::new(manager().with_wait_poll(Duration::from_millis(10)));
        let _g1 = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        let agent = LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred".into(),
        };
        let started = std::time::Instant::now();
        let err = m
            .acquire("dut", &agent, Some(Duration::from_millis(80)))
            .unwrap_err();
        // P2-019：等待超时是专用变体（不再把状态词塞进 Busy.holder）
        assert!(matches!(err, LeaseError::WaitTimeout { .. }), "{err:?}");
        assert!(
            err.message().contains("等待控制权超时"),
            "消息必须表达等待超时：{}",
            err.message()
        );
        assert!(
            started.elapsed() >= Duration::from_millis(70),
            "必须等到截止"
        );
        // 取消排队：外部经 pending_wait_requests 拿到 request_id 后取消
        let waiter_thread = {
            let m = m.clone();
            let holder = agent.clone();
            std::thread::spawn(move || m.acquire("dut", &holder, Some(Duration::from_millis(900))))
        };
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(m.status("dut").queue_len, 1);
        let request_id = *m
            .pending_wait_requests("dut")
            .first()
            .expect("排队中应可取到 request_id");
        assert!(m.cancel_wait("dut", request_id), "取消必须移除等待者");
        assert_eq!(m.status("dut").queue_len, 0);
        // 被取消的等待者最终超时（无人授予）
        assert!(waiter_thread.join().unwrap().is_err());
        assert!(matches!(
            m.status("dut").phase,
            LeasePhase::Held | LeasePhase::Free
        ));
    }

    #[test]
    fn protected_blocks_acquire_renew_release_and_system_write() {
        let m = manager();
        let g = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        m.enter_protected("dut", "烧录中").unwrap();
        let agent = LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred".into(),
        };
        assert!(matches!(
            m.acquire("dut", &agent, None),
            Err(LeaseError::Protected { .. })
        ));
        assert_eq!(
            m.renew("dut", &g.lease_id),
            Err(LeaseError::Protected {
                op: "烧录中".into()
            })
        );
        assert!(matches!(
            m.release("dut", &g.lease_id),
            Err(LeaseError::Protected { .. })
        ));
        assert!(matches!(
            m.admit_write("dut", &WriteIdentity::system()),
            Err(LeaseError::Protected { .. })
        ));
        assert!(matches!(
            m.admit_write("dut", &WriteIdentity::gui()),
            Err(LeaseError::Protected { .. })
        ));
        assert!(matches!(
            m.force_takeover("dut"),
            Err(LeaseError::Protected { .. })
        ));
        // exit(ok) → Free；exit(unknown) → NeedsInspection → 人工确认 → Free
        m.exit_protected("dut", true).unwrap();
        assert_eq!(m.status("dut").phase, LeasePhase::Free);
        let g2 = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        assert!(g2.generation > g.generation);
        m.enter_protected("dut", "烧录中2").unwrap();
        m.exit_protected("dut", false).unwrap();
        assert_eq!(m.status("dut").phase, LeasePhase::NeedsInspection);
        assert!(matches!(
            m.acquire("dut", &LeaseHolder::Gui, None),
            Err(LeaseError::NeedsInspection)
        ));
        m.resolve_inspection("dut", true);
        assert_eq!(m.status("dut").phase, LeasePhase::Free);
    }

    #[test]
    fn restart_invalidates_all_leases() {
        let m1 = manager();
        let g = m1.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        // INV-07：新管理器（进程重启）对旧句柄一律 UnknownLease
        let m2 = manager();
        assert_eq!(
            m2.validate("dut", &g.lease_id, g.generation),
            Err(LeaseError::UnknownLease)
        );
        assert_eq!(m2.renew("dut", &g.lease_id), Err(LeaseError::UnknownLease));
        assert_eq!(m2.status("dut").phase, LeasePhase::Free);
    }

    #[test]
    fn force_takeover_revokes_and_grants_gui() {
        let m = manager();
        let agent = LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred".into(),
        };
        let g_agent = m.acquire("dut", &agent, None).unwrap();
        // Agent 持有期间 GUI 写入被拒（INV-01 另一侧）
        assert!(matches!(
            m.admit_write("dut", &WriteIdentity::gui()),
            Err(LeaseError::Busy { .. })
        ));
        let g_gui = m.force_takeover("dut").unwrap();
        assert!(g_gui.generation > g_agent.generation);
        // 旧持有者句柄失效：新租约（GUI）在场 → Busy 指向 GUI（INV-04 fencing 另一侧）
        assert!(matches!(
            m.admit_write(
                "dut",
                &WriteIdentity::agent(Some(g_agent.lease_id), Some(g_agent.generation))
            ),
            Err(LeaseError::Busy { .. })
        ));
        // GUI 现在可以写
        assert!(m.admit_write("dut", &WriteIdentity::gui()).is_ok());
    }

    #[test]
    fn auth_enabled_requires_agent_lease_and_two_duts_are_independent() {
        let m = manager();
        m.set_auth_enabled(true);
        // auth ON：无租约 Agent 写 → LeaseRequired
        assert!(matches!(
            m.admit_write("dut-a", &WriteIdentity::agent(None, None)),
            Err(LeaseError::LeaseRequired)
        ));
        m.set_auth_enabled(false);
        // auth OFF（默认）：无租约 Agent 写放行（过渡，偏差 7）
        assert_eq!(
            m.admit_write("dut-a", &WriteIdentity::agent(None, None)),
            Ok(None)
        );
        // INV-06：跨 DUT 并行互不阻塞
        let ga = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
        let gb = m.acquire("dut-b", &LeaseHolder::Gui, None).unwrap();
        assert_eq!(ga.generation, 1);
        assert_eq!(gb.generation, 1);
        assert_ne!(ga.lease_id, gb.lease_id);
    }

    #[test]
    fn agent_holds_gui_writes_denied_and_system_passes() {
        let m = manager();
        let agent = LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred".into(),
        };
        let g = m.acquire("dut", &agent, None).unwrap();
        // GUI 拒、Agent 本人放行、System（心跳）静默被拒（Agent 控制期）
        assert!(matches!(
            m.admit_write("dut", &WriteIdentity::gui()),
            Err(LeaseError::Busy { .. })
        ));
        assert!(matches!(
            m.admit_write("dut", &WriteIdentity::system()),
            Err(LeaseError::Busy { .. })
        ));
        assert!(
            m.admit_write(
                "dut",
                &WriteIdentity::agent(Some(g.lease_id.clone()), Some(g.generation))
            )
            .is_ok()
        );
        // 错误代数 → Generation fencing
        assert!(matches!(
            m.admit_write("dut", &WriteIdentity::agent(Some(g.lease_id), Some(999))),
            Err(LeaseError::Generation { .. })
        ));
        // 换 GUI 持有：System 放行（人类在场心跳照发）
        m.force_takeover("dut").unwrap();
        assert_eq!(m.admit_write("dut", &WriteIdentity::system()), Ok(None));
    }

    /// P1-014 器件层前置：Approval 写身份的 admit 规则——Free 放行（空租约
    /// 纪元 fence）、任何在场租约（Agent/GUI）Busy 指向持有者、人工接管拒。
    #[test]
    fn approval_identity_admission_rules() {
        let m = manager();
        let agent = LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred".into(),
        };
        // Free：放行，admit_queued_write 补空租约纪元 fence
        let ctx = m
            .admit_queued_write("dut-a", &WriteIdentity::approval())
            .unwrap()
            .expect("Free 时必须有 fence ctx");
        assert_eq!(ctx.lease_id, "", "审批写无租约句柄");
        assert_eq!(ctx.generation, 0, "纪元 fence：未接管时 manual_epoch=0");
        assert!(
            m.validate_ctx("dut-a", &ctx).is_ok(),
            "fence 必须通过 actor 复核"
        );

        // Agent 持租约 → Busy 指向持有者
        let grant = m.acquire("dut-a", &agent, None).unwrap();
        let err = m
            .admit_write("dut-a", &WriteIdentity::approval())
            .unwrap_err();
        match err {
            LeaseError::Busy { holder, .. } => assert_eq!(holder, "Agent bot"),
            other => panic!("期望 Busy，得到 {other:?}"),
        }
        // GUI 持租约 → 同样 Busy 指向 GUI
        m.force_takeover("dut-a").unwrap();
        let err = m
            .admit_write("dut-a", &WriteIdentity::approval())
            .unwrap_err();
        match err {
            LeaseError::Busy { holder, .. } => assert!(holder.contains("GUI")),
            other => panic!("期望 Busy，得到 {other:?}"),
        }
        m.release_for_holder("dut-a", &LeaseHolder::Gui);
        assert!(grant.lease_id.starts_with("lease-"));

        // 硬件动作结束前接管必须等待，之后审批写仍受人工接管限制。
        let action = m.begin_agent_action().unwrap();
        assert!(matches!(
            m.set_manual_control(true),
            Err(LeaseError::Protected { .. })
        ));
        drop(action);
        m.set_manual_control(true).unwrap();
        let err = m
            .admit_write("dut-a", &WriteIdentity::approval())
            .unwrap_err();
        match err {
            LeaseError::Busy { holder, .. } => assert_eq!(holder, "人工接管"),
            other => panic!("期望 manual Busy，得到 {other:?}"),
        }
        // 纪元 fence 在接管后失效（generation 已过时）
        assert!(m.validate_ctx("dut-a", &ctx).is_err());
    }

    /// P2-019 同族：FIFO 公平拒绝（lease=None 且队列有其他等待者的防御分支）
    /// 用 `QueuedAhead` 表述——不把队列状态词当持有者名。该状态经公开 API 不可
    /// 稳定构造（`grant_to_queue_head` 在释放/过期后立即授予或清空队列），此处
    /// 直接断言两个新变体的文案面。
    #[test]
    fn wait_timeout_and_queued_ahead_messages_are_self_explanatory() {
        let timeout = LeaseError::WaitTimeout { queue_len: 1 };
        assert_eq!(
            timeout.message(),
            "等待控制权超时：租约仍被他人持有（等待队列 1）"
        );
        let ahead = LeaseError::QueuedAhead { queue_len: 2 };
        assert!(
            ahead.message().contains("2 个请求在等待队列中") && ahead.message().contains("插队"),
            "{}",
            ahead.message()
        );
    }
}
