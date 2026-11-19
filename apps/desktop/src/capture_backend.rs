//! Capture-demand policy for the desktop capture lifecycle
//! (`docs/design/CAPTURE_STREAM_BACKENDS.md` §3.4, §4.1–§4.3, §5.4, §5.7).
//!
//! Pure logic only: no GPUI types, no process spawning, no clocks. Every
//! decision that depends on time takes the `Instant` it should use, so the
//! ledger can be driven by integration tests without sleeping. `MediaView`
//! (bin) owns the `sessions` map; this module only answers "may this capture
//! stay alive, and if not, why not?".
//!
//! Blocks:
//! - Idle clamping: `effective_idle` never returns `Duration::ZERO`; a
//!   hand-edited out-of-range `idleSecs` falls back to the default instead of
//!   turning into aggressive reclaim.
//! - ffmpeg resolution: reuses `tool_resolver`'s precedence (explicit path >
//!   downloaded install > PATH) so `media.rs` stays untouched.
//! - Artifact parsing: PNG IHDR dimensions and on-disk byte counts, both
//!   `Option` — a malformed file is reported as unknown, never invented.
//! - Error codes: the single `MEDIA_*` vocabulary of §5.7.
//! - `DemandEntry`: the per-`(dut, source)` lease ledger. `Manual` pins,
//!   `ExternalPreview`/`CopyUrl` are self-expiring holds, `Monitor`/`AgentOp`
//!   are ordinary leases, and a recording is a mirrored *state* that pins
//!   without being a lease.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use duthub_core::media_op::RecordingId;
use duthub_core::model::CaptureSettings;

use crate::tool_resolver;

// ---------------------------------------------------------------------------
// tools.capture: idle clamp and validation (§3.4)
// ---------------------------------------------------------------------------

/// Smallest accepted `idleSecs` value.
pub const IDLE_MIN_SECS: u32 = 1;
/// Largest accepted `idleSecs` value.
pub const IDLE_MAX_SECS: u32 = 3600;
/// Idle window used when `idleSecs` is absent or out of range.
pub const DEFAULT_EPHEMERAL_IDLE_SECS: u32 = 60;

/// Default hold-lease TTL (§4.1): a copied URL or an external ffplay never
/// pins the capture forever; every refresh re-arms the same window.
pub const HOLD_SECS: Duration = Duration::from_secs(300);

/// Effective idle window for ephemeral (Agent-opened) sessions.
///
/// Never returns `Duration::ZERO`. Out-of-range and `None` values fall back to
/// [`DEFAULT_EPHEMERAL_IDLE_SECS`] instead of clamping to one second: a
/// hand-edited typo must not turn into aggressive reclaim.
pub fn effective_idle(capture: &CaptureSettings) -> Duration {
    let secs = capture
        .idle_secs
        .filter(|value| (IDLE_MIN_SECS..=IDLE_MAX_SECS).contains(value))
        .unwrap_or(DEFAULT_EPHEMERAL_IDLE_SECS);
    Duration::from_secs(u64::from(secs))
}

/// Save-time gate: `Some(secs)` inside `1..=3600`, `None` otherwise.
///
/// Called by the settings action only, never from `persist_settings`: an
/// invalid idle value must not block unrelated settings from saving.
pub fn validate_idle_secs(value: u32) -> Option<u32> {
    (IDLE_MIN_SECS..=IDLE_MAX_SECS)
        .contains(&value)
        .then_some(value)
}

// ---------------------------------------------------------------------------
// ffmpeg resolution (§3.6): reuse tool_resolver, do not reinvent it
// ---------------------------------------------------------------------------

