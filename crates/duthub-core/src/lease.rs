//! Control leases and write admission.
//!
//! At most one active lease per DUT. The default TTL is 120 seconds.
//! Acquiring or taking over a lease advances its fencing generation. Renewal preserves it.
//! The FIFO queue holds up to eight waiters and supports cancellation and deadlines.
//! All leases live in memory. Restarting invalidates every previously issued lease ID.
//!
//! The manager also implements the lease authority used at the device boundary.
//! Serial writes and control actions check the lease before dispatch.
//! These checks enforce GUI precedence and exclusive control.
//!
//! Check waiting requests every 200 ms without requiring a Tokio runtime in core.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::audit::{AuditOutcome, AuditSink, unix_ms};

/// Default lease lifetime.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(120);
/// Maximum number of queued waiters per DUT.
pub const QUEUE_CAP: usize = 8;
/// Wait queue polling interval.
const WAIT_POLL: Duration = Duration::from_millis(200);

/// An opaque lease handle with high entropy. It identifies a lease and may be audited.
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

/// Identity confirmed by the service that grants the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum LeaseHolder {
    /// Human GUI access. The session acquires its lease when it opens.
    Gui,
    Agent {
        agent_name: String,
        credential_id: String,
    },
}

impl LeaseHolder {
    /// Label for display and busy responses.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            LeaseHolder::Gui => "GUI（人类在场）".into(),
            LeaseHolder::Agent { agent_name, .. } => format!("Agent {agent_name}"),
        }
    }
}

/// Validate and normalize a serial occupancy session name (1..=128 chars, no control chars).
pub fn validate_session_name(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("sessionName must not be empty");
    }
    if trimmed.chars().count() > 128 {
        return Err("sessionName must be at most 128 characters");
    }
    if trimmed.chars().any(char::is_control) {
        return Err("sessionName must not contain control characters");
    }
    Ok(trimmed.to_owned())
}

/// Credentials returned when a lease is granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseGrant {
    pub lease_id: String,
    pub dut_id: String,
    pub generation: u64,
    pub expires_at_unix_ms: u64,
    /// Display label of the GUI or Agent currently holding the lease.
    pub holder: String,
    /// Registered session name occupying the serial port.
    pub session_name: String,
    /// Unix timestamp (ms) when occupancy started.
    pub acquired_at_unix_ms: u64,
}

/// Control lease errors (business layer of SPEC §6.4; HTTP 401 is handled at the transport layer).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    /// Target DUT is held by another principal.
    #[error("DUT is held by {holder} ({queue_len} queued requests)")]
    Busy { holder: String, queue_len: usize },
    /// Non-interruptible operation in progress (e.g., firmware flash).
    #[error("DUT is protected while {op} is in progress. Writes and handovers are blocked")]
    Protected { op: String },
    /// Outcome of previous protected operation is unknown; manual confirmation required.
    #[error(
        "DUT needs operator inspection because the previous protected operation had an unknown outcome"
    )]
    NeedsInspection,
    /// Lease exists but has expired.
    #[error("Control lease expired. Acquire a new lease")]
    Expired,
    /// Lease does not exist (expired, revoked, or process restarted).
    #[error("Unknown control lease. It may have expired, been revoked, or been lost on restart")]
    UnknownLease,
    /// Caller does not own the referenced lease.
    #[error("The requester does not hold the control lease")]
    NotHolder,
    /// Caller is not authorized to acquire a lease.
    #[error("Not authorized to acquire the control lease")]
    Unauthorized,
    /// Stale fencing generation.
    #[error("Stale write generation (current generation {current})")]
    Generation { current: u64 },
    /// Write attempted without a lease while authentication is enabled.
    #[error("Acquire a control lease with dut_acquire_control before writing")]
    LeaseRequired,
    /// FIFO wait timed out while another holder kept the lease.
    #[error("Timed out waiting for the control lease (queue length: {queue_len})")]
    WaitTimeout { queue_len: usize },
    /// FIFO fairness rejection: other waiters are already queued ahead.
    #[error(
        "DUT is free but {queue_len} requests are queued ahead. Use wait=true to join the queue"
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

/// Identity presented at write admission.
///
/// The device service acquires the GUI lease when opening a session.
/// The manager checks GUI ownership before admitting human input.
/// Agents present a lease ID and generation. With authentication off, the compatibility path
/// allows an unleased write only when no existing lease prevents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteIdentity {
    Gui,
    Agent {
        lease_id: Option<String>,
        generation: Option<u64>,
    },
    /// Internal work such as heartbeats, suppressed under Agent control or while protected.
    System,
    /// A write authorized by a single-use approval token checked by MCP.
    /// Approval grants access only when the DUT is free.
    /// The queue records its generation even without a lease ID.
    /// Existing leases, manual takeover, protected work, and pending inspection block approval writes.
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

/// Admission captures the lease ID and generation in the command context.
/// The serial actor checks them again immediately before writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteCtx {
    pub lease_id: String,
    pub generation: u64,
}

/// An admission result can include a short lease acquired for this control action.
/// Release it with `LeaseAuthority::release_temporary` whether the action succeeds or fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlAdmission {
    pub ctx: Option<WriteCtx>,
    pub temp_lease: Option<LeaseGrant>,
}