/// Same contract as `tool_resolver`'s `_in` variants: an explicit path or
/// command wins, a downloaded install shadows only the canonical bare name,
/// otherwise the configured value (which may still contain `%VAR%`) is passed
/// through for `media.rs` to expand.
///
/// `ffplay` and `listen_ip` are preserved verbatim: there is no `ffplay` tool
/// id, and the sibling-file rule stays in `media.rs`. `idle_secs` rides along
/// untouched.
pub fn resolved_capture_settings_in(
    base: &CaptureSettings,
    tools_root: Option<&Path>,
) -> CaptureSettings {
    let mut resolved = base.clone();
    resolved.ffmpeg = match tools_root {
        Some(root) => tool_resolver::effective_spec_in(&base.ffmpeg, "ffmpeg", root),
        None => tool_resolver::effective_spec(&base.ffmpeg, "ffmpeg"),
    };
    resolved
}

/// [`resolved_capture_settings_in`] anchored at
/// `tool_resolver::config_tools_root()` (the exe-sibling `.duthub/tools`).
pub fn resolved_capture_settings(base: &CaptureSettings) -> CaptureSettings {
    match tool_resolver::config_tools_root() {
        Some(root) => resolved_capture_settings_in(base, Some(&root)),
        None => resolved_capture_settings_in(base, None),
    }
}

// ---------------------------------------------------------------------------
// Artifact facts (§5.4)
// ---------------------------------------------------------------------------

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
/// Signature (8) + IHDR length (4) + chunk type (4) + width/height (8).
const PNG_HEADER_LEN: usize = 24;

/// PNG IHDR dimensions: bytes 16..20 = width, 20..24 = height (big-endian).
///
/// Returns `None` for anything that is not a PNG header with non-zero
/// dimensions (truncated, wrong signature, wrong first chunk). Callers omit
/// `width`/`height` instead of fabricating values.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < PNG_HEADER_LEN {
        return None;
    }
    if bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// Size of a regular file, or `None` when it is missing / not a file.
pub fn artifact_bytes(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
}

// ---------------------------------------------------------------------------
// Error codes (§5.7)
// ---------------------------------------------------------------------------

pub const MEDIA_NOT_BOUND: &str = "MEDIA_NOT_BOUND";
pub const MEDIA_CAPTURE_UNAVAILABLE: &str = "MEDIA_CAPTURE_UNAVAILABLE";
pub const MEDIA_START_FAILED: &str = "MEDIA_START_FAILED";
pub const MEDIA_CAPTURE_FAILED: &str = "MEDIA_CAPTURE_FAILED";
pub const MEDIA_CAPTURE_ENDED: &str = "MEDIA_CAPTURE_ENDED";
pub const MEDIA_RECORDING_ENDED: &str = "MEDIA_RECORDING_ENDED";
pub const MEDIA_RECORDING_UNKNOWN: &str = "MEDIA_RECORDING_UNKNOWN";
pub const MEDIA_BUSY: &str = "MEDIA_BUSY";
pub const MEDIA_RECORDING_IN_PROGRESS: &str = "MEDIA_RECORDING_IN_PROGRESS";
pub const MEDIA_OP_DROPPED: &str = "MEDIA_OP_DROPPED";
pub const MEDIA_OP_TIMEOUT: &str = "MEDIA_OP_TIMEOUT";

/// Every code the media-op surface may return, in §5.7 order.
///
/// There is deliberately no `MEDIA_QUOTA`: quota exhaustion is a storage
/// boundary reported as `MEDIA_CAPTURE_FAILED`, not a separate failure.
pub const MEDIA_ERROR_CODES: [&str; 11] = [
    MEDIA_NOT_BOUND,
    MEDIA_CAPTURE_UNAVAILABLE,
    MEDIA_START_FAILED,
    MEDIA_CAPTURE_FAILED,
    MEDIA_CAPTURE_ENDED,
    MEDIA_RECORDING_ENDED,
    MEDIA_RECORDING_UNKNOWN,
    MEDIA_BUSY,
    MEDIA_RECORDING_IN_PROGRESS,
    MEDIA_OP_DROPPED,
    MEDIA_OP_TIMEOUT,
];

// ---------------------------------------------------------------------------
// Demand ledger (§4.1–§4.3)
// ---------------------------------------------------------------------------

/// Correlates one lease with its holder. Chosen by the caller (`MediaView`).
pub type LeaseId = u64;