/// Lease checks shared by the serial and control entry points.
pub trait LeaseAuthority: Send + Sync {
    fn serial_io(&self) -> &tokio::sync::RwLock<()>;
    /// `Ok(None)` admits a write whose path does not require a lease.
    /// `Ok(Some(ctx))` requires the actor to recheck the attached context. An error denies the write.
    fn admit_write(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
    ) -> Result<Option<WriteCtx>, LeaseError>;

    /// Control admission uses the same rules as write admission.
    /// A GUI action on a free DUT can acquire a short lease that is released afterward.
    fn admit_control(
        &self,
        dut_id: &str,
        writer: &WriteIdentity,
    ) -> Result<ControlAdmission, LeaseError>;

    /// Immediately before serial I/O, validate that the lease ID and generation still match.
    fn validate_ctx(&self, dut_id: &str, ctx: &WriteCtx) -> Result<(), LeaseError>;

    /// Release the temporary lease. Expired or already released leases need no cleanup.
    fn release_temporary(&self, dut_id: &str, ctx: &WriteCtx);
}

/// Lease phase used by the control status tool and GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeasePhase {
    Free,
    Held,
    Protected,
    NeedsInspection,
}

/// Snapshot of the DUT control state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseStatus {
    pub dut_id: String,
    pub phase: LeasePhase,
    pub holder: Option<String>,
    /// Registered session name currently occupying the DUT serial port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    /// Unix timestamp (ms) when the current holder began occupying the DUT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquired_at_unix_ms: Option<u64>,
    /// Elapsed occupancy duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupied_secs: Option<u64>,
    pub generation: u64,
    pub remaining_secs: Option<u64>,
    pub queue_len: usize,
    pub protected_op: Option<String>,
    /// Basename of the firmware image currently being flashed during `LeasePhase::Protected`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_file: Option<String>,
}

#[derive(Debug)]
struct HeldLease {
    lease_id: String,
    holder: LeaseHolder,
    session_name: String,
    acquired_at_ms: u64,
    /// Request ID that won the queue, or zero for an immediate grant.
    request_id: u64,
    expires_at_ms: u64,
    revoked: bool,
}

#[derive(Debug)]
struct Waiter {
    request_id: u64,
    holder: LeaseHolder,
    session_name: String,
    deadline_ms: u64,
}

#[derive(Debug, Default)]
struct DutState {
    generation: u64,
    lease: Option<HeldLease>,
    protected_op: Option<String>,
    protected_file: Option<String>,
    inspection: bool,
    queue: VecDeque<Waiter>,
    cancelled_waiters: HashSet<u64>,
}