/// Who is watching a `(dut, source)` capture.
///
/// `Recording` is not a kind: a recording is a *state of the Capture*, mirrored
/// into [`DemandEntry::recording`] / [`DemandEntry::agent_recording`] on every
/// tick, so `decide` can pin without borrowing the `Capture`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandKind {
    /// Panel "start capture" — pins the session until the user stops it.
    Manual,
    /// Side-panel monitor preview; released by `close_inline_capture`.
    Monitor,
    /// External ffplay preview; a self-expiring hold (its window cannot be
    /// observed).
    ExternalPreview,
    /// Copied stream URL; a self-expiring hold, refreshed on every copy.
    CopyUrl,
    /// `media_streams.op` snapshot / recordStart.
    AgentOp,
}

impl DemandKind {
    /// Pins never expire on their own.
    pub fn is_pin(self) -> bool {
        matches!(self, Self::Manual)
    }

    /// Holds must be acquired through [`DemandEntry::acquire_hold`] so they
    /// expire even when the holder disappears silently.
    pub fn is_hold(self) -> bool {
        matches!(self, Self::ExternalPreview | Self::CopyUrl)
    }
}

/// Outcome of one reclaim evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimDecision {
    /// Someone still holds a live lease.
    Keep,
    /// Interactive session whose last lease was released: stop immediately
    /// (today's `close_inline_capture` behavior).
    StopNow,
    /// Ephemeral session whose idle window has elapsed: reclaim now.
    Reclaim,
    /// Pinned by a `Manual` lease or by an in-progress recording.
    Pinned,
    /// No leases and no prior release: there is nothing to stop or reclaim.
    NotLeased,
}

/// Per-`(dut, source)` lease ledger.
///
/// `idle_since` is the start of the ephemeral idle window. `release` cannot
/// stamp it because the design signature carries no clock, so it only marks
/// the entry as released-to-empty; [`DemandEntry::sweep_expired`] stamps the
/// instant on the next tick (it already receives `now`). Callers must run
/// `sweep_expired(now)` before `decide(now, idle)` on every tick.
#[derive(Debug, Default, Clone)]
pub struct DemandEntry {
    leases: BTreeMap<LeaseId, DemandKind>,
    /// Hold leases expire on their own so a lost ffplay/CopyUrl can never pin
    /// forever.
    holds: BTreeMap<LeaseId, Instant>,
    /// Set by `sweep_expired` once an ephemeral entry is empty and not pinned;
    /// cleared on any acquire. `None` = idle clock not running yet.
    idle_since: Option<Instant>,
    /// Set when the last lease leaves the entry (release or hold expiry).
    /// Distinguishes "was leased, now empty" (interactive → `StopNow`) from a
    /// never-leased entry (`NotLeased`).
    released_empty: bool,
    /// True when the session was created solely by an AgentOp (ephemeral).
    pub ephemeral: bool,
    /// Mirrored from `Capture::is_recording()` on every tick: a recording
    /// (manual or Agent) pins the session exactly like `DemandKind::Manual`.
    pub recording: bool,
    /// Set while an Agent recording owns the recording slot (§5.5). Holds the
    /// `RecordingId` so `recordStop` attribution and the manual-record mutex
    /// are decided from one place. Cleared by `recordStop`, `stop_capture`, or
    /// the stop grace timeout.
    pub agent_recording: Option<RecordingId>,
}

impl DemandEntry {
    /// Empty ledger; `ephemeral = true` for Agent-opened sessions.
    pub fn new(ephemeral: bool) -> Self {
        Self {
            ephemeral,
            ..Self::default()
        }
    }

    /// Take a lease that never expires on its own.
    ///
    /// `now` is accepted for symmetry with [`Self::acquire_hold`] and to keep
    /// the injected clock uniform at call sites; plain leases ignore it.
    /// Re-acquiring the same id replaces the kind and clears the idle clock
    /// (and any stale hold deadline), so repeated acquire calls are
    /// idempotent.
    pub fn acquire(&mut self, id: LeaseId, kind: DemandKind, _now: Instant) {
        self.holds.remove(&id);
        self.leases.insert(id, kind);
        self.idle_since = None;
        self.released_empty = false;
    }

    /// Take a hold lease that expires at `now + ttl`.
    ///
    /// Refreshing an existing hold id re-arms its deadline (the "copy URL
    /// again" case). Only [`DemandKind::is_hold`] kinds need this; a plain
    /// lease passed in here becomes a hold in debug builds (asserted).
    pub fn acquire_hold(&mut self, id: LeaseId, kind: DemandKind, now: Instant, ttl: Duration) {
        debug_assert!(
            kind.is_hold(),
            "acquire_hold is for self-expiring kinds only, got {kind:?}"
        );
        self.acquire(id, kind, now);
        self.holds.insert(id, now.checked_add(ttl).unwrap_or(now));
    }

    /// Release one lease by id. Unknown ids are a no-op (idempotent), which is
    /// what the idempotent `close_inline_capture` path relies on.
    pub fn release(&mut self, id: LeaseId) {
        let removed = self.leases.remove(&id).is_some();
        self.holds.remove(&id);
        if removed && self.leases.is_empty() {
            self.released_empty = true;
        }
    }

    /// Expire hold leases whose deadline is `<= now`, then advance the
    /// ephemeral idle clock.
    ///
    /// A recording keeps the session pinned; the idle window starts only after
    /// the recording is finished (§5.2), so an ongoing recording clears a stale
    /// `idle_since` instead of letting the reclaim clock run behind it.
    pub fn sweep_expired(&mut self, now: Instant) {
        let expired: Vec<LeaseId> = self
            .holds
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.release(id);
        }

        if self.recording || self.agent_recording.is_some() {
            self.idle_since = None;
            return;
        }
        if self.ephemeral && self.leases.is_empty() && self.idle_since.is_none() {
            self.idle_since = Some(now);
            self.released_empty = true;
        }
    }

    /// Whether a `Manual` lease is held. Expired holds are never pins.
    pub fn has_pin(&self) -> bool {
        self.leases.values().any(|kind| kind.is_pin())
    }

    /// Whether the raw lease map is empty. Expired-but-unswept holds still
    /// count here; `decide` is the expiry-aware query.
    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    /// Lease count that ignores holds whose deadline has already passed.
    /// `decide` must not keep a session alive for a hold that is due but not
    /// yet swept (§4.2: holds expire first, then the table is evaluated).
    fn live_lease_count(&self, now: Instant) -> usize {
        self.leases
            .keys()
            .filter(|id| self.holds.get(*id).is_none_or(|deadline| *deadline > now))
            .count()
    }

    /// Reclaim decision for this tick.
    ///
    /// Order: pin / recording first (always `Pinned`), then any live lease
    /// (`Keep`), then the empty-entry table of §4.2:
    /// - ephemeral and the idle window elapsed → `Reclaim`;
    /// - ephemeral and not elapsed (or clock not started) → `Keep`;
    /// - interactive after its last release → `StopNow`;
    /// - never leased → `NotLeased`.
    ///
    /// There is no time-based unlock for a recording: only `recordStop` or
    /// `stop_capture` clears it. Recording length itself has no upper bound.
    pub fn decide(&self, now: Instant, idle: Duration) -> ReclaimDecision {
        if self.has_pin() || self.recording || self.agent_recording.is_some() {
            return ReclaimDecision::Pinned;
        }
        if self.live_lease_count(now) > 0 {
            return ReclaimDecision::Keep;
        }
        if self.ephemeral {
            return match self.idle_since {
                Some(since) if now.saturating_duration_since(since) >= idle => {
                    ReclaimDecision::Reclaim
                }
                _ => ReclaimDecision::Keep,
            };
        }
        if self.released_empty {
            ReclaimDecision::StopNow
        } else {
            ReclaimDecision::NotLeased
        }
    }
}