/// Manage exclusive leases, FIFO waiters, generation fencing, and protected operations.
///
/// State lives in memory. The audit sink receives redacted events.
pub struct ControlLeaseManager {
    states: Mutex<HashMap<String, DutState>>,
    gui_session_names: Mutex<HashMap<String, String>>,
    manual_epoch: AtomicU64,
    manual_started_at_ms: AtomicU64,
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
    /// Create the production manager with a 120-second TTL and a 200 ms polling interval.
    #[must_use]
    pub fn new(audit: Option<Arc<AuditSink>>) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            gui_session_names: Mutex::new(HashMap::new()),
            manual_epoch: AtomicU64::new(0),
            manual_started_at_ms: AtomicU64::new(0),
            device_action: tokio::sync::RwLock::new(()),
            ttl_ms: AtomicU64::new(
                u64::try_from(DEFAULT_LEASE_TTL.as_millis()).unwrap_or(u64::MAX),
            ),
            gui_session_bound: false,
            access: std::sync::OnceLock::new(),
            wait_poll: WAIT_POLL,
            auth_enabled: AtomicBool::new(false),
            request_seq: AtomicU64::new(0),
            audit,
        }
    }

    /// Register the session name associated with a desktop GUI serial session on `dut_id`.
    pub fn set_gui_session_name(&self, dut_id: &str, session_name: impl Into<String>) {
        let raw = session_name.into();
        let normalized = validate_session_name(&raw).unwrap_or_else(|_| format!("GUI/{dut_id}"));
        crate::util::lock_unpoisoned(&self.gui_session_names)
            .insert(dut_id.to_owned(), normalized.clone());
        let mut states = crate::util::lock_unpoisoned(&self.states);
        if let Some(st) = states.get_mut(dut_id)
            && let Some(held) = st.lease.as_mut()
            && held.holder == LeaseHolder::Gui
        {
            held.session_name = normalized;
        }
    }

    fn default_session_name(&self, dut_id: &str, holder: &LeaseHolder) -> String {
        match holder {
            LeaseHolder::Gui => crate::util::lock_unpoisoned(&self.gui_session_names)
                .get(dut_id)
                .cloned()
                .unwrap_or_else(|| format!("GUI/{dut_id}")),
            LeaseHolder::Agent { agent_name, .. } => agent_name.clone(),
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

    /// Use a shorter polling interval for wait queue tests.
    #[must_use]
    pub fn with_wait_poll(mut self, poll: Duration) -> Self {
        self.wait_poll = poll;
        self
    }

    /// Update authentication policy for admitting unleased Agent writes after settings are saved.
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
                op: "device write or control action".into(),
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
        self.manual_started_at_ms
            .store(if enabled { unix_ms() } else { 0 }, Ordering::Release);
        self.manual_epoch.fetch_add(1, Ordering::Release);
        self.audit_lease(
            "lease.manual",
            Some(&LeaseHolder::Gui),
            "",
            AuditOutcome::Ok,
            if enabled {
                "Manual control enabled"
            } else {
                "Manual control disabled"
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
    /// An approved write also needs a generation fence even when it has no lease.
    /// Record the current generation with an empty lease ID for actor-side validation.
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

    /// Grant immediately when free and advance the generation.
    /// An eligible waiting request enters the FIFO queue. Existing GUI grants are idempotent.
    pub fn acquire(
        &self,
        dut_id: &str,
        holder: &LeaseHolder,
        wait: Option<Duration>,
    ) -> Result<LeaseGrant, LeaseError> {
        let session_name = self.default_session_name(dut_id, holder);
        self.acquire_with_session(dut_id, holder, &session_name, wait)
    }

    /// Grant immediately when free with an explicit registered `session_name`.
    pub fn acquire_with_session(
        &self,
        dut_id: &str,
        holder: &LeaseHolder,
        session_name: &str,
        wait: Option<Duration>,
    ) -> Result<LeaseGrant, LeaseError> {
        let session_name = validate_session_name(session_name)
            .unwrap_or_else(|_| self.default_session_name(dut_id, holder));
        let deadline_ms = wait
            .map(|w| unix_ms().saturating_add(u64::try_from(w.as_millis()).unwrap_or(u64::MAX)));
        let request_id =
            deadline_ms.map_or(0, |_| self.request_seq.fetch_add(1, Ordering::Relaxed) + 1);
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
                // Protected and inspection states deny acquisition, including for queued waiters.
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
                        // Return the lease if the queued request was granted since the last poll.
                        return Ok(Self::grant_view(dut_id, held, st.generation));
                    }
                    if *holder == LeaseHolder::Gui && held.holder == LeaseHolder::Gui {
                        // Return the existing grant for repeated acquisition by the GUI holder.
                        return Ok(Self::grant_view(dut_id, held, st.generation));
                    }
                    let busy = LeaseError::Busy {
                        holder: held.holder.label(),
                        queue_len: st.queue.len(),
                    };
                    if deadline_ms.is_none() {
                        return Err(busy);
                    }
                    // Enforce the queue limit before registering the waiter.
                    if st.queue.len() >= QUEUE_CAP {
                        return Err(busy);
                    }
                    if !st.queue.iter().any(|w| w.request_id == request_id) {
                        st.queue.push_back(Waiter {
                            request_id,
                            holder: holder.clone(),
                            session_name: session_name.clone(),
                            deadline_ms: deadline_ms.unwrap_or(now),
                        });
                        self.audit_lease(
                            "lease.queue",
                            Some(holder),
                            dut_id,
                            AuditOutcome::Ok,
                            &format!("Queued at position {}", st.queue.len()),
                        );
                    }
                } else if st.queue.iter().any(|w| &w.holder != holder) {
                    // Preserve FIFO order when other waiters are queued.
                    // Queue position uses its own error variant because it has no holder identity.
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
                            session_name: session_name.clone(),
                            deadline_ms: deadline_ms.unwrap_or(now),
                        });
                    }
                } else {
                    // Grant immediately when the DUT is free.
                    return Ok(self.grant_free(st, dut_id, holder, &session_name, now, 0));
                }
                // Check the deadline before sleeping outside the lock.
                let deadline = deadline_ms.unwrap_or(now);
                if now >= deadline {
                    Self::remove_waiter(st, request_id);
                    self.audit_lease(
                        "lease.queue",
                        Some(holder),
                        dut_id,
                        AuditOutcome::Denied,
                        "Wait timed out",
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

    /// Renew the TTL while preserving the fencing generation.
    /// Reject expired, unknown, and protected leases with their corresponding errors.
    pub fn renew(&self, dut_id: &str, lease_id: &str) -> Result<LeaseGrant, LeaseError> {
        self.renew_with_clock(dut_id, lease_id, unix_ms)
    }

    fn renew_with_clock(
        &self,
        dut_id: &str,
        lease_id: &str,
        now_ms: impl FnOnce() -> u64,
    ) -> Result<LeaseGrant, LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return Err(LeaseError::UnknownLease);
        };
        let now = now_ms();
        if let Some(op) = &st.protected_op {
            return Err(LeaseError::Protected { op: op.clone() });
        }
        if st.inspection {
            return Err(LeaseError::NeedsInspection);
        }
        let Some(held) = st.lease.as_mut().filter(|held| held.lease_id == lease_id) else {
            return Err(LeaseError::UnknownLease);
        };
        if now > held.expires_at_ms {
            self.expire_if_due(st, dut_id, now);
            return Err(LeaseError::Expired);
        }
        if !self.holder_enabled(&held.holder) || held.revoked {
            return Err(LeaseError::Unauthorized);
        }
        if held.expires_at_ms == u64::MAX {
            return Ok(Self::grant_view(dut_id, held, st.generation));
        }
        let ttl = self.ttl_ms.load(Ordering::Relaxed);
        held.expires_at_ms = now.saturating_add(ttl);
        self.audit_lease(
            "lease.renew",
            Some(&held.holder),
            dut_id,
            AuditOutcome::Ok,
            &format!("Renewed for {ttl} ms"),
        );
        Ok(Self::grant_view(dut_id, held, st.generation))
    }

    /// Release a matching lease. Protected operations prohibit handover.
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
                    "Holder released the lease",
                );
                self.grant_to_queue_head(st, dut_id, now);
                Ok(())
            }
            Some(_) => Err(LeaseError::NotHolder),
            None => Err(LeaseError::UnknownLease),
        }
    }

    /// Check lease ownership and generation for MCP writes.
    /// The lease ID must match, and the authenticated credential must own the Agent lease.
    /// Return the current generation for the command, or an ownership error if the lease is absent or belongs to another holder.
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

    /// Release a holder's lease when its GUI session closes. A missing lease needs no action.
    pub fn release_for_holder(&self, dut_id: &str, holder: &LeaseHolder) {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return;
        };
        let now = unix_ms();
        if st.protected_op.is_some() || st.inspection {
            return; // Do not hand over during protected work or pending inspection.
        }
        let matches = st.lease.as_ref().is_some_and(|held| &held.holder == holder);
        if matches {
            st.lease = None;
            self.audit_lease(
                "lease.release",
                Some(holder),
                dut_id,
                AuditOutcome::Ok,
                "Session closed and released the lease",
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
                    "Credential disabled or revoked",
                );
                self.expire_if_due(state, dut_id, now);
            }
        }
    }

    /// Return current control status and process lazy expiration.
    pub fn status(&self, dut_id: &str) -> LeaseStatus {
        self.status_with_clock(dut_id, unix_ms)
    }

    fn status_with_clock(&self, dut_id: &str, now_ms: impl FnOnce() -> u64) -> LeaseStatus {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let now = now_ms();
        let Some(st) = states.get_mut(dut_id) else {
            if self.manual_control() {
                let started = self.manual_started_at_ms.load(Ordering::Acquire);
                let acquired = (started > 0).then_some(started);
                let occupied = acquired.map(|ts| now.saturating_sub(ts) / 1000);
                return LeaseStatus {
                    dut_id: dut_id.to_string(),
                    phase: LeasePhase::Held,
                    holder: Some("人工接管".into()),
                    session_name: Some("人工接管".into()),
                    acquired_at_unix_ms: acquired,
                    occupied_secs: occupied,
                    generation: 0,
                    remaining_secs: None,
                    queue_len: 0,
                    protected_op: None,
                    protected_file: None,
                };
            }
            return LeaseStatus {
                dut_id: dut_id.to_string(),
                phase: LeasePhase::Free,
                holder: None,
                session_name: None,
                acquired_at_unix_ms: None,
                occupied_secs: None,
                generation: 0,
                remaining_secs: None,
                queue_len: 0,
                protected_op: None,
                protected_file: None,
            };
        };
        self.expire_if_due(st, dut_id, now);
        let (phase, holder, session_name, acquired_at_unix_ms, remaining) = match (
            st.protected_op.is_some(),
            st.inspection,
            self.manual_control(),
            &st.lease,
        ) {
            (true, _, _, lease) => (
                LeasePhase::Protected,
                lease.as_ref().map(|h| h.holder.label()),
                lease.as_ref().map(|h| h.session_name.clone()),
                lease.as_ref().map(|h| h.acquired_at_ms),
                None,
            ),
            (false, true, _, _) => (LeasePhase::NeedsInspection, None, None, None, None),
            (false, false, true, _) => {
                let started = self.manual_started_at_ms.load(Ordering::Acquire);
                (
                    LeasePhase::Held,
                    Some("人工接管".into()),
                    Some("人工接管".into()),
                    (started > 0).then_some(started),
                    None,
                )
            }
            (false, false, false, Some(held)) => {
                // Explicit session-bound GUI leases have no finite TTL to display.
                let remaining = (held.expires_at_ms != u64::MAX)
                    .then(|| held.expires_at_ms.saturating_sub(now) / 1000);
                (
                    LeasePhase::Held,
                    Some(held.holder.label()),
                    Some(held.session_name.clone()),
                    Some(held.acquired_at_ms),
                    remaining,
                )
            }
            (false, false, false, None) => (LeasePhase::Free, None, None, None, None),
        };
        let occupied_secs = acquired_at_unix_ms.map(|ts| now.saturating_sub(ts) / 1000);
        LeaseStatus {
            dut_id: dut_id.to_string(),
            phase,
            holder,
            session_name,
            acquired_at_unix_ms,
            occupied_secs,
            generation: st.generation,
            remaining_secs: remaining,
            queue_len: st.queue.len(),
            protected_op: st.protected_op.clone(),
            protected_file: st.protected_file.clone(),
        }
    }

    /// Check the lease ID and generation at admission and immediately before dispatch.
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

    /// Enter the protected phase for an uninterruptible operation and block lease changes and writes.
    /// A current lease is required.
    pub fn enter_protected(&self, dut_id: &str, op: &str) -> Result<(), LeaseError> {
        self.enter_protected_with_file(dut_id, op, None)
    }

    /// Enter the protected phase while recording the canonical firmware image basename being
    /// flashed, so concurrent uploads targeting *different* files can proceed safely while
    /// writes/overwrites to the same active image file are rejected.
    pub fn enter_protected_with_file(
        &self,
        dut_id: &str,
        op: &str,
        file_name: Option<&str>,
    ) -> Result<(), LeaseError> {
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
        st.protected_file = file_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|raw| raw.rsplit(['/', '\\']).next().unwrap_or(raw).to_owned());
        self.audit_lease(
            "lease.protected.enter",
            Some(&holder),
            dut_id,
            AuditOutcome::Ok,
            &format!("Entered protected state for {op}"),
        );
        Ok(())
    }

    /// Return to free state after verified success. An unknown outcome requires inspection.
    /// Use `Self::resolve_inspection` to return to free state after manual review.
    pub fn exit_protected(&self, dut_id: &str, outcome_ok: bool) -> Result<(), LeaseError> {
        let mut states = crate::util::lock_unpoisoned(&self.states);
        let Some(st) = states.get_mut(dut_id) else {
            return Err(LeaseError::UnknownLease);
        };
        let now = unix_ms();
        let Some(op) = st.protected_op.take() else {
            return Err(LeaseError::UnknownLease);
        };
        st.protected_file = None;
        st.lease = None; // A protected operation never restores the previous grant.
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
                "Exited protected state for {op} (outcome: {})",
                if outcome_ok { "success" } else { "unknown" }
            ),
        );
        if outcome_ok {
            self.grant_to_queue_head(st, dut_id, now);
        } else {
            st.inspection = true;
        }
        Ok(())
    }

    /// Return the DUT to free state after manual inspection.
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
                "Operator inspection completed. DUT available",
            );
            self.grant_to_queue_head(st, dut_id, now);
        }
    }

    /// Revoke the old grant and advance the generation before giving control to the GUI.
    /// Reject takeover while protected or awaiting inspection. Later writes from the old holder fail admission.
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
                &format!(
                    "GUI took control and revoked the lease held by {}",
                    held.holder.label()
                ),
            );
        }
        st.lease = None;
        st.queue.clear(); // The human takeover supersedes queued requests.
        self.audit_lease(
            "lease.queue",
            None,
            dut_id,
            AuditOutcome::Ok,
            "Manual takeover cleared the wait queue",
        );
        let gui_session = self.default_session_name(dut_id, &LeaseHolder::Gui);
        Ok(self.grant_free(st, dut_id, &LeaseHolder::Gui, &gui_session, now, 0))
    }

    /// Cancel a queued control acquisition request.
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
                &format!("Cancelled queued request {request_id}"),
            );
        }
        removed
    }

    /// List queued request IDs for the GUI panel and cancellation tool.
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

    /// On expiration, record the release and grant the next eligible FIFO waiter.
    fn expire_if_due(&self, st: &mut DutState, dut_id: &str, now: u64) {
        if st.protected_op.is_some() || st.inspection {
            return; // Skip lazy expiration during protected work or pending inspection.
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
                .as_ref()
                .map_or_else(|| "?".into(), LeaseHolder::label);
            self.audit_lease(
                "lease.expire",
                holder.as_ref(),
                dut_id,
                AuditOutcome::Ok,
                &format!("Lease for {label} expired"),
            );
            self.grant_to_queue_head(st, dut_id, now);
        }
    }

    /// Grant to the first eligible waiter and audit any expired entries that are skipped.
    fn grant_to_queue_head(&self, st: &mut DutState, dut_id: &str, now: u64) {
        while let Some(head) = st.queue.front() {
            if !self.holder_enabled(&head.holder) {
                st.queue.pop_front();
                continue;
            }
            if head.deadline_ms < now {
                let skipped = st
                    .queue
                    .pop_front()
                    .expect("queue front was just inspected");
                self.audit_lease(
                    "lease.queue",
                    Some(&skipped.holder),
                    dut_id,
                    AuditOutcome::Denied,
                    "Skipped waiter whose deadline passed",
                );
                continue;
            }
            let head = st
                .queue
                .pop_front()
                .expect("queue front was just inspected");
            self.grant_free(
                st,
                dut_id,
                &head.holder,
                &head.session_name,
                now,
                head.request_id,
            );
            return;
        }
    }

    /// Advance the generation for a grant. A nonzero request ID identifies a queued grant.
    fn grant_free(
        &self,
        st: &mut DutState,
        dut_id: &str,
        holder: &LeaseHolder,
        session_name: &str,
        now: u64,
        request_id: u64,
    ) -> LeaseGrant {
        st.generation = st.generation.saturating_add(1);
        let lease_id = LeaseId::new();
        // Automatic GUI leases are exempt from expiration checks.
        // Use the maximum timestamp so their remaining lifetime cannot be mistaken for a finite TTL.
        let expires_at = if self.gui_session_bound && *holder == LeaseHolder::Gui {
            u64::MAX
        } else {
            now.saturating_add(self.ttl_ms.load(Ordering::Relaxed))
        };
        st.lease = Some(HeldLease {
            lease_id: lease_id.to_string(),
            holder: holder.clone(),
            session_name: session_name.to_owned(),
            acquired_at_ms: now,
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
                "Granted lease to {} (session: {session_name}) at generation {} (from queue: {})",
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
            session_name: session_name.to_owned(),
            acquired_at_unix_ms: now,
        }
    }

    fn grant_view(dut_id: &str, held: &HeldLease, generation: u64) -> LeaseGrant {
        LeaseGrant {
            lease_id: held.lease_id.clone(),
            dut_id: dut_id.to_string(),
            generation,
            expires_at_unix_ms: held.expires_at_ms,
            holder: held.holder.label(),
            session_name: held.session_name.clone(),
            acquired_at_unix_ms: held.acquired_at_ms,
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
                    // Suppress the heartbeat while an Agent has control.
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
                match &mut st.lease {
                    Some(held) if held.holder == LeaseHolder::Gui => {
                        if held.expires_at_ms != u64::MAX {
                            held.expires_at_ms =
                                now.saturating_add(self.ttl_ms.load(Ordering::Relaxed));
                        }
                        Ok(Some(WriteCtx {
                            lease_id: held.lease_id.clone(),
                            generation: st.generation,
                        }))
                    }
                    Some(held) => Err(LeaseError::Busy {
                        holder: held.holder.label(),
                        queue_len: st.queue.len(),
                    }),
                    None => {
                        // Acquire a finite-TTL GUI lease while the human is actively typing on
                        // a free DUT so Agents are not blocked indefinitely after idle input.
                        let gui_session = self.default_session_name(dut_id, &LeaseHolder::Gui);
                        let grant =
                            self.grant_free(st, dut_id, &LeaseHolder::Gui, &gui_session, now, 0);
                        let finite_expiry = now.saturating_add(self.ttl_ms.load(Ordering::Relaxed));
                        if let Some(held) = st.lease.as_mut() {
                            held.expires_at_ms = finite_expiry;
                        }
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
                // An existing lease rejects Agent writes from other holders.
                // This also applies to unleased writes in compatibility mode.
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
                            // Fence the current holder against an obsolete generation.
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
                        // The supplied handle was expired, revoked, or invalidated by a restart.
                        Some(_) => Err(LeaseError::UnknownLease),
                        // Authentication must be disabled to admit an unleased write to a free DUT.
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
                    // An approval is independent of the lease and cannot act while any lease is active.
                    // Report the actual holder in the busy response.
                    Some(held) => Err(LeaseError::Busy {
                        holder: held.holder.label(),
                        queue_len: st.queue.len(),
                    }),
                    // Admit approved writes on a free DUT without requiring a lease.
                    // Queue admission adds a generation context with an empty lease ID.
                    // The actor checks that context just like the unleased Agent compatibility path.
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
                        // Acquire a temporary GUI lease and release it after this action.
                        let gui_session = self.default_session_name(dut_id, &LeaseHolder::Gui);
                        let grant =
                            self.grant_free(st, dut_id, &LeaseHolder::Gui, &gui_session, now, 0);
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
                // Use write admission for Agent and system actions.
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
        // Release is idempotent after expiration or takeover.
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
        let manual_status = manager.status("dut-b");
        assert_eq!(manual_status.session_name.as_deref(), Some("人工接管"));
        assert!(manual_status.acquired_at_unix_ms.is_some());
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

    #[test]
    fn lease_records_session_name_and_acquired_timestamp() {
        let manager = manager();
        let agent = LeaseHolder::Agent {
            agent_name: "planner".into(),
            credential_id: "cred-1".into(),
        };
        assert!(validate_session_name("   ").is_err());
        assert!(validate_session_name("bad\nname").is_err());
        assert_eq!(
            validate_session_name("  stress-run-01 ").unwrap(),
            "stress-run-01"
        );

        let grant = manager
            .acquire_with_session("dut-1", &agent, "stress-run-01", None)
            .unwrap();
        assert_eq!(grant.session_name, "stress-run-01");
        assert!(grant.acquired_at_unix_ms > 0);

        let status = manager.status_with_clock("dut-1", || grant.acquired_at_unix_ms + 12_500);
        assert_eq!(status.phase, LeasePhase::Held);
        assert_eq!(status.session_name.as_deref(), Some("stress-run-01"));
        assert_eq!(status.acquired_at_unix_ms, Some(grant.acquired_at_unix_ms));
        assert_eq!(status.occupied_secs, Some(12));

        manager.release("dut-1", &grant.lease_id).unwrap();
        let free = manager.status("dut-1");
        assert_eq!(free.phase, LeasePhase::Free);
        assert_eq!(free.session_name, None);
        assert_eq!(free.acquired_at_unix_ms, None);
        assert_eq!(free.occupied_secs, None);
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
        // A second holder is refused and the response identifies the first one.
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
            other => panic!("Expected Busy, got {other:?}"),
        }
        // Renewal preserves the generation.
        let g1r = m.renew("dut", &g1.lease_id).unwrap();
        assert_eq!(g1r.generation, 1);
        assert!(g1r.expires_at_unix_ms >= g1.expires_at_unix_ms);
        // Releasing and acquiring again increments the generation.
        m.release("dut", &g1.lease_id).unwrap();
        let g2 = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        assert_eq!(g2.generation, 2);
        assert_ne!(g2.lease_id, g1.lease_id);
        // The old holder no longer owns the current grant.
        assert!(matches!(
            m.validate("dut", &g1.lease_id, 1),
            Err(LeaseError::NotHolder)
        ));
    }

    #[test]
    fn ttl_expiry_is_lazy_and_renew_of_expired_is_expired() {
        let m =
            ControlLeaseManager::new(Some(AuditSink::memory())).with_ttl(Duration::from_millis(60));
        // Use an Agent lease to test lazy TTL expiration.
        // A separate test covers the automatic GUI lease exemption.
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
        let expiry = g.expires_at_unix_ms;
        let status_held = m.status_with_clock("dut", || expiry - 1);
        assert_eq!(status_held.phase, LeasePhase::Held);

        // Renewal detects expiration even if no status read has processed it.
        let after_expiry = || expiry + 1;
        assert_eq!(
            m.renew_with_clock("dut", &g.lease_id, after_expiry),
            Err(LeaseError::Expired)
        );
        let status = m.status_with_clock("dut", after_expiry);
        assert_eq!(status.phase, LeasePhase::Free);
        // After cleanup, the expired handle no longer identifies a lease.
        assert_eq!(
            m.validate("dut", &g.lease_id, g.generation),
            Err(LeaseError::UnknownLease)
        );
    }

    /// Automatic GUI leases stay held beyond the normal TTL.
    /// Agent acquisition remains blocked while the human operator holds the lease.
    #[test]
    fn gui_lease_does_not_expire_and_keeps_blocking_agent() {
        let m = ControlLeaseManager::new(Some(AuditSink::memory()))
            .with_ttl(Duration::from_millis(60))
            .with_gui_session_lifetime();
        let grant = m.acquire("dut", &LeaseHolder::Gui, None).unwrap();
        // The GUI grant uses the maximum expiry timestamp.
        assert_eq!(grant.expires_at_unix_ms, u64::MAX);
        std::thread::sleep(Duration::from_millis(90));
        // Repeated status checks must leave the GUI grant active.
        let status = m.status("dut");
        assert_eq!(status.phase, LeasePhase::Held);
        assert_eq!(status.holder.as_deref(), Some("GUI（人类在场）"));
        assert_eq!(status.remaining_secs, None);
        // The GUI still takes precedence after the ordinary TTL has elapsed.
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
        // GUI writes remain admitted.
        assert!(m.admit_write("dut", &WriteIdentity::gui()).is_ok());
        // Explicitly releasing the GUI grant frees the DUT.
        m.release_for_holder("dut", &LeaseHolder::Gui);
        assert_eq!(m.status("dut").phase, LeasePhase::Free);
    }

    #[test]
    fn implicit_gui_write_lease_expires_after_ttl_so_agents_can_resume() {
        let m = ControlLeaseManager::new(Some(AuditSink::memory()))
            .with_ttl(Duration::from_millis(60))
            .with_gui_session_lifetime();
        // Typing on a free DUT grants a finite-TTL GUI lease rather than blocking Agents forever.
        let ctx = m
            .admit_write("dut", &WriteIdentity::gui())
            .expect("GUI write admitted")
            .expect("returns lease context");
        let status = m.status("dut");
        assert_eq!(status.phase, LeasePhase::Held);
        assert!(
            status.remaining_secs.is_some(),
            "implicit GUI typing lease must expose a finite TTL"
        );
        std::thread::sleep(Duration::from_millis(90));
        assert_eq!(m.status("dut").phase, LeasePhase::Free);
        // Once idle TTL elapses, an Agent can acquire control immediately.
        let agent_grant = m
            .acquire(
                "dut",
                &LeaseHolder::Agent {
                    agent_name: "bot".into(),
                    credential_id: "cred".into(),
                },
                None,
            )
            .expect("agent acquires after implicit GUI write TTL expires");
        assert!(agent_grant.generation > ctx.generation);
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
        // Queue two waiters in FIFO order.
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
        // Releasing the lease grants the first waiter.
        let ga = h.join().unwrap().unwrap();
        assert_eq!(ga.holder, "Agent bot-a");
        m.release("dut", &ga.lease_id).unwrap();
        let gb = h2.join().unwrap().unwrap();
        assert_eq!(gb.holder, "Agent bot-b");
        assert!(
            ga.generation < gb.generation,
            "generation must strictly increase across grants"
        );
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
        // Distinguish a queue deadline from a busy holder.
        assert!(matches!(err, LeaseError::WaitTimeout { .. }), "{err:?}");
        assert!(
            err.message()
                .contains("Timed out waiting for the control lease"),
            "error message must indicate wait timeout: {}",
            err.message()
        );
        assert!(
            started.elapsed() >= Duration::from_millis(70),
            "must wait until the deadline"
        );
        // Cancel the waiter using the request ID returned by the queue listing.
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
            .expect("pending waiter should expose a request_id");
        assert!(
            m.cancel_wait("dut", request_id),
            "cancel_wait must remove the queued waiter"
        );
        assert_eq!(m.status("dut").queue_len, 0);
        // A cancelled acquisition exits without receiving a grant.
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
        // Successful completion frees the DUT. Uncertain completion requires manual inspection first.
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
        // A fresh manager rejects handles from before a restart.
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
        // An Agent lease prevents GUI writes until control changes hands.
        assert!(matches!(
            m.admit_write("dut", &WriteIdentity::gui()),
            Err(LeaseError::Busy { .. })
        ));
        let g_gui = m.force_takeover("dut").unwrap();
        assert!(g_gui.generation > g_agent.generation);
        // After takeover, the prior holder's write reports the current GUI holder.
        assert!(matches!(
            m.admit_write(
                "dut",
                &WriteIdentity::agent(Some(g_agent.lease_id), Some(g_agent.generation))
            ),
            Err(LeaseError::Busy { .. })
        ));
        // The GUI can now write.
        assert!(m.admit_write("dut", &WriteIdentity::gui()).is_ok());
    }

    #[test]
    fn auth_enabled_requires_agent_lease_and_two_duts_are_independent() {
        let m = manager();
        m.set_auth_enabled(true);
        // With authentication on, an Agent must provide a lease.
        assert!(matches!(
            m.admit_write("dut-a", &WriteIdentity::agent(None, None)),
            Err(LeaseError::LeaseRequired)
        ));
        m.set_auth_enabled(false);
        // With authentication off, an unleased write uses the compatibility path.
        assert_eq!(
            m.admit_write("dut-a", &WriteIdentity::agent(None, None)),
            Ok(None)
        );
        // Separate DUTs can be controlled independently.
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
        // The active Agent can write. GUI input and the heartbeat are blocked.
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
        // Reject an obsolete generation even for the current holder.
        assert!(matches!(
            m.admit_write("dut", &WriteIdentity::agent(Some(g.lease_id), Some(999))),
            Err(LeaseError::Generation { .. })
        ));
        // A GUI lease allows the system heartbeat.
        m.force_takeover("dut").unwrap();
        assert_eq!(m.admit_write("dut", &WriteIdentity::system()), Ok(None));
    }

    /// Approved writes to a free DUT are fenced by generation without a lease ID.
    /// Any active holder or manual takeover blocks approval writes.
    #[test]
    fn approval_identity_admission_rules() {
        let m = manager();
        let agent = LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred".into(),
        };
        // Queue admission adds a generation fence to the free-state write.
        let ctx = m
            .admit_queued_write("dut-a", &WriteIdentity::approval())
            .unwrap()
            .expect("free state must return a fence context");
        assert_eq!(ctx.lease_id, "", "approval write has no lease handle");
        assert_eq!(
            ctx.generation, 0,
            "epoch fence: manual_epoch must be 0 before any takeover"
        );
        assert!(
            m.validate_ctx("dut-a", &ctx).is_ok(),
            "fence must pass actor revalidation"
        );

        // An active Agent lease is reported as busy with the Agent holder.
        let grant = m.acquire("dut-a", &agent, None).unwrap();
        let err = m
            .admit_write("dut-a", &WriteIdentity::approval())
            .unwrap_err();
        match err {
            LeaseError::Busy { holder, .. } => assert_eq!(holder, "Agent bot"),
            other => panic!("Expected Busy, got {other:?}"),
        }
        // An active GUI lease is likewise reported as busy with that holder.
        m.force_takeover("dut-a").unwrap();
        let err = m
            .admit_write("dut-a", &WriteIdentity::approval())
            .unwrap_err();
        match err {
            LeaseError::Busy { holder, .. } => assert!(holder.contains("GUI")),
            other => panic!("Expected Busy, got {other:?}"),
        }
        m.release_for_holder("dut-a", &LeaseHolder::Gui);
        assert!(grant.lease_id.starts_with("lease-"));

        // Takeover waits for in-flight hardware work. Manual control then blocks approval writes.
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
            other => panic!("Expected Busy during manual control, got {other:?}"),
        }
        // A takeover invalidates the earlier generation fence.
        assert!(m.validate_ctx("dut-a", &ctx).is_err());
    }

    /// A free lease slot can still have waiters, so fairness errors use their own variant.
    /// `QueuedAhead` carries queue state without implying a current holder.
    /// `grant_to_queue_head` normally resolves that intermediate state before callers observe it.
    /// Check the wording of both queue errors directly.
    #[test]
    fn wait_timeout_and_queued_ahead_messages_are_self_explanatory() {
        let timeout = LeaseError::WaitTimeout { queue_len: 1 };
        assert_eq!(
            timeout.message(),
            "Timed out waiting for the control lease (queue length: 1)"
        );
        let ahead = LeaseError::QueuedAhead { queue_len: 2 };
        assert!(
            ahead.message().contains("2 requests are queued ahead")
                && ahead.message().contains("wait=true"),
            "{}",
            ahead.message()
        );
    }
}
