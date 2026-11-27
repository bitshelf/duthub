//! MCP tool registration, dispatch, and authorization.
//!
//! Check the tool allowlist before checking credentials and DUT grants.
//! The internal device services retain their own lease, approval, and permission checks.
//!
//! Layer the checks as follows.
//! Verify the credential has the requested DUT action grant.
//! Verify lease ownership and generation before constructing the write identity.
//! The device checks that identity at both admission and execution.
//! Approval tokens provide a separate confirmation for designated writes.
//!
//! Return authorization, lease, and approval failures as coded tool errors.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use rmcp::{
    ServerHandler,
    handler::server::{
        router::tool::ToolRouter, tool::ToolCallContext, wrapper::Json, wrapper::Parameters,
    },
    model::{CallToolRequestParams, CallToolResponse, Implementation, InitializeResult},
    schemars, tool, tool_handler, tool_router,
};

use duthub_core::Principal;
use duthub_core::agent_access::DutAction;
use duthub_core::audit::AuditOutcome;
use duthub_core::lease::WriteIdentity;
use duthub_core::media_op::{MediaArtifact, MediaOp, MediaOpReply, RecordingHandle};

use crate::auth_http::{
    AccessContext, CurrentPrincipal, holder_for, lease_error, unauthorized_error,
};
use crate::notify::{
    AGENT_NOTICE_SOURCE, MESSAGE_MAX_CHARS, NotifyLevel, NotifyOutcome, NotifyRequest,
    TITLE_MAX_CHARS, UserNotifier, sanitize_notify_text,
};
use crate::state::{ApprovalErr, HeartbeatUpdate, McpState};

/// Names of interactive/control tools exposed ONLY on user-configured HTTP
/// listeners (never on desktop-issued Agent sessions, stdio snapshot, or A2A).
pub const NOTIFY_TOOL_NAME: &str = "notify_user";
pub const DUT_CONTROL_TOOL_NAME: &str = "dut_control";
pub const SERIAL_EXEC_TOOL_NAME: &str = "serial_exec";
pub const ADB_EXEC_TOOL_NAME: &str = "adb_exec";
pub const AGENT_FEEDBACK_TOOL_NAME: &str = "agent_feedback";
pub const USER_EXTRA_TOOL_NAMES: &[&str] = &[
    NOTIFY_TOOL_NAME,
    DUT_CONTROL_TOOL_NAME,
    SERIAL_EXEC_TOOL_NAME,
    ADB_EXEC_TOOL_NAME,
    AGENT_FEEDBACK_TOOL_NAME,
];

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutInfoArgs {
    pub dut_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaStreamsArgs {
    pub dut_id: String,
    /// Absent = status query; that response stays byte-for-byte what it was
    /// before `op` existed.
    #[serde(default)]
    pub op: Option<MediaOpArgs>,
}

/// The `op` payload of `media_streams`.
///
/// `deny_unknown_fields` is part of the contract: any key the schema does not
/// declare is rejected as invalid params instead of being silently ignored.
/// No variant carries a duration field — a recording ends at its paired
/// `RecordStop`, not at a deadline.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields,
    tag = "kind"
)]
pub enum MediaOpArgs {
    /// Grab one PNG frame and return it as an artifact.
    Snapshot {
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        note: Option<String>,
    },
    /// Start recording; returns a `recordingId` the paired `RecordStop` needs.
    /// There is deliberately no duration field.
    RecordStart {
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        note: Option<String>,
    },
    /// Stop the recording that `recordingId` identifies and return its artifact.
    RecordStop { recording_id: u64 },
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialReadArgs {
    pub dut_id: String,
    /// Return chunks with seq > sinceSeq (cursor pagination). Ignored when `tail` is set.
    #[serde(default)]
    pub since_seq: u64,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Return the newest `tail` chunks (1..=400), replacing the legacy `analyze_serial` tool.
    #[serde(default)]
    pub tail: Option<usize>,
    /// Output mode: `"stream"` (default raw chunks) or `"screen"` (clean rendered VT100 terminal screen).
    #[serde(default)]
    pub mode: Option<String>,
    /// Optional literal substring to wait for on the live serial stream (bounded by `timeoutMs`).
    #[serde(default)]
    pub wait_for: Option<String>,
    /// Optional bounded regex pattern to wait for on the live serial stream (bounded by `timeoutMs`).
    #[serde(default)]
    pub wait_regex: Option<String>,
    /// Optional wait budget in ms when `waitFor` or `waitRegex` is set (default 5000, clamped 100..=30000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DutControlArgs {
    pub dut_id: String,
    /// Action: `power_on` | `power_off` | `power_cycle` | reset | `power_key_short` | `power_key_long` | `enter_maskrom` | `enter_loader` | `relay_test` | `flash_list` | `flash_update` | `flash_reboot` | `acquire_lease` | `renew_lease` | `release_lease`.
    pub action: String,
    /// Relay channel number (1-based), required only when action == "`relay_test`".
    #[serde(default)]
    pub channel: Option<u8>,
    /// Hold duration in ms for `enter_maskrom` / `enter_loader` (default 1500, clamped 200..=10000).
    #[serde(default)]
    pub hold_ms: Option<u64>,
    /// Off duration in ms for `power_cycle` (default 1500, clamped 200..=10000).
    #[serde(default)]
    pub off_ms: Option<u64>,
    /// Plain image filename inside the DUT directory (or images/data/), required for action == "`flash_update`".
    #[serde(default)]
    pub file_id: Option<String>,
    /// Optional flash tool preset ID from settings.flashTools[] (defaults to the first configured preset).
    #[serde(default)]
    pub preset_id: Option<String>,
    /// Optional explicit multi-step lease handle from `dut_control(action="acquire_lease")` (required for `renew_lease`/`release_lease`).
    #[serde(default)]
    pub lease_id: Option<String>,
    /// Registered session name occupying the serial port (required for `action="acquire_lease"`, optional for single-step auto-leases).
    #[serde(default)]
    pub session_name: Option<String>,
    /// Optional FIFO wait budget in ms when another Agent holds this DUT (default 5000, max 60000; up to 120000 for `acquire_lease`).
    #[serde(default)]
    pub wait_ms: Option<u64>,
    /// Whether to automatically raise a desktop alert asking for human intervention when access/lease is blocked (default true).
    #[serde(default)]
    pub notify_on_blocked: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdbExecArgs {
    pub dut_id: String,
    /// Operation: shell | reboot | `reboot_loader` | logcat | push | pull.
    pub op: String,
    /// Shell command executed on the target board (required when op == "shell").
    #[serde(default)]
    pub command: Option<String>,
    /// Plain filename strictly inside the DUT directory (required for op == "push" / "pull").
    #[serde(default)]
    pub file_id: Option<String>,
    /// Target path on the board (required for op == "push" / "pull", e.g. "/data/local/tmp/test.bin").
    #[serde(default)]
    pub remote_path: Option<String>,
    /// Execution timeout in ms (default 15000, clamped 500..=30000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Optional explicit multi-step lease handle from `dut_control(action="acquire_lease")`.
    #[serde(default)]
    pub lease_id: Option<String>,
    /// Optional session name registered when acquiring a temporary serial lease.
    #[serde(default)]
    pub session_name: Option<String>,
    /// Optional FIFO wait budget in ms when another Agent holds this DUT (default 5000, max 60000).
    #[serde(default)]
    pub wait_ms: Option<u64>,
    /// Whether to automatically notify the desktop user for human intervention when blocked (default true).
    #[serde(default)]
    pub notify_on_blocked: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerialExecArgs {
    pub dut_id: String,
    /// Plain text/command to send immediately when `waitFor` is not used.
    #[serde(default)]
    pub text: Option<String>,
    /// Validated logical terminal keys to send as raw control bytes (e.g. `["ctrl+c"]`, `["esc"]`, `["up", "enter"]`).
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// Bookmark serial cursor before triggering relay `reset` so boot prompts are never missed.
    #[serde(default)]
    pub trigger_reset: bool,
    /// Wait until serial output contains this substring before sending `send` (e.g. "Hit any key").
    #[serde(default)]
    pub wait_for: Option<String>,
    /// Payload sent immediately after `waitFor` matches (defaults to "\r" when `waitFor` is set).
    #[serde(default)]
    pub send: Option<String>,
    /// Follow-up command sent ~250ms after `send` (e.g. "rockusb 0 mmc 0\r" or "reboot loader\r").
    #[serde(default)]
    pub then_send: Option<String>,
    /// Optional substring to wait for after sending commands to confirm completion.
    #[serde(default)]
    pub expect: Option<String>,
    /// Optional bounded regex pattern to wait for after sending commands to confirm completion.
    #[serde(default)]
    pub expect_regex: Option<String>,
    /// Optional quiet-settle window in ms (200..=5000): once output starts arriving, treat the command as done after `settleMs` of silence.
    #[serde(default)]
    pub settle_ms: Option<u64>,
    /// Stall detection threshold in ms (default 3000, clamped 200..=30000): flag `stalled: true` if zero response bytes arrive within this window.
    #[serde(default)]
    pub stall_timeout_ms: Option<u64>,
    /// Optional `[mark] {label}` written to the DUT serial log (creates a log segment boundary when split logs are enabled).
    #[serde(default)]
    pub mark_label: Option<String>,
    /// Overall wait budget in ms (default 5000, clamped 200..=30000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Optional idempotency key (1..=128 printable ASCII chars) for plain sends.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Optional explicit multi-step lease handle from `dut_control(action="acquire_lease")`.
    #[serde(default)]
    pub lease_id: Option<String>,
    /// Optional session name registered when acquiring a temporary serial lease.
    #[serde(default)]
    pub session_name: Option<String>,
    /// Optional FIFO wait budget in ms when another Agent holds this DUT (default 5000, max 60000).
    #[serde(default)]
    pub wait_ms: Option<u64>,
    /// Whether to automatically notify the desktop user for human intervention when blocked (default true).
    #[serde(default)]
    pub notify_on_blocked: Option<bool>,
}

fn default_limit() -> usize {
    80
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogSegmentsArgs {
    pub dut_id: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogSegmentReadArgs {
    pub dut_id: String,
    pub segment_id: String,
    #[serde(default)]
    pub offset: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DutFilesArgs {
    /// Omit to discover all DUT folders in the working directory.
    dut_id: Option<String>,
    /// Optional category filter: `image` | `TEST` | `压力&老化` | `串口日志` | `misc` | `feedback`.
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

/// Arguments of `agent_feedback`: record structured Agent usage feedback under `feedback/`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentFeedbackArgs {
    /// Target DUT ID (auto-bound on a DUT-scoped listener; writes to `<work_dir>/feedback/` when omitted on a generic listener).
    #[serde(default)]
    pub dut_id: Option<String>,
    /// Feedback category: `bug` | `usability` | `missing_feature` | `performance` | `doc` | `other`.
    pub category: String,
    /// Severity: `low` | `medium` | `high` (default `medium`).
    #[serde(default)]
    pub severity: Option<String>,
    /// Short feedback summary (1..=120 characters).
    pub title: String,
    /// Detailed context, pain point, reproduction steps, or optimization suggestion (1..=8192 characters).
    pub detail: String,
    /// Optional related MCP tool name (e.g. `serial_exec`, `dut_control`).
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Optional Agent session name.
    #[serde(default)]
    pub session_name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DutFileReadArgs {
    dut_id: String,
    file_id: String,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialWriteArgs {
    pub dut_id: String,
    pub text: String,
    /// Supply an approval token for the supported unleased compatibility path.
    /// A valid control lease is sufficient where the tool permits leased writes.
    #[serde(default)]
    pub approval_token: Option<String>,
    /// Lease handle issued by the acquire tool and verified against the current credential.
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialWriteRequest {
    #[serde(flatten)]
    args: SerialWriteArgs,
    /// Use a fresh ID for each operation. Retry within five minutes with the same ID and text.
    request_id: Option<String>,
}

const WRITE_RECEIPT_TTL: Duration = Duration::from_secs(300);
const WRITE_RECEIPT_LIMIT: usize = 4096;
const MAX_LEASE_WAIT_MS: u64 = 120_000;
const DEFAULT_SCOPED_LEASE_WAIT_MS: u64 = 5_000;
const MAX_SCOPED_LEASE_WAIT_MS: u64 = 60_000;
const MAX_CONCURRENT_LEASE_WAITS: usize = 16;

/// Client-side ceilings for one `media_streams` op. Each is a bound on a
/// single tool call, never on a recording: `recordStart` and `recordStop` are
/// two independent bounded calls, so how long the Agent records between them
/// is not part of any timer here.
pub const MEDIA_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);
pub const MEDIA_RECORD_START_TIMEOUT: Duration = Duration::from_secs(30);
pub const MEDIA_RECORD_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// `note` is metadata only (it never becomes part of a file name).
const MEDIA_NOTE_MAX_CHARS: usize = 64;

type WriteReceiptKey = (String, String, String);

struct WriteReceipt {
    text_digest: [u8; 32],
    accepted_at: Instant,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdbShellArgs {
    pub dut_id: String,
    /// Pass the ADB arguments without the executable or the device-selection flags.
    pub argv: Vec<String>,
    pub approval_token: String,
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControlPowerArgs {
    pub dut_id: String,
    pub on: bool,
    pub approval_token: String,
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControlResetArgs {
    pub dut_id: String,
    pub approval_token: String,
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControlPowerKeyArgs {
    pub dut_id: String,
    /// Use false for a short power-key press and true for a long press.
    pub long_press: bool,
    pub approval_token: String,
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RelayTestArgs {
    pub dut_id: String,
    /// One-based relay channel. The test affects only this channel.
    pub channel: u8,
    pub approval_token: String,
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcquireControlArgs {
    pub dut_id: String,
    /// Required session name occupying the serial port (1..=128 chars).
    pub session_name: String,
    /// Whether to wait in FIFO order when the lease is busy.
    #[serde(default)]
    pub wait: bool,
    /// Maximum wait in milliseconds. Zero returns immediately.
    #[serde(default)]
    pub deadline_ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaseIdArgs {
    pub dut_id: String,
    /// Handle returned by the control lease acquire tool.
    pub lease_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutLabelArgs {
    pub dut_id: String,
    /// Text appended to the segment marker.
    pub label: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatSetArgs {
    pub dut_id: String,
    #[serde(flatten)]
    pub update: HeartbeatUpdate,
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlashTestArgs {
    /// Stable ID of the flash preset in settings.
    pub preset_id: String,
}

/// Arguments of `notify_user`.
///
/// `level` stays a plain string so an unknown literal is reported as
/// `invalid_params` by the tool instead of being defaulted by serde.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotifyUserArgs {
    /// Notification body (control characters removed, capped at 200 characters).
    pub message: String,
    #[serde(default)]
    pub title: Option<String>,
    /// `info` | `warning` | `error`; anything else is `invalid_params`.
    #[serde(default)]
    pub level: Option<String>,
    /// Optional DUT the notice is about. On a DUT-bound listener this is
    /// checked against the binding; omitting it binds the listener's DUT.
    #[serde(default)]
    pub dut_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestArgs {
    pub dut_id: String,
    /// serial.write | adb.shell | control.power | control.reset | control.test
    pub action: String,
    /// JSON parameters bound to the approval. Channel tests bind the channel number.
    pub args: serde_json::Value,
}

/// Return a coded approval failure with recovery instructions.
fn approval_error(err: &ApprovalErr) -> rmcp::ErrorData {
    rmcp::ErrorData::invalid_params(
        format!("ApprovalRequired/{}: {}", err.code(), err.message()),
        Some(serde_json::json!({ "kind": "approval", "code": err.code().to_ascii_uppercase() })),
    )
}

fn state_err(message: &str) -> rmcp::ErrorData {
    if let Some((code, _)) = message
        .strip_prefix("LeaseRequired/")
        .and_then(|detail| detail.split_once(':'))
    {
        return rmcp::ErrorData::new(
            rmcp::model::ErrorCode(-32000),
            message.to_owned(),
            Some(serde_json::json!({ "kind": "lease", "code": code })),
        );
    }
    rmcp::ErrorData::internal_error(message.to_string(), None)
}

/// Capture-op failure: the `code` is the `MEDIA_*` contract from the design
/// (`docs/design/CAPTURE_STREAM_BACKENDS.md` §5.7). It never degrades into an empty
/// success response.
fn media_op_error(code: &str, message: &str) -> rmcp::ErrorData {
    rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32000),
        format!("{code}: {message}"),
        Some(serde_json::json!({ "kind": "media_op", "code": code })),
    )
}

/// `note` stays metadata: control characters are stripped and the length is
/// capped before the value leaves the MCP layer. It is not audited and never
/// becomes a file name.
fn sanitize_media_note(note: Option<String>) -> Option<String> {
    note.map(|note| {
        note.chars()
            .filter(|ch| !ch.is_control())
            .take(MEDIA_NOTE_MAX_CHARS)
            .collect()
    })
}

/// JSON for a finished media file. Snapshot and recording artifacts have
/// different shapes: `firstPath`/`rotated`/`durationSecs` only exist for a
/// recording, `width`/`height` only for a snapshot (omitted, never guessed,
/// when the PNG header could not be parsed).
fn media_artifact_json(dut_id: &str, artifact: MediaArtifact) -> serde_json::Value {
    let MediaArtifact {
        path,
        first_path,
        rotated,
        bytes,
        width,
        height,
        duration_secs,
        note,
        source,
    } = artifact;
    let mut object = serde_json::Map::new();
    if let Some(file_name) = std::path::Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
    {
        object.insert("fileId".to_owned(), file_name.into());
        object.insert(
            "resourceUri".to_owned(),
            format!("duthub://dut/{dut_id}/files/{file_name}").into(),
        );
    }
    object.insert("path".to_owned(), path.into());
    if let Some(first_path) = first_path {
        object.insert("firstPath".to_owned(), first_path.into());
        object.insert("rotated".to_owned(), rotated.into());
        if let Some(duration_secs) = duration_secs {
            object.insert("durationSecs".to_owned(), duration_secs.into());
        }
    } else {
        if let Some(width) = width {
            object.insert("width".to_owned(), width.into());
        }
        if let Some(height) = height {
            object.insert("height".to_owned(), height.into());
        }
    }
    object.insert("bytes".to_owned(), bytes.into());
    if let Some(note) = note {
        object.insert("note".to_owned(), note.into());
    }
    if let Some(source) = source {
        object.insert("source".to_owned(), source.into());
    }
    serde_json::Value::Object(object)
}

/// JSON for `recordStart`: the id the caller must pass back to `recordStop`.
fn media_recording_json(recording: RecordingHandle) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert("recordingId".to_owned(), recording.recording_id.into());
    object.insert("source".to_owned(), recording.source.into());
    object.insert("firstPath".to_owned(), recording.first_path.into());
    if let Some(note) = recording.note {
        object.insert("note".to_owned(), note.into());
    }
    object.insert("startedAtMs".to_owned(), recording.started_at_ms.into());
    serde_json::Value::Object(object)
}

/// Server core with the host-supplied live or snapshot state.
/// Stdio uses the local administrator context.
/// Its snapshot implementation rejects writes.
#[derive(Clone)]
pub struct McpServerCore {
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    /// Address advertised in the generated skill for this listener.
    listener_endpoint: Option<SocketAddr>,
    /// DUT bound to a dedicated listener. General listeners have no fixed DUT.
    listener_dut: Option<Arc<str>>,
    /// Evidence-only server (A2A): tool calls outside [`EVIDENCE_TOOL_NAMES`]
    /// are rejected even when they are otherwise plan-safe reads.
    evidence_scope: bool,
    /// Host UI bridge for `notify_user`. `None` (stdio snapshot, evidence
    /// server, desktop-issued Agent session listeners) hides the tool from
    /// `tools/list` and keeps it out of the dispatch table, so the visible
    /// surface equals the usable surface.
    notifier: Option<Arc<dyn UserNotifier>>,
    adb_discovery: Arc<tokio::sync::Semaphore>,
    lease_waits: Arc<tokio::sync::Semaphore>,
    write_receipts: Arc<Mutex<HashMap<WriteReceiptKey, WriteReceipt>>>,
}

impl std::fmt::Debug for McpServerCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerCore")
            .field("has_access", &self.access.is_some())
            .field("listener_endpoint", &self.listener_endpoint)
            .field("listener_dut", &self.listener_dut)
            .field("evidence_scope", &self.evidence_scope)
            .field("has_notifier", &self.notifier.is_some())
            .finish_non_exhaustive()
    }
}

impl McpServerCore {
    pub fn new(state: Arc<dyn McpState>, access: Option<Arc<AccessContext>>) -> Self {
        Self {
            state,
            access,
            listener_endpoint: None,
            listener_dut: None,
            evidence_scope: false,
            notifier: None,
            adb_discovery: Arc::new(tokio::sync::Semaphore::new(1)),
            lease_waits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LEASE_WAITS)),
            write_receipts: Arc::default(),
        }
    }

    /// Evidence-only server for the A2A surface: no auth context (in-process
    /// peer), bound to one DUT, and restricted to [`EVIDENCE_TOOL_NAMES`].
    pub fn for_evidence(state: Arc<dyn McpState>, dut_id: &str) -> Self {
        let mut server = Self::new(state, None);
        server.listener_dut = Some(Arc::from(dut_id));
        server.evidence_scope = true;
        server
    }

    pub(crate) fn with_listener_endpoint(mut self, endpoint: Option<SocketAddr>) -> Self {
        self.listener_endpoint = endpoint;
        self
    }

    pub(crate) fn with_listener_dut_arc(mut self, dut_id: Option<Arc<str>>) -> Self {
        self.listener_dut = dut_id;
        self
    }

    /// Attach (or clear) the host notifier for `notify_user`.
    ///
    /// Only user-configured MCP listeners pass `Some`; every desktop-issued
    /// Agent session listener keeps `None`, so those Agents cannot reach the
    /// tool at all.
    #[must_use]
    pub fn with_user_notifier(mut self, notifier: Option<Arc<dyn UserNotifier>>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Resolve the authenticated principal from `RequestContext` for Resource /
    /// Prompt handlers (same fail-closed rule as `CurrentPrincipal` on tools).
    fn resolve_principal(
        &self,
        context: &rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<Principal, rmcp::ErrorData> {
        if let Some(principal) = principal_in_context(context) {
            return Ok(principal);
        }
        if self.has_access() {
            return Err(unauthorized_error("", "read"));
        }
        Ok(Principal::LocalAdmin)
    }

    fn authorized_duts(&self, principal: &Principal) -> Vec<crate::state::DutSummary> {
        self.state
            .list_duts()
            .into_iter()
            .filter(|dut| {
                self.listener_dut
                    .as_deref()
                    .is_none_or(|bound| bound == dut.dut_id)
                    && self.access.as_ref().is_none_or(|access| {
                        access
                            .auth
                            .authorize(principal, &dut.dut_id, DutAction::Read)
                            .is_ok()
                    })
            })
            .collect()
    }

    /// Whether this server carries an auth context (HTTP listener).
    pub(crate) fn has_access(&self) -> bool {
        self.access.is_some()
    }

    /// Static route table for this instance. `list_tools`, `get_tool` and
    /// `call_tool` all read it, so a listener without a notifier can neither
    /// list nor dispatch `notify_user`.
    fn routes(&self) -> &'static ToolRouter<McpServerCore> {
        if self.notifier.is_some() {
            &PLAN_ROUTER_WITH_NOTIFY
        } else {
            &PLAN_ROUTER
        }
    }

    fn require_adb_shell(&self) -> Result<(), rmcp::ErrorData> {
        if self.state.supports_adb_shell() {
            return Ok(());
        }
        Err(rmcp::ErrorData::new(
            rmcp::model::ErrorCode(-32000),
            "ADB Shell 经此运行时不可用。请使用桌面 DUT 的 ADB/HDC Shell。",
            Some(serde_json::json!({ "kind": "capability", "code": "TOOL_UNAVAILABLE" })),
        ))
    }

    fn readable_adb_serials(
        &self,
        principal: &Principal,
    ) -> Option<std::collections::HashSet<String>> {
        let access = self.access.as_ref()?;
        if matches!(principal, Principal::LocalAdmin) {
            return None;
        }
        Some(
            self.state
                .list_duts()
                .iter()
                .filter(|dut| {
                    self.listener_dut
                        .as_deref()
                        .is_none_or(|bound| bound == dut.dut_id)
                        && access
                            .auth
                            .authorize(principal, &dut.dut_id, DutAction::Read)
                            .is_ok()
                })
                .filter_map(|dut| self.state.dut_info(&dut.dut_id)?.adb_serial)
                .collect(),
        )
    }

    /// The local administrator context has full grants.
    /// Authenticated credentials require the requested DUT grant. Record denials in the audit log.
    fn authorize(
        &self,
        principal: &Principal,
        dut_id: &str,
        action: DutAction,
    ) -> Result<(), rmcp::ErrorData> {
        // Reject cross-DUT requests without disclosing target existence.
        if let Some(bound) = self.listener_dut.as_deref()
            && bound != dut_id
        {
            return Err(unauthorized_error(bound, action.as_str()));
        }
        let Some(access) = &self.access else {
            return Ok(());
        };
        if matches!(principal, Principal::LocalAdmin) {
            return Ok(());
        }
        if access.auth.authorize(principal, dut_id, action).is_ok() {
            return Ok(());
        }
        access.audit.record(
            "grant.deny",
            principal.audit_identity().map(|(n, _)| n),
            principal.audit_identity().map(|(_, c)| c),
            Some(dut_id),
            duthub_core::audit::AuditOutcome::Denied,
            Some(&format!("{} 未授权", action.as_str())),
        );
        Err(unauthorized_error(dut_id, action.as_str()))
    }

    /// Verify the read grant for the selected DUT.
    fn authorize_read(&self, principal: &Principal, dut_id: &str) -> Result<(), rmcp::ErrorData> {
        if dut_id.trim().is_empty() {
            return Err(rmcp::ErrorData::invalid_params(
                "dutId 不能为空",
                Some(serde_json::json!({ "code": "DUT_ID_REQUIRED" })),
            ));
        }
        self.authorize(principal, dut_id, DutAction::Read)
    }

    /// Capture authorization for `media_streams` with an `op`: taking a frame or
    /// recording spawns capture work, writes into the DUT folder and consumes
    /// quota, so `Read` alone must not unlock it. The query branch keeps using
    /// [`Self::authorize_read`]; the listener-bound cross-DUT check comes for
    /// free from [`Self::authorize`].
    fn authorize_capture(
        &self,
        principal: &Principal,
        dut_id: &str,
    ) -> Result<(), rmcp::ErrorData> {
        if dut_id.trim().is_empty() {
            return Err(rmcp::ErrorData::invalid_params(
                "dutId 不能为空",
                Some(serde_json::json!({ "code": "DUT_ID_REQUIRED" })),
            ));
        }
        self.authorize(principal, dut_id, DutAction::Capture)
    }

    /// The original `media_streams` response (no `op`). Kept as its own method
    /// so the status query keeps producing exactly today's shape.
    fn media_streams_status(
        &self,
        principal: &Principal,
        dut_id: &str,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(principal, dut_id)?;
        let streams = self.state.media_streams(dut_id);
        let status = if streams.is_empty() {
            match self.state.has_media_binding(dut_id) {
                Some(true) => "bound_not_started",
                Some(false) => "not_bound",
                None => return Ok(Json(serde_json::json!({ "streams": streams }))),
            }
        } else {
            "streaming"
        };
        Ok(Json(
            serde_json::json!({ "streams": streams, "status": status }),
        ))
    }

    /// Authorize DUT-scoped log and artifact reads.
    ///
    /// Both generic and DUT-bound listeners enforce per-DUT `DutAction::Read`
    /// grants so credentials only access logs and files of DUTs they were
    /// explicitly authorized to read.
    fn authorize_scoped_read(
        &self,
        principal: &Principal,
        dut_id: &str,
    ) -> Result<(), rmcp::ErrorData> {
        self.authorize(principal, dut_id, DutAction::Read)
    }

    /// Return the default DUT for a dedicated endpoint.
    fn scoped_dut<'a>(&'a self, requested: Option<&'a str>) -> Option<&'a str> {
        requested.or(self.listener_dut.as_deref())
    }

    /// Listing tools require a read grant for at least one DUT.
    /// Filter individual DUT results against their own grants.
    fn authorize_any_read(&self, principal: &Principal) -> Result<(), rmcp::ErrorData> {
        if let Some(bound) = self.listener_dut.as_deref() {
            return self.authorize(principal, bound, DutAction::Read);
        }
        let Some(access) = &self.access else {
            return Ok(());
        };
        if matches!(principal, Principal::LocalAdmin) {
            return Ok(());
        }
        let Principal::Agent { credential_id, .. } = principal else {
            return Ok(());
        };
        // Combine persistent and session-specific grants.
        // Inactive and unknown credentials cannot use either grant source.
        let any_read = access.auth.grants_for(credential_id).is_some_and(|grants| {
            grants
                .iter()
                .any(|grant| grant.actions.contains(&DutAction::Read))
        });
        if any_read {
            Ok(())
        } else {
            access.audit.record(
                "grant.deny",
                principal.audit_identity().map(|(n, _)| n),
                Some(credential_id.as_str()),
                None,
                duthub_core::audit::AuditOutcome::Denied,
                Some("无任何 read 授权"),
            );
            Err(unauthorized_error("*", "read"))
        }
    }

    /// Acquisition requires at least one write grant for the DUT.
    fn authorize_any_write(
        &self,
        principal: &Principal,
        dut_id: &str,
    ) -> Result<(), rmcp::ErrorData> {
        if let Some(bound) = self.listener_dut.as_deref()
            && bound != dut_id
        {
            return Err(unauthorized_error(bound, "任一写类动作"));
        }
        let Some(access) = &self.access else {
            return Ok(());
        };
        if matches!(principal, Principal::LocalAdmin) {
            return Ok(());
        }
        for action in [
            DutAction::SerialWrite,
            DutAction::Adb,
            DutAction::Relay,
            DutAction::Power,
            DutAction::Reset,
            DutAction::Flash,
        ] {
            if access.auth.authorize(principal, dut_id, action).is_ok() {
                return Ok(());
            }
        }
        access.audit.record(
            "grant.deny",
            principal.audit_identity().map(|(n, _)| n),
            principal.audit_identity().map(|(_, c)| c),
            Some(dut_id),
            duthub_core::audit::AuditOutcome::Denied,
            Some("acquire 需要任一写类授权"),
        );
        Err(unauthorized_error(dut_id, "任一写类动作"))
    }

    /// Check the credential's lease ownership and generation to build the write context.
    /// The actor rechecks it when queuing and executing the write.
    /// With authentication enabled, clients must acquire a lease first.
    /// Unauthenticated compatibility paths may omit the lease.
    /// A supplied lease is still validated and GUI takeover remains effective.
    fn lease_identity(
        &self,
        principal: &Principal,
        dut_id: &str,
        lease_id: Option<&str>,
    ) -> Result<WriteIdentity, rmcp::ErrorData> {
        let Some(access) = &self.access else {
            return Ok(WriteIdentity::agent(None, None));
        };
        let credential_id = match principal {
            Principal::Agent { credential_id, .. } => credential_id.clone(),
            Principal::LocalAdmin => "local-admin".to_string(),
        };
        match lease_id {
            Some(lease_id) => {
                let generation = access
                    .lease
                    .validate_agent_lease(dut_id, lease_id, &credential_id)
                    .map_err(|err| {
                        access.audit.record(
                            "write.deny",
                            principal.audit_identity().map(|(n, _)| n),
                            Some(credential_id.as_str()),
                            Some(dut_id),
                            duthub_core::audit::AuditOutcome::Denied,
                            Some(&format!("lease 复核拒绝：{}", err.message())),
                        );
                        lease_error(&err)
                    })?;
                Ok(WriteIdentity::agent(
                    Some(lease_id.to_string()),
                    Some(generation),
                ))
            }
            None if access.auth.enabled() => {
                Err(lease_error(&duthub_core::lease::LeaseError::LeaseRequired))
            }
            None => Ok(WriteIdentity::agent(None, None)),
        }
    }

    /// Verify that an Agent owns the lease before renewal or release.
    fn ensure_holder_owned(
        &self,
        principal: &Principal,
        dut_id: &str,
        lease_id: &str,
    ) -> Result<(), rmcp::ErrorData> {
        let Some(access) = &self.access else {
            return Ok(());
        };
        if let Principal::Agent { credential_id, .. } = principal {
            access
                .lease
                .validate_agent_lease(dut_id, lease_id, credential_id)
                .map_err(|err| lease_error(&err))?;
        }
        Ok(())
    }

    /// Derive a unified machine-readable DUT state and deterministic next-action hint
    /// for Agent preflight routing (`ready`, `busy_gui`, `busy_agent`, `flashing`,
    /// `needs_inspection`, `manual_takeover`, `disconnected`).
    fn derive_dut_state(
        &self,
        source: &str,
        control: Option<&crate::state::ControlStatusDto>,
    ) -> (&'static str, String) {
        if self
            .access
            .as_ref()
            .is_some_and(|access| access.lease.manual_control())
        {
            return (
                "manual_takeover",
                "Manual takeover is active in duthub. Read-only inspection still works; wait for the human operator to disable Manual Takeover before writing or controlling hardware.".into(),
            );
        }
        if let Some(ctrl) = control {
            match ctrl.state.as_str() {
                "needs_inspection" => {
                    return (
                        "needs_inspection",
                        "DUT is locked for human safety inspection after a fault. Do not retry writes; call notify_user and wait for operator clearance.".into(),
                    );
                }
                "protected" => {
                    let op = ctrl
                        .protected_op
                        .as_deref()
                        .unwrap_or("protected operation");
                    return (
                        "flashing",
                        format!(
                            "DUT is in protected state ({op}). Avoid control writes; monitor via serial_read or dut_status until it returns to ready."
                        ),
                    );
                }
                _ => {}
            }
        }
        if source == "idle" {
            return (
                "disconnected",
                "Serial session is not open. Read saved logs via log_segments or call notify_user to ask the operator to open the serial port.".into(),
            );
        }
        if let Some(ctrl) = control
            && ctrl.state == "held"
        {
            let holder = ctrl.holder.as_deref().unwrap_or("unknown");
            if holder.eq_ignore_ascii_case("gui") || holder.contains("GUI") {
                return (
                    "busy_gui",
                    "Desktop GUI operator holds the serial port. Do not retry-loop writes; call notify_user to ask the operator to release control (or start duthub with --release-gui-lease).".into(),
                );
            }
            let session = ctrl
                .session_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(holder);
            let occupied = ctrl.occupied_secs.unwrap_or(0);
            return (
                "busy_agent",
                format!(
                    "Held by Agent session '{session}' (occupied {occupied}s, queue={}). Use waitMs to queue politely or notify_user if stuck.",
                    ctrl.queue_len
                ),
            );
        }
        (
            "ready",
            "DUT is ready for serial_read, serial_exec, adb_exec, and dut_control.".into(),
        )
    }

    /// Return the clean rendered terminal screen text for a DUT, preferring the live
    /// VT100 emulator snapshot and falling back to ANSI-stripped recent serial output.
    fn render_dut_screen(&self, dut_id: &str, max_tail_lines: Option<usize>) -> Option<String> {
        let raw = if let Some(screen) = self.state.serial_screen(dut_id) {
            screen
        } else {
            let result = self.state.serial_read(dut_id, 0, usize::MAX)?;
            let start = result.lines.len().saturating_sub(150);
            let mut joined = String::new();
            for line in &result.lines[start..] {
                joined.push_str(&line.text);
            }
            crate::keys::strip_ansi_escapes(&joined)
        };
        Some(match max_tail_lines {
            Some(max_lines) => tail_text_lines(&raw, max_lines),
            None => raw,
        })
    }

    /// Build a human-intervention desktop notice when an Agent fails to obtain
    /// DUT usage rights (lease busy, manual takeover, protected op, inspection,
    /// or missing action grant), and enrich the MCP error data with the exact
    /// holder, reason, wait duration, and notification outcome.
    fn attach_blocked_notice(
        &self,
        mut err: rmcp::ErrorData,
        principal: &Principal,
        dut_id: &str,
        action_label: &str,
        waited_ms: u64,
        notify_on_blocked: Option<bool>,
    ) -> rmcp::ErrorData {
        let requester = match principal {
            Principal::Agent { agent_name, .. } => agent_name.as_str(),
            Principal::LocalAdmin => "local-admin",
        };
        let credential_id = match principal {
            Principal::Agent { credential_id, .. } => Some(credential_id.clone()),
            Principal::LocalAdmin => None,
        };
        let dut_label = self
            .state
            .dut_info(dut_id)
            .map_or_else(|| dut_id.to_owned(), |info| info.name);

        let code = err
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("ACCESS_BLOCKED")
            .to_owned();

        let status = self.access.as_ref().map(|a| a.lease.status(dut_id));
        let manual = self
            .access
            .as_ref()
            .is_some_and(|a| a.lease.manual_control());

        let (title, reason, suggested_action) = match (code.as_str(), manual, status.as_ref()) {
            ("UNAUTHORIZED", _, _) => (
                "Human intervention: Agent permission denied",
                format!(
                    "Agent '{requester}' cannot run '{action_label}' on {dut_label}: missing required DUT grant. Enable the grant in Connection -> MCP service if intended."
                ),
                "Grant the required DUT permission in duthub UI (Connection -> MCP service) or use a credential with the needed grant.",
            ),
            (_, true, _) => (
                "Human intervention: Manual takeover active",
                format!(
                    "Agent '{requester}' cannot run '{action_label}' on {dut_label}: manual takeover mode is active. Turn off Manual Takeover when ready to resume Agents."
                ),
                "Wait for the human operator to disable Manual Takeover in duthub.",
            ),
            (_, false, Some(st)) if st.phase == duthub_core::lease::LeasePhase::NeedsInspection => {
                (
                    "Human intervention: DUT needs safety check",
                    format!(
                        "Agent '{requester}' cannot run '{action_label}' on {dut_label}: previous critical operation requires manual inspection and recovery in duthub."
                    ),
                    "Wait for the human operator to inspect the hardware and click 'Mark safe & recover'.",
                )
            }
            (_, false, Some(st)) if st.phase == duthub_core::lease::LeasePhase::Protected => {
                let op = st.protected_op.as_deref().unwrap_or("critical operation");
                (
                    "Human intervention: DUT in protected operation",
                    format!(
                        "Agent '{requester}' cannot run '{action_label}' on {dut_label}: protected operation '{op}' is running."
                    ),
                    "Wait for the protected operation (such as firmware flashing) to finish, then retry.",
                )
            }
            (_, false, Some(st)) if st.phase == duthub_core::lease::LeasePhase::Held => {
                let holder = st.holder.as_deref().unwrap_or("another holder");
                if holder.starts_with("GUI") {
                    (
                        "Human intervention: GUI holds DUT control",
                        format!(
                            "Agent '{requester}' cannot run '{action_label}' on {dut_label}: desktop GUI currently holds control. Click 'Release control' to let Agents proceed."
                        ),
                        "Ask the human operator to click 'Release control' in duthub (or launch with --release-gui-lease for unattended mode).",
                    )
                } else {
                    (
                        "Human intervention: Multi-Agent DUT contention",
                        format!(
                            "Agent '{requester}' waited {waited_ms}ms for '{action_label}' on {dut_label}, still held by '{holder}' (queue {}). Check for Agent conflict or take over/release control.",
                            st.queue_len
                        ),
                        "Wait for the current Agent to release its lease, increase waitMs, or wait for human intervention.",
                    )
                }
            }
            _ => (
                "Human intervention: DUT access blocked",
                format!(
                    "Agent '{requester}' cannot run '{action_label}' on {dut_label}: {}",
                    err.message
                ),
                "Check dut_status and resolve lease or permission state before retrying.",
            ),
        };

        let should_notify = notify_on_blocked.unwrap_or(true);
        let notify_outcome = if should_notify && let Some(notifier) = &self.notifier {
            let req = NotifyRequest {
                dut_id: Some(dut_id.to_owned()),
                title: Some(sanitize_notify_text(title, TITLE_MAX_CHARS)),
                message: sanitize_notify_text(&reason, MESSAGE_MAX_CHARS),
                level: NotifyLevel::Warning,
                credential_id,
            };
            Some(notifier.notify(&req))
        } else {
            None
        };

        let mut data = match err.data.take() {
            Some(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        data.insert("dutId".into(), serde_json::json!(dut_id));
        data.insert("action".into(), serde_json::json!(action_label));
        data.insert("requester".into(), serde_json::json!(requester));
        data.insert("waitedMs".into(), serde_json::json!(waited_ms));
        data.insert("reason".into(), serde_json::json!(reason));
        data.insert(
            "suggestedAction".into(),
            serde_json::json!(suggested_action),
        );
        if let Some(st) = status {
            if let Some(holder) = st.holder {
                data.insert("holder".into(), serde_json::json!(holder));
            }
            if let Some(session_name) = st.session_name {
                data.insert("sessionName".into(), serde_json::json!(session_name));
            }
            if let Some(acquired_at) = st.acquired_at_unix_ms {
                data.insert("acquiredAtUnixMs".into(), serde_json::json!(acquired_at));
            }
            if let Some(occupied_secs) = st.occupied_secs {
                data.insert("occupiedSecs".into(), serde_json::json!(occupied_secs));
            }
            data.insert("queueLen".into(), serde_json::json!(st.queue_len));
            if let Some(op) = st.protected_op {
                data.insert("protectedOp".into(), serde_json::json!(op));
            }
        }
        data.insert("manualControl".into(), serde_json::json!(manual));
        data.insert(
            "humanNotified".into(),
            serde_json::json!(matches!(notify_outcome, Some(NotifyOutcome::Queued))),
        );
        if let Some(outcome) = notify_outcome {
            data.insert("notifyOutcome".into(), serde_json::json!(outcome.as_str()));
        }
        err.data = Some(serde_json::Value::Object(data));
        err
    }

    /// Acquire or validate an exclusive DUT control lease for a write/control/ADB
    /// operation, supporting fair FIFO queuing across concurrent Agents and
    /// automatic human-intervention alerts when access is blocked.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal helper aggregating lease options from MCP tool arguments"
    )]
    async fn acquire_or_use_lease(
        &self,
        principal: &Principal,
        dut_id: &str,
        action_label: &str,
        explicit_lease_id: Option<&str>,
        session_name: Option<&str>,
        wait_ms: Option<u64>,
        notify_on_blocked: Option<bool>,
    ) -> Result<(WriteIdentity, Option<ScopedLeaseGuard>, u64), rmcp::ErrorData> {
        let Some(access) = &self.access else {
            return Ok((WriteIdentity::agent(None, None), None, 0));
        };
        if let Some(lease_id) = explicit_lease_id.map(str::trim).filter(|s| !s.is_empty()) {
            let identity = self
                .lease_identity(principal, dut_id, Some(lease_id))
                .map_err(|err| {
                    self.attach_blocked_notice(
                        err,
                        principal,
                        dut_id,
                        action_label,
                        0,
                        notify_on_blocked,
                    )
                })?;
            return Ok((identity, None, 0));
        }

        let holder = holder_for(principal);
        let registered_session = if let Some(raw) = session_name {
            duthub_core::lease::validate_session_name(raw).map_err(|msg| {
                rmcp::ErrorData::invalid_params(
                    msg,
                    Some(serde_json::json!({ "code": "INVALID_SESSION_NAME" })),
                )
            })?
        } else {
            let requester = match principal {
                Principal::Agent { agent_name, .. } => agent_name.as_str(),
                Principal::LocalAdmin => "local-admin",
            };
            format!("{requester}/{action_label}")
        };
        let status = access.lease.status(dut_id);
        let blocked_for_human = access.lease.manual_control()
            || matches!(
                status.phase,
                duthub_core::lease::LeasePhase::NeedsInspection
                    | duthub_core::lease::LeasePhase::Protected
            )
            || (status.phase == duthub_core::lease::LeasePhase::Held
                && status
                    .holder
                    .as_deref()
                    .is_some_and(|h| h.starts_with("GUI")));

        let effective_wait_ms = if blocked_for_human {
            0
        } else {
            wait_ms
                .unwrap_or(DEFAULT_SCOPED_LEASE_WAIT_MS)
                .min(MAX_SCOPED_LEASE_WAIT_MS)
        };

        let started = Instant::now();
        let grant_result = if effective_wait_ms == 0 {
            self.state
                .dut_acquire_control_with_session(dut_id, holder, &registered_session, 0)
        } else {
            let Ok(permit) = self.lease_waits.clone().try_acquire_owned() else {
                let err = rmcp::ErrorData::new(
                    rmcp::model::ErrorCode(-32000),
                    "LeaseRequired/DUT_BUSY: lease wait queue is full; retry later",
                    Some(serde_json::json!({ "kind": "lease", "code": "DUT_BUSY" })),
                );
                return Err(self.attach_blocked_notice(
                    err,
                    principal,
                    dut_id,
                    action_label,
                    0,
                    notify_on_blocked,
                ));
            };
            let state = self.state.clone();
            let dut = dut_id.to_owned();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                state.dut_acquire_control_with_session(
                    &dut,
                    holder,
                    &registered_session,
                    effective_wait_ms,
                )
            })
            .await
            .map_err(|_| state_err("Lease wait worker failed"))?
        };
        let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let grant = grant_result.map_err(|msg| {
            self.attach_blocked_notice(
                state_err(&msg),
                principal,
                dut_id,
                action_label,
                waited_ms,
                notify_on_blocked,
            )
        })?;

        let identity = WriteIdentity::agent(Some(grant.lease_id.clone()), Some(grant.generation));
        if let Principal::Agent { credential_id, .. } = principal
            && let Err(err) =
                access
                    .lease
                    .validate_agent_lease(dut_id, &grant.lease_id, credential_id)
        {
            let _ = self.state.dut_release_control(dut_id, &grant.lease_id);
            return Err(self.attach_blocked_notice(
                lease_error(&err),
                principal,
                dut_id,
                action_label,
                waited_ms,
                notify_on_blocked,
            ));
        }
        let guard = ScopedLeaseGuard {
            state: self.state.clone(),
            dut_id: dut_id.to_owned(),
            lease_id: grant.lease_id,
        };
        Ok((identity, Some(guard), waited_ms))
    }
}

struct ScopedLeaseGuard {
    state: Arc<dyn McpState>,
    dut_id: String,
    lease_id: String,
}

impl Drop for ScopedLeaseGuard {
    fn drop(&mut self) {
        let _ = self.state.dut_release_control(&self.dut_id, &self.lease_id);
    }
}

#[tool_router]
impl McpServerCore {
    #[tool(
        description = "查询视频流状态，或带 op 执行截屏/录屏（参数均 camelCase）。示例：① 查状态 {\"dutId\":\"<id>\"}；② 截屏 {\"dutId\":\"<id>\",\"op\":{\"kind\":\"snapshot\"}}（返回 fileId 与 resourceUri，可直接 resources/read 读 PNG）；③ 录屏 {\"dutId\":\"<id>\",\"op\":{\"kind\":\"recordStart\"}} → 停止 {\"dutId\":\"<id>\",\"op\":{\"kind\":\"recordStop\",\"recordingId\":1}}。op 需该 DUT 的 capture 授权。"
    )]
    async fn media_streams(
        &self,
        Parameters(args): Parameters<MediaStreamsArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        let Some(op) = args.op else {
            return self.media_streams_status(&principal.0, &args.dut_id);
        };
        // Only the op branch needs Capture; the query above still needs Read.
        self.authorize_capture(&principal.0, &args.dut_id)?;
        let dut_id = args.dut_id;
        let (request_op, source, timeout) = match op {
            MediaOpArgs::Snapshot { source, note } => (
                MediaOp::Snapshot {
                    note: sanitize_media_note(note),
                },
                source,
                MEDIA_SNAPSHOT_TIMEOUT,
            ),
            MediaOpArgs::RecordStart { source, note } => (
                MediaOp::RecordStart {
                    note: sanitize_media_note(note),
                },
                source,
                MEDIA_RECORD_START_TIMEOUT,
            ),
            MediaOpArgs::RecordStop { recording_id } => (
                MediaOp::RecordStop { recording_id },
                None,
                MEDIA_RECORD_STOP_TIMEOUT,
            ),
        };
        // The desktop drains this queue on its own 50ms tick and answers via the
        // oneshot. Never block a runtime worker (the HTTP listener has two):
        // awaiting the receiver under `timeout` is the only waiting allowed.
        let Some(receiver) = self.state.media_op(dut_id.clone(), source, request_op) else {
            return Err(media_op_error(
                "MEDIA_CAPTURE_UNAVAILABLE",
                "当前状态源没有采集能力（桌面端未运行）",
            ));
        };
        let reply = match tokio::time::timeout(timeout, receiver).await {
            Err(_) => {
                return Err(media_op_error(
                    "MEDIA_OP_TIMEOUT",
                    "请求超时，桌面端未在规定时间内应答",
                ));
            }
            // The workbench is gone: the reply sender was dropped.
            Ok(Err(_)) => {
                return Err(media_op_error(
                    "MEDIA_OP_DROPPED",
                    "请求未被桌面端处理（工作台已退出或未取走请求）",
                ));
            }
            Ok(Ok(reply)) => reply,
        };
        match reply {
            MediaOpReply::Artifact(artifact) => Ok(Json(serde_json::json!({
                "artifact": media_artifact_json(&dut_id, artifact),
            }))),
            MediaOpReply::Recording(recording) => Ok(Json(serde_json::json!({
                "recording": media_recording_json(recording),
            }))),
            MediaOpReply::Err(error) => Err(media_op_error(error.code, &error.message)),
        }
    }

    #[tool(
        description = "列出当前凭据有权读取的 DUT、机读状态（dutState + nextActionHint）、串口摘要、已配置的继电器控制能力及当前串口占用信息（只读，不含凭据）"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn dut_list(
        &self,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_any_read(&principal.0)?;
        let duts: Vec<serde_json::Value> = self
            .state
            .list_duts()
            .into_iter()
            .filter(|dut| {
                self.listener_dut
                    .as_deref()
                    .is_none_or(|bound| bound == dut.dut_id)
                    && self.access.as_ref().is_none_or(|access| {
                        access
                            .auth
                            .authorize(&principal.0, &dut.dut_id, DutAction::Read)
                            .is_ok()
                    })
            })
            .map(|d| {
                let mut value = serde_json::to_value(&d).unwrap_or(serde_json::Value::Null);
                if let Some(info) = self.state.dut_info(&d.dut_id)
                    && let Some(relay) = info.relay
                {
                    value["relayCapabilities"] = serde_json::json!(relay.capabilities);
                    value["relayChannels"] = serde_json::json!(relay.channels);
                }
                let control_status = self.state.dut_control_status(&d.dut_id).ok();
                let (dut_state, next_action_hint) =
                    self.derive_dut_state(&d.source, control_status.as_ref());
                value["dutState"] = serde_json::json!(dut_state);
                value["nextActionHint"] = serde_json::json!(next_action_hint);
                if let Some(control_status) = control_status {
                    value["controlStatus"] =
                        serde_json::to_value(control_status).unwrap_or(serde_json::Value::Null);
                }
                value
            })
            .collect();
        Ok(Json(serde_json::json!({ "duts": duts })))
    }

    #[tool(
        description = "获取单个 DUT 的完整只读状态（合并原 dut_info/dut_control_status/heartbeat_get/relay_status）：返回机读 dutState 与 nextActionHint、串口与会话、已配置继电器板卡与各功能通道（power/reset/recovery/maskrom/powerKey）、继电器通道实时状态、烧录模式能力、控制租约与心跳状态。"
    )]
    async fn dut_status(
        &self,
        Parameters(args): Parameters<DutInfoArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(&principal.0, &args.dut_id)?;
        let Some(info) = self.state.dut_info(&args.dut_id) else {
            return Err(rmcp::ErrorData::invalid_params(
                format!("DUT 未找到: {}（会话未打开）", args.dut_id),
                None,
            ));
        };
        let session_id = (info.source != "idle").then(|| format!("sess-{}", info.dut_id));
        let has_reset = info
            .relay
            .as_ref()
            .is_some_and(|relay| relay.channels.reset.is_some());
        let has_power = info
            .relay
            .as_ref()
            .is_some_and(|relay| relay.channels.power.is_some() || relay.power_outlet.is_some());
        let has_maskrom = info
            .relay
            .as_ref()
            .is_some_and(|relay| relay.channels.maskrom.is_some());
        let has_recovery = info
            .relay
            .as_ref()
            .is_some_and(|relay| relay.channels.recovery.is_some());
        let has_adb = info
            .adb_serial
            .as_deref()
            .is_some_and(|serial| !serial.trim().is_empty());
        let can_trigger_reboot = has_reset || has_power;
        let can_enter_maskrom = has_maskrom && can_trigger_reboot;
        let can_enter_loader_hw = has_recovery && can_trigger_reboot;
        let can_enter_loader_serial = session_id.is_some();
        let recommended_loader_flow = match (
            can_enter_loader_hw,
            has_adb,
            can_enter_loader_serial,
            has_reset,
        ) {
            (true, _, _, _) => "hardware_recovery_relay",
            (false, true, _, _) => "adb_exec_reboot_loader_or_serial_exec",
            (false, false, true, true) => "serial_exec_with_trigger_reset_or_reboot_loader",
            (false, false, true, false) => "serial_exec_reboot_loader",
            (false, false, false, _) => "unavailable",
        };
        let channel_states = if info.relay.is_some() {
            let state = self.state.clone();
            let dut_id = args.dut_id.clone();
            let probe = tokio::time::timeout(
                Duration::from_millis(1_500),
                tokio::task::spawn_blocking(move || state.relay_status(&dut_id)),
            )
            .await;
            probe
                .ok()
                .and_then(std::result::Result::ok)
                .and_then(std::result::Result::ok)
                .map(|channels| {
                    channels
                        .into_iter()
                        .map(|(channel, on)| serde_json::json!({ "channel": channel, "on": on }))
                        .collect::<Vec<_>>()
                })
        } else {
            None
        };
        let control_dto = self.state.dut_control_status(&args.dut_id).ok();
        let (dut_state, next_action_hint) =
            self.derive_dut_state(&info.source, control_dto.as_ref());
        let control = control_dto
            .as_ref()
            .and_then(|status| serde_json::to_value(status).ok());
        let heartbeat = self
            .state
            .heartbeat_get(&args.dut_id)
            .ok()
            .and_then(|hb| serde_json::to_value(hb).ok());
        let mut payload = serde_json::to_value(&info).unwrap_or(serde_json::Value::Null);
        payload["dutState"] = serde_json::json!(dut_state);
        payload["nextActionHint"] = serde_json::json!(next_action_hint);
        payload["session_id"] = serde_json::json!(session_id);
        payload["session_open"] = serde_json::json!(session_id.is_some());
        payload["relayChannelStates"] =
            channel_states.map_or(serde_json::Value::Null, serde_json::Value::Array);
        payload["bootModes"] = serde_json::json!({
            "canEnterMaskrom": can_enter_maskrom,
            "canEnterLoaderByHardware": can_enter_loader_hw,
            "canEnterLoaderByAdb": has_adb,
            "canEnterLoaderBySerial": can_enter_loader_serial,
            "recommendedLoaderFlow": recommended_loader_flow,
        });
        payload["control"] = control.unwrap_or(serde_json::Value::Null);
        payload["heartbeat"] = heartbeat.unwrap_or(serde_json::Value::Null);
        Ok(Json(payload))
    }

    #[tool(
        description = "读取 DUT 实时串口数据或干净的 VT100 渲染屏快照。支持游标续读（sinceSeq + limit）、尾部取数（tail=1..=400）、渲染屏模式（mode=\"screen\"）以及只读阻塞等待（waitFor / waitRegex + timeoutMs）。关闭会话的历史请使用 log_segments。"
    )]
    async fn serial_read(
        &self,
        Parameters(args): Parameters<SerialReadArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        const MAX_WAIT_PATTERN_CHARS: usize = 256;

        self.authorize_read(&principal.0, &args.dut_id)?;
        let mode = match args
            .mode
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None | Some("stream") => "stream",
            Some("screen") => "screen",
            Some(other) => {
                return Err(rmcp::ErrorData::invalid_params(
                    format!(
                        "unsupported serial_read mode '{other}'; expected 'stream' or 'screen'"
                    ),
                    None,
                ));
            }
        };

        let wait_for = args
            .wait_for
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(pat) = wait_for.as_deref()
            && pat.chars().count() > MAX_WAIT_PATTERN_CHARS
        {
            return Err(rmcp::ErrorData::invalid_params(
                format!("waitFor exceeds {MAX_WAIT_PATTERN_CHARS} chars"),
                None,
            ));
        }
        let wait_regex = match args.wait_regex.as_deref().filter(|s| !s.is_empty()) {
            Some(pat) => Some(compile_bounded_regex(pat, "waitRegex")?),
            None => None,
        };

        // Screen-only snapshot mode (no wait pattern).
        if mode == "screen" && wait_for.is_none() && wait_regex.is_none() {
            let Some(probe) = self.state.serial_read(&args.dut_id, 0, 1) else {
                return Err(rmcp::ErrorData::new(
                    rmcp::model::ErrorCode(-32000),
                    "DUT 会话未打开。已保存的历史请查 log_segments。",
                    Some(serde_json::json!({ "kind": "session", "code": "SESSION_NOT_OPEN" })),
                ));
            };
            let latest_seq = self
                .state
                .serial_read(&args.dut_id, 0, usize::MAX)
                .map_or(probe.last_seq, |r| r.last_seq);
            let screen = self
                .render_dut_screen(&args.dut_id, args.tail.map(|t| t.clamp(1, 400)))
                .unwrap_or_default();
            return Ok(Json(serde_json::json!({
                "dut_id": args.dut_id,
                "session_id": probe.session_id,
                "last_seq": latest_seq,
                "mode": "screen",
                "screen": screen,
                "resourceUri": format!("duthub://dut/{}/serial/screen", args.dut_id),
            })));
        }

        // Read-only wait mode (`waitFor` and/or `waitRegex`), cursor-safe with `sinceSeq`.
        if wait_for.is_some() || wait_regex.is_some() {
            let limit = args.limit.clamp(1, 400);
            let timeout_ms = args.timeout_ms.unwrap_or(5_000).clamp(100, 30_000);
            let started = Instant::now();
            let deadline = started + Duration::from_millis(timeout_ms);
            let mut cursor = args.since_seq;
            let mut session_id;
            let mut collected: Vec<crate::state::LogLineDto> = Vec::new();
            let mut raw_buf = String::new();
            let mut matched_by = "timeout";

            loop {
                let page_limit = if cursor == 0 { usize::MAX } else { 400 };
                let Some(page) = self.state.serial_read(&args.dut_id, cursor, page_limit) else {
                    return Err(rmcp::ErrorData::new(
                        rmcp::model::ErrorCode(-32000),
                        "DUT 会话未打开。已保存的历史请查 log_segments。",
                        Some(serde_json::json!({ "kind": "session", "code": "SESSION_NOT_OPEN" })),
                    ));
                };
                session_id = page.session_id;
                cursor = page.last_seq;
                for line in page.lines {
                    if raw_buf.len() < 128 * 1024 {
                        raw_buf.push_str(&line.text);
                    }
                    collected.push(line);
                }
                if collected.len() > limit {
                    let drop_n = collected.len() - limit;
                    collected.drain(..drop_n);
                }
                let cleaned = crate::keys::strip_ansi_escapes(&raw_buf);
                if let Some(pat) = wait_for.as_deref()
                    && (raw_buf.contains(pat) || cleaned.contains(pat))
                {
                    matched_by = "waitFor";
                    break;
                }
                if let Some(re) = wait_regex.as_ref()
                    && (re.is_match(&raw_buf) || re.is_match(&cleaned))
                {
                    matched_by = "waitRegex";
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }

            let matched = matched_by != "timeout";
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            clean_log_lines(&mut collected);
            let mut payload = serde_json::json!({
                "dut_id": args.dut_id,
                "session_id": session_id,
                "last_seq": cursor,
                "lines": collected,
                "cursorReset": cursor < args.since_seq,
                "mode": mode,
                "matched": matched,
                "matchedBy": matched_by,
                "elapsedMs": elapsed_ms,
            });
            if mode == "screen" || !matched {
                payload["screen"] = serde_json::json!(
                    self.render_dut_screen(&args.dut_id, Some(40))
                        .unwrap_or_default()
                );
            }
            return Ok(Json(payload));
        }

        if let Some(tail) = args.tail {
            let tail = tail.clamp(1, 400);
            return match self.state.serial_read(&args.dut_id, 0, usize::MAX) {
                Some(mut result) => {
                    let start = result.lines.len().saturating_sub(tail);
                    result.lines.drain(..start);
                    clean_log_lines(&mut result.lines);
                    let mut payload =
                        serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
                    payload["cursorReset"] = serde_json::json!(false);
                    Ok(Json(payload))
                }
                None => Err(rmcp::ErrorData::new(
                    rmcp::model::ErrorCode(-32000),
                    "DUT 会话未打开。已保存的历史请查 log_segments。",
                    Some(serde_json::json!({ "kind": "session", "code": "SESSION_NOT_OPEN" })),
                )),
            };
        }
        let limit = args.limit.clamp(1, 400);
        match self.state.serial_read(&args.dut_id, args.since_seq, limit) {
            Some(mut result) => {
                clean_log_lines(&mut result.lines);
                let mut payload = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
                payload["cursorReset"] = serde_json::json!(result.last_seq < args.since_seq);
                Ok(Json(payload))
            }
            None => Err(rmcp::ErrorData::new(
                rmcp::model::ErrorCode(-32000),
                "DUT 会话未打开。已保存的历史请查 log_segments。",
                Some(serde_json::json!({ "kind": "session", "code": "SESSION_NOT_OPEN" })),
            )),
        }
    }

    #[tool(
        description = "列出 DUT 已落盘的串口日志段，包含已关闭的会话。按 offset 分页，返回下一页游标。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn log_segments(
        &self,
        Parameters(args): Parameters<LogSegmentsArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_scoped_read(&principal.0, &args.dut_id)?;
        let dir = self
            .state
            .log_archive_dir()
            .ok_or_else(|| state_err("Log archive unavailable"))?;
        let limit = args.limit.clamp(1, 200);
        let mut segments =
            duthub_core::log_archive::list(&dir, &args.dut_id, args.offset, limit + 1);
        let next_offset = (segments.len() > limit).then(|| args.offset.saturating_add(limit));
        segments.truncate(limit);
        let segments: Vec<serde_json::Value> = segments
            .into_iter()
            .map(|seg| {
                let mut val = serde_json::to_value(&seg).unwrap_or(serde_json::Value::Null);
                val["resourceUri"] =
                    serde_json::json!(format!("duthub://dut/{}/logs/{}", args.dut_id, seg.id));
                val
            })
            .collect();
        Ok(Json(
            serde_json::json!({ "segments": segments, "nextOffset": next_offset }),
        ))
    }

    #[tool(
        description = "读取已落盘的 DUT 日志段。segmentId 来自 log_segments，offset 使用上次返回的 nextOffset。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn log_segment_read(
        &self,
        Parameters(args): Parameters<LogSegmentReadArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_scoped_read(&principal.0, &args.dut_id)?;
        let dir = self
            .state
            .log_archive_dir()
            .ok_or_else(|| state_err("Log archive unavailable"))?;
        let mut page =
            duthub_core::log_archive::read(&dir, &args.dut_id, &args.segment_id, args.offset)
                .map_err(|err| state_err(&err.to_string()))?;
        page.text = crate::keys::strip_ansi_codes(&page.text);
        Ok(Json(serde_json::json!(page)))
    }

    #[tool(
        description = "读取工作目录内各 DUT 的资料目录。省略 dutId 时列出 DUT（DUT 专用端点默认到绑定 DUT），指定 dutId 时分页列出该目录内的普通文件，支持按 category（image/TEST/压力&老化/串口日志/misc/feedback）过滤。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn dut_files(
        &self,
        Parameters(args): Parameters<DutFilesArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        let limit = args.limit.clamp(1, 200);
        let category_filter = match args
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(cat) => {
                if !duthub_core::dut_artifacts::DUT_CATEGORY_DIRS.contains(&cat) {
                    return Err(rmcp::ErrorData::invalid_params(
                        format!(
                            "category must be one of: {}",
                            duthub_core::dut_artifacts::DUT_CATEGORY_DIRS.join(", ")
                        ),
                        Some(serde_json::json!({ "code": "INVALID_CATEGORY" })),
                    ));
                }
                Some(cat.to_owned())
            }
            None => None,
        };
        // Default to the listener's own DUT when the caller omits the ID.
        if let Some(dut_id) = self.scoped_dut(args.dut_id.as_deref()) {
            self.authorize_scoped_read(&principal.0, dut_id)?;
            let root = self
                .state
                .dut_artifact_root()
                .ok_or_else(|| state_err("DUT 资料目录不可用"))?;
            let dut = self
                .state
                .dut_info(dut_id)
                .ok_or_else(|| state_err("DUT 不存在"))?;
            let mut files =
                match duthub_core::dut_artifacts::list_files(&root, &dut.dut_id, &dut.name) {
                    Ok(files) => files,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(error) => return Err(state_err(&error.to_string())),
                };
            if let Some(ref cat) = category_filter {
                let prefix = format!("{cat}/");
                files.retain(|file| file.id.starts_with(&prefix));
            }
            let next =
                (files.len() > args.offset.saturating_add(limit)).then(|| args.offset + limit);
            let files: Vec<serde_json::Value> = files
                .into_iter()
                .skip(args.offset)
                .take(limit)
                .map(|file| {
                    let top_category = file.id.split_once('/').and_then(|(top, _)| {
                        duthub_core::dut_artifacts::DUT_CATEGORY_DIRS
                            .contains(&top)
                            .then_some(top)
                    });
                    let mut val = serde_json::to_value(&file).unwrap_or(serde_json::Value::Null);
                    val["resourceUri"] =
                        serde_json::json!(format!("duthub://dut/{dut_id}/files/{}", file.id));
                    if let Some(cat) = top_category {
                        val["category"] = serde_json::json!(cat);
                    }
                    val
                })
                .collect();
            return Ok(Json(serde_json::json!({
                "files": files,
                "categories": duthub_core::dut_artifacts::DUT_CATEGORY_DIRS,
                "nextOffset": next,
            })));
        }
        // A general listener lists eligible DUTs when no specific DUT was requested.
        // The credential still needs a read grant.
        self.authorize_any_read(&principal.0)?;
        let mut duts = self.state.list_duts();
        duts.sort_unstable_by(|left, right| left.dut_id.cmp(&right.dut_id));
        let next = (duts.len() > args.offset.saturating_add(limit)).then(|| args.offset + limit);
        let duts: Vec<_> = duts
            .into_iter()
            .skip(args.offset)
            .take(limit)
            .map(|dut| serde_json::json!({ "dutId": dut.dut_id, "name": dut.name }))
            .collect();
        Ok(Json(
            serde_json::json!({ "duts": duts, "nextOffset": next }),
        ))
    }

    #[tool(
        description = "只读获取工作目录内 DUT 文件的分块。fileId 来自 dut_files，返回 Base64 内容和续读偏移。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn dut_file_read(
        &self,
        Parameters(args): Parameters<DutFileReadArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_scoped_read(&principal.0, &args.dut_id)?;
        let root = self
            .state
            .dut_artifact_root()
            .ok_or_else(|| state_err("DUT 资料目录不可用"))?;
        let dut = self
            .state
            .dut_info(&args.dut_id)
            .ok_or_else(|| state_err("DUT 不存在"))?;
        let (bytes, next, eof) = duthub_core::dut_artifacts::read_file(
            &root,
            &dut.dut_id,
            &dut.name,
            &args.file_id,
            args.offset,
        )
        .map_err(|error| state_err(&error.to_string()))?;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        Ok(Json(
            serde_json::json!({ "base64": data, "nextOffset": next, "eof": eof }),
        ))
    }

    #[tool(
        description = "获取 DUT 独占控制租约（必须传 sessionName 登记会话名），记录开始占用时间 acquiredAtUnixMs 并按 expiresAtUnixMs 处理到期。忙时可设置 wait 和 deadlineMs（最多 120000）；等待超时返回 DUT_BUSY。"
    )]
    async fn dut_acquire_control(
        &self,
        Parameters(args): Parameters<AcquireControlArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        let session_name =
            duthub_core::lease::validate_session_name(&args.session_name).map_err(|msg| {
                rmcp::ErrorData::invalid_params(
                    msg,
                    Some(serde_json::json!({ "code": "SESSION_NAME_REQUIRED" })),
                )
            })?;
        self.authorize_any_write(&principal.0, &args.dut_id)?;
        let holder = holder_for(&principal.0);
        let wait_ms = if args.wait { args.deadline_ms } else { 0 };
        if wait_ms > MAX_LEASE_WAIT_MS {
            return Err(rmcp::ErrorData::invalid_params(
                "deadlineMs 不能超过 120000",
                Some(serde_json::json!({ "code": "LEASE_WAIT_TOO_LONG" })),
            ));
        }
        let grant = if wait_ms == 0 {
            self.state
                .dut_acquire_control_with_session(&args.dut_id, holder, &session_name, 0)
        } else {
            let permit = self.lease_waits.clone().try_acquire_owned().map_err(|_| {
                rmcp::ErrorData::new(
                    rmcp::model::ErrorCode(-32000),
                    "控制租约等待队列已满，请稍后重试",
                    Some(serde_json::json!({ "kind": "lease", "code": "DUT_BUSY" })),
                )
            })?;
            let state = self.state.clone();
            let dut_id = args.dut_id.clone();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                state.dut_acquire_control_with_session(&dut_id, holder, &session_name, wait_ms)
            })
            .await
            .map_err(|_| state_err("Lease wait worker failed"))?
        }
        .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({
            "leaseId": grant.lease_id,
            "dutId": grant.dut_id,
            "generation": grant.generation,
            "expiresAtUnixMs": grant.expires_at_unix_ms,
            "acquiredAtUnixMs": grant.acquired_at_unix_ms,
            "sessionName": grant.session_name,
            "holder": grant.holder,
            "queued": wait_ms > 0,
        })))
    }

    #[tool(
        description = "续租控制租约（TTL 重置，generation 不变；持有者本人）：过期/撤销返回 LeaseRequired/LEASE_EXPIRED，Protected 返回 DUT_PROTECTED"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn dut_renew_control(
        &self,
        Parameters(args): Parameters<LeaseIdArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_any_write(&principal.0, &args.dut_id)?;
        self.ensure_holder_owned(&principal.0, &args.dut_id, &args.lease_id)?;
        let grant = self
            .state
            .dut_renew_control(&args.dut_id, &args.lease_id)
            .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({
            "leaseId": grant.lease_id,
            "dutId": grant.dut_id,
            "generation": grant.generation,
            "expiresAtUnixMs": grant.expires_at_unix_ms,
        })))
    }

    #[tool(
        description = "释放控制租约（持有者本人；Protected 中拒 → DUT_PROTECTED）：释放后其他 Agent/GUI 可立即取得控制权"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn dut_release_control(
        &self,
        Parameters(args): Parameters<LeaseIdArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_any_write(&principal.0, &args.dut_id)?;
        self.ensure_holder_owned(&principal.0, &args.dut_id, &args.lease_id)?;
        self.state
            .dut_release_control(&args.dut_id, &args.lease_id)
            .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }

    #[tool(
        description = "只读查询 DUT 控制权。响应字段有 state、holder、remaining_secs、generation、queue_len、protected_op。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn dut_control_status(
        &self,
        Parameters(args): Parameters<DutInfoArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(&principal.0, &args.dut_id)?;
        let status = self
            .state
            .dut_control_status(&args.dut_id)
            .map_err(|e| state_err(&e))?;
        Ok(Json(
            serde_json::to_value(status).unwrap_or(serde_json::Value::Null),
        ))
    }

    #[tool(
        name = "serial_write",
        description = "向 DUT 串口发送数据。认证开启时需 leaseId。每个新操作传唯一 requestId，超时后五分钟内以同一个 requestId 和 text 查询或重试。queued 仅表示已入队，请通过 serial_read 核对设备输出。"
    )]
    pub fn serial_write_tool(
        &self,
        Parameters(request): Parameters<SerialWriteRequest>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        let Some(id) = request.request_id else {
            return self.serial_write(Parameters(request.args), principal);
        };
        if id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(rmcp::ErrorData::invalid_params(
                "requestId 须为 1–128 个可显示 ASCII 字符",
                None,
            ));
        }
        self.authorize(&principal.0, &request.args.dut_id, DutAction::SerialWrite)?;
        // Validate lease ownership BEFORE returning any cached write receipt
        // so a revoked or non-holding caller cannot probe/replay receipts.
        let _identity = self.lease_identity(
            &principal.0,
            &request.args.dut_id,
            request.args.lease_id.as_deref(),
        )?;
        let credential = match &principal.0 {
            Principal::Agent { credential_id, .. } => format!("agent:{credential_id}"),
            Principal::LocalAdmin => "local-admin".into(),
        };
        let key = (credential, request.args.dut_id.clone(), id.clone());
        let digest: [u8; 32] = Sha256::digest(request.args.text.as_bytes()).into();
        let mut receipts = duthub_core::util::lock_unpoisoned(&self.write_receipts);
        receipts.retain(|_, receipt| receipt.accepted_at.elapsed() < WRITE_RECEIPT_TTL);
        if let Some(receipt) = receipts.get(&key) {
            if receipt.text_digest != digest {
                return Err(rmcp::ErrorData::invalid_params(
                    "requestId 已用于不同的串口内容",
                    Some(serde_json::json!({ "code": "REQUEST_ID_CONFLICT" })),
                ));
            }
            return Ok(Json(serde_json::json!({
                "ok": true, "status": "queued", "requestId": id, "replayed": true
            })));
        }
        if receipts.len() >= WRITE_RECEIPT_LIMIT {
            return Err(rmcp::ErrorData::new(
                rmcp::model::ErrorCode(-32000),
                "串口回执容量已满，请稍后重试",
                Some(serde_json::json!({ "code": "RECEIPT_BUSY" })),
            ));
        }
        self.serial_write(Parameters(request.args), principal)?;
        receipts.insert(
            key,
            WriteReceipt {
                text_digest: digest,
                accepted_at: Instant::now(),
            },
        );
        Ok(Json(serde_json::json!({
            "ok": true, "status": "queued", "requestId": id, "replayed": false
        })))
    }

    // Retain the typed direct entry point for local integrations.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "matches rmcp #[tool] extractor signature for direct callers"
    )]
    pub fn serial_write(
        &self,
        Parameters(args): Parameters<SerialWriteArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::SerialWrite)?;
        let identity = self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        if let Some(token) = args.approval_token.as_deref().map(str::trim) {
            let args_json = serde_json::json!({ "text": args.text }).to_string();
            if let Err(err) =
                self.state
                    .approval_validate(token, &args.dut_id, "serial.write", &args_json)
            {
                return Err(approval_error(&err));
            }
        } else if args.lease_id.is_none() {
            return Err(rmcp::ErrorData::invalid_params(
                "ApprovalRequired/Missing: 认证关闭且无租约时，先调用 approval_request 或获取控制租约。",
                Some(serde_json::json!({ "kind": "approval", "code": "MISSING" })),
            ));
        }
        self.state
            .serial_write(&args.dut_id, &args.text, identity)
            .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({ "ok": true, "status": "queued" })))
    }

    #[tool(
        description = "从已运行的本机 ADB 服务读取设备列表。Agent 只会看到绑定到其可读 DUT 的 ADB 序列号。"
    )]
    async fn adb_list_devices(
        &self,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_any_read(&principal.0)?;
        let allowed = self.readable_adb_serials(&principal.0);
        if allowed
            .as_ref()
            .is_some_and(std::collections::HashSet::is_empty)
        {
            return Ok(Json(serde_json::json!({ "devices": [] })));
        }
        let permit = self
            .adb_discovery
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                rmcp::ErrorData::new(
                    rmcp::model::ErrorCode(-32000),
                    "ADB 枚举进行中",
                    Some(serde_json::json!({ "code": "DISCOVERY_BUSY" })),
                )
            })?;
        let state = self.state.clone();
        let devices: Vec<serde_json::Value> = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            state.discover_adb_devices()
        })
        .await
        .map_err(|_| state_err("ADB discovery worker failed"))?
        .map_err(|e| state_err(&e))?
        .into_iter()
        .filter(|device| {
            allowed
                .as_ref()
                .is_none_or(|serials| serials.contains(&device.serial))
        })
        .map(|d| serde_json::to_value(d).unwrap_or(serde_json::Value::Null))
        .collect();
        Ok(Json(serde_json::json!({ "devices": devices })))
    }

    #[tool(
        description = "ADB Shell。仅在宿主已启用时可用；桌面版请使用 DUT 右侧 ADB/HDC Shell。收到 TOOL_UNAVAILABLE 时停止调用。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn adb_shell(
        &self,
        Parameters(args): Parameters<AdbShellArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::Adb)?;
        self.require_adb_shell()?;
        let identity = self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        let args_json = serde_json::json!({ "argv": args.argv }).to_string();
        if let Err(err) = self.state.approval_validate(
            &args.approval_token,
            &args.dut_id,
            "adb.shell",
            &args_json,
        ) {
            return Err(approval_error(&err));
        }
        let result = self
            .state
            .adb_shell(&args.dut_id, &args.argv, identity)
            .map_err(|e| state_err(&e))?;
        Ok(Json(
            serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
        ))
    }

    #[tool(
        description = "DUT 电源通断（控制单元动作；高危动作：审批令牌必填作二次确认，auth 开启时还需 leaseId 控制租约；PDU 仅支持 Power）"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn control_power(
        &self,
        Parameters(args): Parameters<ControlPowerArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::Power)?;
        let identity = self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        // The shared device entry point verifies and consumes approval.
        // A failed lease check leaves the approval token unconsumed.
        self.state
            .control_power(&args.dut_id, args.on, &args.approval_token, identity)
            .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }

    #[tool(
        description = "DUT 电源按键（继电器脉冲 PWR_KEY；long_press=true 为长按强制断电；审批令牌必填二次确认，auth 开启时还需 leaseId）"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn control_power_key(
        &self,
        Parameters(args): Parameters<ControlPowerKeyArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::Power)?;
        let identity = self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        self.state
            .control_power_key(
                &args.dut_id,
                args.long_press,
                &args.approval_token,
                identity,
            )
            .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }

    #[tool(
        description = "DUT 复位（USB 继电器专用；审批令牌必填二次确认，auth 开启时还需 leaseId；PDU 不支持 Reset）"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn control_reset(
        &self,
        Parameters(args): Parameters<ControlResetArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::Reset)?;
        let identity = self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        self.state
            .control_reset(&args.dut_id, &args.approval_token, identity)
            .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }

    #[tool(
        description = "继电器逐通道状态查询（只读，无需审批；返回应答 STATUS 的通道及 ON/OFF，板卡不回答 STATUS 时为空表）"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn relay_status(
        &self,
        Parameters(args): Parameters<DutInfoArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(&principal.0, &args.dut_id)?;
        let channels = self
            .state
            .relay_status(&args.dut_id)
            .map_err(|e| state_err(&e))?;
        let channels: Vec<serde_json::Value> = channels
            .into_iter()
            .map(|(channel, on)| serde_json::json!({ "channel": channel, "on": on }))
            .collect();
        Ok(Json(serde_json::json!({
            "dutId": args.dut_id,
            "channels": channels,
        })))
    }

    #[tool(
        description = "继电器单通道往返测试（ON→STATUS 确认吸合→OFF→STATUS 确认恢复；写动作：审批令牌 action=control.test 必填二次确认（参数绑定 {\"channel\": N}），auth 开启时还需 leaseId；无论断言是否通过都会恢复 OFF）"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn relay_test(
        &self,
        Parameters(args): Parameters<RelayTestArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::Relay)?;
        let identity = self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        // The relay test entry point validates both the lease and approval.
        // It consumes the token only after authorization succeeds.
        let report = self
            .state
            .relay_test(&args.dut_id, args.channel, &args.approval_token, identity)
            .map_err(|e| state_err(&e))?;
        let ok = report.on_ok && report.off_ok;
        Ok(Json(serde_json::json!({
            "dutId": args.dut_id,
            "channel": args.channel,
            "ok": ok,
            "before": report.before,
            "onOk": report.on_ok,
            "offOk": report.off_ok,
            "after": report.after,
        })))
    }

    #[tool(
        description = "查询 DUT 会话心跳：当前配置（enabled/interval_secs/command/loss_threshold）+ 最近一次状态（last_ok/丢联计数/响应原文/系统信息 uptime/mem/cpu/hostname；last_ok=null 表示尚未探测）"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn heartbeat_get(
        &self,
        Parameters(args): Parameters<DutInfoArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(&principal.0, &args.dut_id)?;
        let view = self
            .state
            .heartbeat_get(&args.dut_id)
            .map_err(|e| state_err(&e))?;
        Ok(Json(
            serde_json::to_value(view).unwrap_or(serde_json::Value::Null),
        ))
    }

    #[tool(
        description = "设置 DUT 心跳命令、间隔及响应匹配；enabled=false 持续停用，pauseSecs 暂停指定秒数，0 恢复。需串口写授权，开启认证时需 leaseId。返回当前配置与最近探测状态"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn heartbeat_set(
        &self,
        Parameters(args): Parameters<HeartbeatSetArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::SerialWrite)?;
        self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        if args
            .update
            .command
            .as_ref()
            .is_some_and(|command| command.trim().is_empty())
        {
            return Err(rmcp::ErrorData::invalid_params(
                "command 不能为空（如 \"uname -a\"）",
                None,
            ));
        }
        if args
            .update
            .interval_secs
            .is_some_and(|secs| !(1..=86_400).contains(&secs))
        {
            return Err(rmcp::ErrorData::invalid_params(
                "intervalSecs 必须在 1–86400 秒之间",
                None,
            ));
        }
        let view = self
            .state
            .heartbeat_set(&args.dut_id, args.update)
            .map_err(|e| state_err(&e))?;
        Ok(Json(
            serde_json::to_value(view).unwrap_or(serde_json::Value::Null),
        ))
    }

    #[tool(
        description = "向 DUT 会话日志写入 [mark] {label}。分段由 DUT 的日志模式决定，不发送串口数据。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn log_mark(
        &self,
        Parameters(args): Parameters<DutLabelArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(&principal.0, &args.dut_id)?;
        self.state
            .log_mark(&args.dut_id, &args.label)
            .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }

    #[tool(description = "烧录工具只读测试预留入口。桌面版请在设备工具的烧录预设中运行测试。")]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn flash_test(
        &self,
        Parameters(args): Parameters<FlashTestArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        // The allowlisted flash probe uses read permission.
        self.authorize_any_read(&principal.0)?;
        let result = self
            .state
            .flash_test(&args.preset_id)
            .map_err(|e| state_err(&e))?;
        Ok(Json(
            serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
        ))
    }

    #[tool(
        description = "为已获操作者许可的动作签发一次性参数绑定令牌，五分钟有效。启用认证时，审批令牌不能代替控制租约。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn approval_request(
        &self,
        Parameters(args): Parameters<ApprovalRequestArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        // Keep issuable approval actions aligned with the shared control entry points.
        // Control actions validate and consume their own approval parameters.
        // Channel tests use the dedicated control test action.
        let permission = match args.action.as_str() {
            "serial.write" => DutAction::SerialWrite,
            "adb.shell" => DutAction::Adb,
            "control.power" | "control.power_key" => DutAction::Power,
            "control.reset" => DutAction::Reset,
            "control.test" => DutAction::Relay,
            _ => {
                return Err(rmcp::ErrorData::invalid_params(
                    "action 只支持 serial.write、adb.shell、control.power、control.power_key、control.reset、control.test",
                    None,
                ));
            }
        };
        self.authorize(&principal.0, &args.dut_id, permission)?;
        if permission == DutAction::Adb {
            self.require_adb_shell()?;
        }
        let args_json = args.args.to_string();
        let token = self
            .state
            .approval_issue(&args.dut_id, &args.action, &args_json);
        Ok(Json(
            serde_json::to_value(token).unwrap_or(serde_json::Value::Null),
        ))
    }

    #[tool(
        description = "只读证据包：返回 DUT 会话最近日志与元数据，供客户端 LLM 分析（不下诊断结论）"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn analyze_serial(
        &self,
        Parameters(args): Parameters<DutInfoArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(&principal.0, &args.dut_id)?;
        let Some(result) = self.state.serial_read(&args.dut_id, 0, usize::MAX) else {
            return Err(rmcp::ErrorData::invalid_params(
                format!("DUT 会话不存在: {}（会话未打开）", args.dut_id),
                None,
            ));
        };
        let start = result.lines.len().saturating_sub(150);
        Ok(Json(serde_json::json!({
            "session_id": result.session_id,
            "evidence_lines": &result.lines[start..],
            "note": "只读证据包：结论由客户端 LLM 依据证据生成，duthub 不下诊断结论"
        })))
    }

    #[tool(description = "读取当前监听器的 MCP 使用说明 SKILL.md，返回 Markdown 正文。")]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn skill_export(
        &self,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        use std::fmt::Write as _;

        // HTTP credentials without grants cannot export the skill. Stdio local access still works.
        self.authorize_any_read(&principal.0)?;
        let mut markdown = crate::skill::skill_for_endpoint(self.listener_endpoint);
        if let Some(bound) = self.listener_dut.as_deref() {
            let _ = writeln!(
                markdown,
                "\n> 本端点仅可读 DUT `{bound}`；跨 DUT 请求返回 `LeaseRequired/UNAUTHORIZED`，且不开放写工具。"
            );
        }
        Ok(Json(
            serde_json::json!({ "ok": true, "markdown": markdown }),
        ))
    }

    /// Raise a notice in the duthub UI. This is not a device action: it needs
    /// no approval token and no lease. It is available in Plan mode because it
    /// cannot change the DUT — but it does surface UI, so the host throttles
    /// and suppresses it like any other alert.
    #[tool(
        name = "notify_user",
        description = "向 duthub 用户界面发送一条通知。不触设备、不写串口、无需审批。message 必填（去控制字符后 ≤200 字符），title 可选（≤48 字符），level 取 info|warning|error（其他值报 invalid_params），dutId 可选。返回 outcome=queued|throttled|suppressed|disabled 与 queued 布尔值：queued 仅表示已进入待呈现队列，不代表用户已看到。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn notify_user(
        &self,
        Parameters(args): Parameters<NotifyUserArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        // Validate parameters before authorization so a malformed call never
        // reaches the host. An unknown level is rejected, never downgraded.
        let level = match args.level.as_deref() {
            None => NotifyLevel::Info,
            Some(raw) => NotifyLevel::parse(raw).ok_or_else(|| {
                rmcp::ErrorData::invalid_params(
                    "level 只支持 info、warning、error",
                    Some(serde_json::json!({ "code": "INVALID_LEVEL" })),
                )
            })?,
        };
        let message = sanitize_notify_text(&args.message, MESSAGE_MAX_CHARS);
        if message.is_empty() {
            return Err(rmcp::ErrorData::invalid_params(
                "message 不能为空",
                Some(serde_json::json!({ "code": "MESSAGE_REQUIRED" })),
            ));
        }
        let title = args
            .title
            .as_deref()
            .map(|title| sanitize_notify_text(title, TITLE_MAX_CHARS))
            .filter(|title| !title.is_empty());

        // Authorization follows the explicit target DUT, falling back to the
        // listener binding. `authorize_scoped_read` is not enough here: on a
        // generic listener it degrades to any-read and would not check an
        // explicit `dutId`, letting a token for DUT A label a notice as DUT B.
        let target_dut = self.scoped_dut(args.dut_id.as_deref()).map(str::to_owned);
        match target_dut.as_deref() {
            Some(dut_id) => self.authorize_read(&principal.0, dut_id)?,
            None => self.authorize_any_read(&principal.0)?,
        }

        let Some(notifier) = self.notifier.clone() else {
            // Unreachable through `tools/list`/`call_tool`: a listener without
            // a notifier does not expose this route. Kept as an honest answer
            // for direct callers.
            return Ok(Json(notify_response(NotifyOutcome::Disabled)));
        };
        let credential_id = match &principal.0 {
            Principal::Agent { credential_id, .. } => Some(credential_id.clone()),
            Principal::LocalAdmin => None,
        };
        let outcome = notifier.notify(&NotifyRequest {
            message,
            title,
            level,
            dut_id: target_dut,
            credential_id,
        });
        Ok(Json(notify_response(outcome)))
    }

    #[tool(
        name = "agent_feedback",
        description = "记录 Agent 使用反馈到 feedback 目录，便于后续优化 duthub 工具与流程（仅用户配置监听器开放，参数 camelCase）。category 取 bug | usability | missing_feature | performance | doc | other；severity 取 low | medium | high（默认 medium）；title 1–120 字符；detail 1–8192 字符。"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "required by rmcp #[tool] extractor signature"
    )]
    fn agent_feedback(
        &self,
        Parameters(args): Parameters<AgentFeedbackArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        const CATEGORIES: &[&str] = &[
            "bug",
            "usability",
            "missing_feature",
            "performance",
            "doc",
            "other",
        ];
        const SEVERITIES: &[&str] = &["low", "medium", "high"];
        let category = args.category.trim().to_ascii_lowercase();
        if !CATEGORIES.contains(&category.as_str()) {
            return Err(rmcp::ErrorData::invalid_params(
                "category must be one of: bug, usability, missing_feature, performance, doc, other",
                Some(serde_json::json!({ "code": "INVALID_FEEDBACK_CATEGORY" })),
            ));
        }
        let severity = match args.severity.as_deref() {
            None => "medium".to_owned(),
            Some(raw) => {
                let s = raw.trim().to_ascii_lowercase();
                if !SEVERITIES.contains(&s.as_str()) {
                    return Err(rmcp::ErrorData::invalid_params(
                        "severity must be one of: low, medium, high",
                        Some(serde_json::json!({ "code": "INVALID_FEEDBACK_SEVERITY" })),
                    ));
                }
                s
            }
        };
        let title = args.title.trim();
        if title.is_empty() || title.chars().count() > 120 || title.chars().any(char::is_control) {
            return Err(rmcp::ErrorData::invalid_params(
                "title must be 1..=120 non-control characters",
                Some(serde_json::json!({ "code": "INVALID_FEEDBACK_TITLE" })),
            ));
        }
        let detail = args.detail.trim();
        if detail.is_empty() || detail.chars().count() > 8192 {
            return Err(rmcp::ErrorData::invalid_params(
                "detail must be 1..=8192 characters",
                Some(serde_json::json!({ "code": "INVALID_FEEDBACK_DETAIL" })),
            ));
        }
        let tool_name = args
            .tool_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(truncate_audit_field);
        let session_name = match args.session_name.as_deref() {
            Some(raw) if !raw.trim().is_empty() => Some(
                duthub_core::lease::validate_session_name(raw).map_err(|msg| {
                    rmcp::ErrorData::invalid_params(
                        msg,
                        Some(serde_json::json!({ "code": "INVALID_SESSION_NAME" })),
                    )
                })?,
            ),
            _ => None,
        };

        let target_dut = self.scoped_dut(args.dut_id.as_deref()).map(str::to_owned);
        if let Some(dut_id) = target_dut.as_deref() {
            self.authorize_scoped_read(&principal.0, dut_id)?;
        } else {
            self.authorize_any_read(&principal.0)?;
        }

        let root = self
            .state
            .dut_artifact_root()
            .ok_or_else(|| state_err("DUT 资料目录不可用"))?;
        let now_ms = duthub_core::audit::unix_ms();
        let author = match &principal.0 {
            Principal::Agent { agent_name, .. } => agent_name.as_str(),
            Principal::LocalAdmin => "local-admin",
        };
        let slug: String = title
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
            .take(32)
            .collect();
        let seq = FEEDBACK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let filename = if slug.is_empty() {
            format!("feedback-{now_ms}-{seq:04}.md")
        } else {
            format!("feedback-{now_ms}-{slug}-{seq:04}.md")
        };
        let mut md = String::new();
        md.push_str("---\n");
        let _ = std::fmt::Write::write_fmt(&mut md, format_args!("timestampUnixMs: {now_ms}\n"));
        let _ = std::fmt::Write::write_fmt(&mut md, format_args!("category: {category}\n"));
        let _ = std::fmt::Write::write_fmt(&mut md, format_args!("severity: {severity}\n"));
        let _ = std::fmt::Write::write_fmt(
            &mut md,
            format_args!("author: {}\n", truncate_audit_field(author)),
        );
        if let Some(dut_id) = &target_dut {
            let _ = std::fmt::Write::write_fmt(
                &mut md,
                format_args!("dutId: {}\n", truncate_audit_field(dut_id)),
            );
        }
        if let Some(tool) = &tool_name {
            let _ = std::fmt::Write::write_fmt(&mut md, format_args!("toolName: {tool}\n"));
        }
        if let Some(session) = &session_name {
            let _ = std::fmt::Write::write_fmt(&mut md, format_args!("sessionName: {session}\n"));
        }
        md.push_str("---\n\n");
        let _ = std::fmt::Write::write_fmt(&mut md, format_args!("# {title}\n\n{detail}\n"));

        let (file_id, resource_uri) = if let Some(dut_id) = &target_dut {
            let dut = self
                .state
                .dut_info(dut_id)
                .ok_or_else(|| state_err("DUT 不存在"))?;
            let (file_id, _) = duthub_core::dut_artifacts::write_feedback_file(
                &root,
                &dut.dut_id,
                &dut.name,
                &filename,
                md.as_bytes(),
            )
            .map_err(|error| state_err(&error.to_string()))?;
            let uri = format!("duthub://dut/{dut_id}/files/{file_id}");
            (file_id, Some(uri))
        } else {
            let (file_id, _) = duthub_core::dut_artifacts::write_global_feedback_file(
                &root,
                &filename,
                md.as_bytes(),
            )
            .map_err(|error| state_err(&error.to_string()))?;
            (file_id, None)
        };

        Ok(Json(serde_json::json!({
            "ok": true,
            "dutId": target_dut,
            "category": category,
            "severity": severity,
            "fileId": file_id,
            "resourceUri": resource_uri,
            "timestampUnixMs": now_ms,
        })))
    }

    #[tool(
        name = "dut_control",
        description = "统一原子硬件控制、多 Agent 租约仲裁与受控烧录（仅用户配置监听器开放，参数 camelCase）：action 支持 power_on | power_off | power_cycle | reset | power_key_short | power_key_long | enter_maskrom | enter_loader | relay_test | flash_list | flash_update | flash_reboot | acquire_lease | renew_lease | release_lease。多 Agent 并发时自动 FIFO 排队（waitMs 默认 5000ms），跨步独占可先 acquire_lease 再传 leaseId；无法获取使用权时默认向桌面发人工干预提醒并附带具体原因（notifyOnBlocked=false 可静默）。"
    )]
    async fn dut_control(
        &self,
        Parameters(args): Parameters<DutControlArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        let action_label = format!("dut_control({})", args.action);
        if args.action == "acquire_lease" {
            let raw_session = args.session_name.as_deref().unwrap_or_default();
            let session_name =
                duthub_core::lease::validate_session_name(raw_session).map_err(|msg| {
                    rmcp::ErrorData::invalid_params(
                        format!("{msg} (sessionName is required for action=acquire_lease)"),
                        Some(serde_json::json!({ "code": "SESSION_NAME_REQUIRED" })),
                    )
                })?;
            self.authorize_any_write(&principal.0, &args.dut_id)
                .map_err(|err| {
                    self.attach_blocked_notice(
                        err,
                        &principal.0,
                        &args.dut_id,
                        &action_label,
                        0,
                        args.notify_on_blocked,
                    )
                })?;
            let holder = holder_for(&principal.0);
            let wait_ms = args
                .wait_ms
                .unwrap_or(DEFAULT_SCOPED_LEASE_WAIT_MS)
                .min(MAX_LEASE_WAIT_MS);
            let started = Instant::now();
            let grant_result = if wait_ms == 0 {
                self.state
                    .dut_acquire_control_with_session(&args.dut_id, holder, &session_name, 0)
            } else {
                let Ok(permit) = self.lease_waits.clone().try_acquire_owned() else {
                    let err = rmcp::ErrorData::new(
                        rmcp::model::ErrorCode(-32000),
                        "LeaseRequired/DUT_BUSY: lease wait queue is full; retry later",
                        Some(serde_json::json!({ "kind": "lease", "code": "DUT_BUSY" })),
                    );
                    return Err(self.attach_blocked_notice(
                        err,
                        &principal.0,
                        &args.dut_id,
                        &action_label,
                        0,
                        args.notify_on_blocked,
                    ));
                };
                let state = self.state.clone();
                let dut = args.dut_id.clone();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    state.dut_acquire_control_with_session(&dut, holder, &session_name, wait_ms)
                })
                .await
                .map_err(|_| state_err("Lease wait worker failed"))?
            };
            let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let grant = grant_result.map_err(|msg| {
                self.attach_blocked_notice(
                    state_err(&msg),
                    &principal.0,
                    &args.dut_id,
                    &action_label,
                    waited_ms,
                    args.notify_on_blocked,
                )
            })?;
            return Ok(Json(serde_json::json!({
                "ok": true,
                "dutId": grant.dut_id,
                "action": "acquire_lease",
                "leaseId": grant.lease_id,
                "generation": grant.generation,
                "expiresAtUnixMs": grant.expires_at_unix_ms,
                "acquiredAtUnixMs": grant.acquired_at_unix_ms,
                "sessionName": grant.session_name,
                "holder": grant.holder,
                "waitedMs": waited_ms,
            })));
        }
        if args.action == "renew_lease" {
            self.authorize_any_write(&principal.0, &args.dut_id)
                .map_err(|err| {
                    self.attach_blocked_notice(
                        err,
                        &principal.0,
                        &args.dut_id,
                        &action_label,
                        0,
                        args.notify_on_blocked,
                    )
                })?;
            let Some(lease_id) = args
                .lease_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Err(rmcp::ErrorData::invalid_params(
                    "leaseId is required for action=renew_lease",
                    None,
                ));
            };
            self.ensure_holder_owned(&principal.0, &args.dut_id, lease_id)
                .map_err(|err| {
                    self.attach_blocked_notice(
                        err,
                        &principal.0,
                        &args.dut_id,
                        &action_label,
                        0,
                        args.notify_on_blocked,
                    )
                })?;
            let grant = self
                .state
                .dut_renew_control(&args.dut_id, lease_id)
                .map_err(|msg| {
                    self.attach_blocked_notice(
                        state_err(&msg),
                        &principal.0,
                        &args.dut_id,
                        &action_label,
                        0,
                        args.notify_on_blocked,
                    )
                })?;
            return Ok(Json(serde_json::json!({
                "ok": true,
                "dutId": grant.dut_id,
                "action": "renew_lease",
                "leaseId": grant.lease_id,
                "generation": grant.generation,
                "expiresAtUnixMs": grant.expires_at_unix_ms,
                "acquiredAtUnixMs": grant.acquired_at_unix_ms,
                "sessionName": grant.session_name,
                "holder": grant.holder,
            })));
        }
        if args.action == "release_lease" {
            self.authorize_any_write(&principal.0, &args.dut_id)?;
            let Some(lease_id) = args
                .lease_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Err(rmcp::ErrorData::invalid_params(
                    "leaseId is required for action=release_lease",
                    None,
                ));
            };
            self.ensure_holder_owned(&principal.0, &args.dut_id, lease_id)?;
            self.state
                .dut_release_control(&args.dut_id, lease_id)
                .map_err(|e| state_err(&e))?;
            return Ok(Json(serde_json::json!({
                "ok": true,
                "dutId": args.dut_id,
                "action": "release_lease",
                "released": true,
            })));
        }

        let required_permission = match args.action.as_str() {
            "power_on" | "power_off" | "power_cycle" | "power_key_short" | "power_key_long" => {
                DutAction::Power
            }
            "reset" => DutAction::Reset,
            "enter_maskrom" | "enter_loader" | "relay_test" => DutAction::Relay,
            "flash_list" => DutAction::Read,
            "flash_update" | "flash_reboot" => DutAction::Flash,
            _ => {
                return Err(rmcp::ErrorData::invalid_params(
                    "action must be one of: power_on, power_off, power_cycle, reset, power_key_short, power_key_long, enter_maskrom, enter_loader, relay_test, flash_list, flash_update, flash_reboot, acquire_lease, renew_lease, release_lease",
                    Some(serde_json::json!({ "code": "INVALID_ACTION" })),
                ));
            }
        };
        if args.action == "flash_list" {
            self.authorize_read(&principal.0, &args.dut_id)?;
            let state = self.state.clone();
            let dut_id = args.dut_id.clone();
            let preset_id = args.preset_id.clone();
            let out = tokio::task::spawn_blocking(move || state.flash_list(preset_id.as_deref()))
                .await
                .map_err(|_| state_err("Flash list worker failed"))?
                .map_err(|err| state_err(&err))?;
            let combined = format!("{}\n{}", out.stdout, out.stderr);
            let in_loader = combined.to_ascii_lowercase().contains("loader");
            let in_maskrom = combined.to_ascii_lowercase().contains("maskrom");
            return Ok(Json(serde_json::json!({
                "ok": out.exit_code == Some(0),
                "dutId": dut_id,
                "action": "flash_list",
                "inLoader": in_loader,
                "inMaskrom": in_maskrom,
                "exitCode": out.exit_code,
                "stdout": out.stdout,
                "stderr": out.stderr,
                "elapsedMs": out.elapsed_ms,
            })));
        }
        self.authorize(&principal.0, &args.dut_id, required_permission)
            .map_err(|err| {
                self.attach_blocked_notice(
                    err,
                    &principal.0,
                    &args.dut_id,
                    &action_label,
                    0,
                    args.notify_on_blocked,
                )
            })?;
        let (identity, _lease_guard, waited_ms) = self
            .acquire_or_use_lease(
                &principal.0,
                &args.dut_id,
                &action_label,
                args.lease_id.as_deref(),
                args.session_name.as_deref(),
                args.wait_ms,
                args.notify_on_blocked,
            )
            .await?;
        let state = self.state.clone();
        let access = self.access.clone();
        let dut_id = args.dut_id.clone();
        let action = args.action.clone();
        let hold_ms = args.hold_ms.unwrap_or(1_500).clamp(200, 10_000);
        let off_ms = args.off_ms.unwrap_or(1_500).clamp(200, 10_000);
        let channel = args.channel;
        let file_id = args.file_id.clone();
        let preset_id = args.preset_id.clone();
        let had_explicit_lease = args
            .lease_id
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());

        let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
            match action.as_str() {
                "power_on" => {
                    let args_json = serde_json::json!({ "on": true }).to_string();
                    let tok = state.approval_issue(&dut_id, "control.power", &args_json);
                    state.control_power(&dut_id, true, &tok.token, identity)?;
                    Ok(serde_json::json!({ "ok": true, "dutId": dut_id, "action": action, "waitedMs": waited_ms }))
                }
                "power_off" => {
                    let args_json = serde_json::json!({ "on": false }).to_string();
                    let tok = state.approval_issue(&dut_id, "control.power", &args_json);
                    state.control_power(&dut_id, false, &tok.token, identity)?;
                    Ok(serde_json::json!({ "ok": true, "dutId": dut_id, "action": action, "waitedMs": waited_ms }))
                }
                "power_cycle" => {
                    let args_json = serde_json::json!({ "offMs": off_ms }).to_string();
                    let tok = state.approval_issue(&dut_id, "control.power_cycle", &args_json);
                    state.control_power_cycle(&dut_id, off_ms, &tok.token, identity)?;
                    Ok(serde_json::json!({ "ok": true, "dutId": dut_id, "action": action, "offMs": off_ms, "waitedMs": waited_ms }))
                }
                "reset" => {
                    let tok = state.approval_issue(&dut_id, "control.reset", "{}");
                    state.control_reset(&dut_id, &tok.token, identity)?;
                    Ok(serde_json::json!({ "ok": true, "dutId": dut_id, "action": action, "waitedMs": waited_ms }))
                }
                "power_key_short" => {
                    let args_json = serde_json::json!({ "long_press": false }).to_string();
                    let tok = state.approval_issue(&dut_id, "control.power_key", &args_json);
                    state.control_power_key(&dut_id, false, &tok.token, identity)?;
                    Ok(serde_json::json!({ "ok": true, "dutId": dut_id, "action": action, "waitedMs": waited_ms }))
                }
                "power_key_long" => {
                    let args_json = serde_json::json!({ "long_press": true }).to_string();
                    let tok = state.approval_issue(&dut_id, "control.power_key", &args_json);
                    state.control_power_key(&dut_id, true, &tok.token, identity)?;
                    Ok(serde_json::json!({ "ok": true, "dutId": dut_id, "action": action, "waitedMs": waited_ms }))
                }
                "enter_maskrom" => {
                    let args_json = serde_json::json!({ "holdMs": hold_ms }).to_string();
                    let tok = state.approval_issue(&dut_id, "control.enter_maskrom", &args_json);
                    state.control_enter_maskrom(&dut_id, hold_ms, &tok.token, identity)?;
                    Ok(serde_json::json!({ "ok": true, "dutId": dut_id, "action": action, "holdMs": hold_ms, "waitedMs": waited_ms }))
                }
                "enter_loader" => {
                    let args_json = serde_json::json!({ "holdMs": hold_ms }).to_string();
                    let tok = state.approval_issue(&dut_id, "control.enter_loader", &args_json);
                    state.control_enter_loader(&dut_id, hold_ms, &tok.token, identity)?;
                    Ok(serde_json::json!({ "ok": true, "dutId": dut_id, "action": action, "holdMs": hold_ms, "waitedMs": waited_ms }))
                }
                "relay_test" => {
                    let Some(ch) = channel.filter(|ch| *ch > 0) else {
                        return Err("channel (>= 1) is required for action=relay_test".into());
                    };
                    let args_json = serde_json::json!({ "channel": ch }).to_string();
                    let tok = state.approval_issue(&dut_id, "control.test", &args_json);
                    let report = state.relay_test(&dut_id, ch, &tok.token, identity)?;
                    let ok = report.on_ok && report.off_ok;
                    Ok(serde_json::json!({
                        "ok": ok,
                        "dutId": dut_id,
                        "action": action,
                        "channel": ch,
                        "before": report.before,
                        "onOk": report.on_ok,
                        "offOk": report.off_ok,
                        "after": report.after,
                        "waitedMs": waited_ms,
                    }))
                }
                "flash_update" => {
                    let Some(fid) = file_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
                        return Err("fileId (plain .img/.bin filename inside the DUT directory) is required for action=flash_update".into());
                    };
                    // Reject obvious traversal before entering Protected state so an
                    // invalid path can never push the DUT into NeedsInspection.
                    if fid.contains('/') || fid.contains('\\') || fid.contains("..") {
                        return Err("fileId must be a plain filename inside the DUT directory".into());
                    }
                    let protected = access
                        .as_ref()
                        .and_then(|ctx| ctx.lease.enter_protected(&dut_id, &format!("flash_update {fid}")).ok())
                        .is_some();
                    let outcome = state.flash_update(&dut_id, fid, preset_id.as_deref(), identity);
                    if protected && let Some(ctx) = &access {
                        let safe_exit = match &outcome {
                            Ok(out) => out.exit_code == Some(0),
                            Err(msg) => {
                                msg.contains("file fence rejected")
                                    || msg.contains("not found")
                                    || msg.contains("must have .img or .bin")
                            }
                        };
                        let _ = ctx.lease.exit_protected(&dut_id, safe_exit);
                    }
                    let out = outcome?;
                    Ok(serde_json::json!({
                        "ok": out.exit_code == Some(0),
                        "dutId": dut_id,
                        "action": action,
                        "fileId": fid,
                        "exitCode": out.exit_code,
                        "elapsedMs": out.elapsed_ms,
                        "stdout": out.stdout,
                        "stderr": out.stderr,
                        "waitedMs": waited_ms,
                        "leaseReleasedAfterProtected": protected && had_explicit_lease,
                    }))
                }
                "flash_reboot" => {
                    let out = state.flash_reboot(&dut_id, preset_id.as_deref(), identity)?;
                    Ok(serde_json::json!({
                        "ok": out.exit_code == Some(0),
                        "dutId": dut_id,
                        "action": action,
                        "exitCode": out.exit_code,
                        "elapsedMs": out.elapsed_ms,
                        "stdout": out.stdout,
                        "stderr": out.stderr,
                        "waitedMs": waited_ms,
                    }))
                }
                _ => Err("unsupported action".into()),
            }
        })
        .await
        .map_err(|_| state_err("Control worker failed"))?
        .map_err(|err| state_err(&err))?;

        Ok(Json(result))
    }

    #[tool(
        name = "adb_exec",
        description = "通过绑定 DUT 的 ADB 序列号在目标开发板上调试（仅用户配置监听器且具 adb 权限，参数 camelCase）。op 支持 shell | reboot | reboot_loader | logcat | push | pull。示例：① {\"dutId\":\"<id>\",\"op\":\"shell\",\"command\":\"uname -a\"}；② {\"dutId\":\"<id>\",\"op\":\"reboot_loader\"}（直接切入 Rockchip Loader）；③ {\"dutId\":\"<id>\",\"op\":\"pull\",\"remotePath\":\"/proc/cmdline\",\"fileId\":\"cmdline.txt\"}（文件严格限定在 DUT 目录内，返回 resourceUri）。绝不执行宿主机命令。"
    )]
    async fn adb_exec(
        &self,
        Parameters(args): Parameters<AdbExecArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        const MAX_CMD_BYTES: usize = 2048;
        if !matches!(
            args.op.as_str(),
            "shell" | "reboot" | "reboot_loader" | "logcat" | "push" | "pull"
        ) {
            return Err(rmcp::ErrorData::invalid_params(
                "op must be one of: shell, reboot, reboot_loader, logcat, push, pull",
                Some(serde_json::json!({ "code": "INVALID_OP" })),
            ));
        }
        if args.op == "shell" {
            let cmd = args.command.as_deref().map_or_default(str::trim);
            if cmd.is_empty() || cmd.len() > MAX_CMD_BYTES {
                return Err(rmcp::ErrorData::invalid_params(
                    "command (1..=2048 bytes) is required when op == \"shell\"",
                    None,
                ));
            }
        }
        if matches!(args.op.as_str(), "push" | "pull") {
            let fid = args.file_id.as_deref().map_or_default(str::trim);
            let remote = args.remote_path.as_deref().map_or_default(str::trim);
            if fid.is_empty() || remote.is_empty() {
                return Err(rmcp::ErrorData::invalid_params(
                    "fileId and remotePath are required for push/pull",
                    None,
                ));
            }
        }
        let action_label = format!("adb_exec({})", args.op);
        self.authorize(&principal.0, &args.dut_id, DutAction::Adb)
            .map_err(|err| {
                self.attach_blocked_notice(
                    err,
                    &principal.0,
                    &args.dut_id,
                    &action_label,
                    0,
                    args.notify_on_blocked,
                )
            })?;
        let (identity, _lease_guard, waited_ms) = self
            .acquire_or_use_lease(
                &principal.0,
                &args.dut_id,
                &action_label,
                args.lease_id.as_deref(),
                args.session_name.as_deref(),
                args.wait_ms,
                args.notify_on_blocked,
            )
            .await?;
        let timeout_ms = args.timeout_ms.unwrap_or(15_000).clamp(500, 30_000);
        let state = self.state.clone();
        let dut_id = args.dut_id.clone();
        let op = args.op.clone();
        let command = args.command.clone();
        let file_id = args.file_id.clone();
        let remote_path = args.remote_path.clone();

        let out = tokio::task::spawn_blocking(move || {
            state.adb_exec(
                &dut_id,
                &op,
                command.as_deref(),
                file_id.as_deref(),
                remote_path.as_deref(),
                timeout_ms,
                identity,
            )
        })
        .await
        .map_err(|_| state_err("ADB worker failed"))?
        .map_err(|err| state_err(&err))?;

        let mut payload = serde_json::json!({
            "ok": out.exit_code == Some(0),
            "dutId": args.dut_id,
            "op": args.op,
            "exitCode": out.exit_code,
            "elapsedMs": out.elapsed_ms,
            "waitedMs": waited_ms,
            "stdout": out.stdout,
            "stderr": out.stderr,
        });
        if args.op == "pull"
            && out.exit_code == Some(0)
            && let Some(fid) = args
                .file_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            payload["fileId"] = serde_json::json!(fid);
            payload["resourceUri"] =
                serde_json::json!(format!("duthub://dut/{}/files/{fid}", args.dut_id));
        }
        Ok(Json(payload))
    }

    #[tool(
        name = "serial_exec",
        description = "向 DUT 串口发指令、发送经白名单校验的逻辑控制键（keys，如 ctrl+c / esc / enter）、生成日志分段标记（markLabel），或以本地毫秒级触发器拦截启动窗口（triggerReset + waitFor + send + thenSend + expect / expectRegex / settleMs）。返回 matchedBy、bytesObserved、stalled 及超时/停滞时的 screenTail。需 serialWrite 权限（triggerReset 时还需 reset 权限），绝不执行宿主机命令。"
    )]
    async fn serial_exec(
        &self,
        Parameters(args): Parameters<SerialExecArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        const MAX_SERIAL_PAYLOAD_BYTES: usize = 4096;
        const MAX_PATTERN_CHARS: usize = 256;
        const MAX_CAPTURED_BYTES: usize = 64 * 1024;

        let encoded_keys = match args.keys.as_deref() {
            Some(keys) if !keys.is_empty() => {
                Some(crate::keys::encode_logical_keys(keys).map_err(|msg| {
                    rmcp::ErrorData::invalid_params(
                        msg,
                        Some(serde_json::json!({ "code": "INVALID_KEY" })),
                    )
                })?)
            }
            _ => None,
        };
        let keys_sent_count = args.keys.as_ref().map_or(0, Vec::len);

        let needs_write = args.text.is_some()
            || encoded_keys.is_some()
            || args.send.is_some()
            || args.then_send.is_some()
            || args.wait_for.is_some();
        let needs_mark_only = args.mark_label.is_some() && !needs_write && !args.trigger_reset;

        if needs_mark_only {
            self.authorize_read(&principal.0, &args.dut_id)?;
            let label = args.mark_label.as_deref().unwrap_or_default().trim();
            if label.is_empty() {
                return Err(rmcp::ErrorData::invalid_params(
                    "markLabel cannot be empty",
                    None,
                ));
            }
            self.state
                .log_mark(&args.dut_id, label)
                .map_err(|e| state_err(&e))?;
            return Ok(Json(serde_json::json!({
                "ok": true,
                "dutId": args.dut_id,
                "marked": label,
            })));
        }

        if !needs_write && !args.trigger_reset {
            return Err(rmcp::ErrorData::invalid_params(
                "provide at least one of text, keys, waitFor, send, triggerReset, or markLabel",
                None,
            ));
        }

        for (field, payload) in [
            ("text", args.text.as_deref()),
            ("send", args.send.as_deref()),
            ("thenSend", args.then_send.as_deref()),
        ] {
            if let Some(p) = payload
                && p.len() > MAX_SERIAL_PAYLOAD_BYTES
            {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("{field} exceeds {MAX_SERIAL_PAYLOAD_BYTES} bytes"),
                    None,
                ));
            }
        }
        for (field, pat) in [
            ("waitFor", args.wait_for.as_deref()),
            ("expect", args.expect.as_deref()),
        ] {
            if let Some(p) = pat
                && p.chars().count() > MAX_PATTERN_CHARS
            {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("{field} exceeds {MAX_PATTERN_CHARS} chars"),
                    None,
                ));
            }
        }
        let expect_regex = match args.expect_regex.as_deref().filter(|s| !s.is_empty()) {
            Some(pat) => Some(compile_bounded_regex(pat, "expectRegex")?),
            None => None,
        };

        let action_label = if args.trigger_reset {
            "serial_exec(triggerReset)"
        } else {
            "serial_exec"
        };
        self.authorize(&principal.0, &args.dut_id, DutAction::SerialWrite)
            .map_err(|err| {
                self.attach_blocked_notice(
                    err,
                    &principal.0,
                    &args.dut_id,
                    action_label,
                    0,
                    args.notify_on_blocked,
                )
            })?;
        if args.trigger_reset {
            self.authorize(&principal.0, &args.dut_id, DutAction::Reset)
                .map_err(|err| {
                    self.attach_blocked_notice(
                        err,
                        &principal.0,
                        &args.dut_id,
                        action_label,
                        0,
                        args.notify_on_blocked,
                    )
                })?;
        }

        if let Some(req_id) = args.request_id.as_deref() {
            if req_id.is_empty()
                || req_id.len() > 128
                || !req_id.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(rmcp::ErrorData::invalid_params(
                    "requestId must be 1..=128 printable ASCII characters",
                    None,
                ));
            }
            let credential = match &principal.0 {
                Principal::Agent { credential_id, .. } => format!("agent:{credential_id}"),
                Principal::LocalAdmin => "local-admin".into(),
            };
            let key = (credential, args.dut_id.clone(), req_id.to_owned());
            let fingerprint_src = format!(
                "{}|{}|{:?}|{:?}|{:?}|{:?}",
                args.trigger_reset,
                args.text.as_deref().unwrap_or_default(),
                args.keys,
                args.wait_for,
                args.send,
                args.then_send
            );
            let digest: [u8; 32] = Sha256::digest(fingerprint_src.as_bytes()).into();
            let mut receipts = duthub_core::util::lock_unpoisoned(&self.write_receipts);
            receipts.retain(|_, receipt| receipt.accepted_at.elapsed() < WRITE_RECEIPT_TTL);
            if let Some(receipt) = receipts.get(&key) {
                if receipt.text_digest != digest {
                    return Err(rmcp::ErrorData::invalid_params(
                        "requestId was already used with a different serial payload",
                        Some(serde_json::json!({ "code": "REQUEST_ID_CONFLICT" })),
                    ));
                }
                return Ok(Json(serde_json::json!({
                    "ok": true,
                    "dutId": args.dut_id,
                    "status": "queued",
                    "requestId": req_id,
                    "replayed": true,
                })));
            }
            if receipts.len() >= WRITE_RECEIPT_LIMIT {
                return Err(rmcp::ErrorData::new(
                    rmcp::model::ErrorCode(-32000),
                    "Serial receipt cache full; retry later",
                    Some(serde_json::json!({ "code": "RECEIPT_BUSY" })),
                ));
            }
            receipts.insert(
                key,
                WriteReceipt {
                    text_digest: digest,
                    accepted_at: Instant::now(),
                },
            );
        }

        if let Some(label) = args
            .mark_label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.state
                .log_mark(&args.dut_id, label)
                .map_err(|e| state_err(&e))?;
        }

        let (identity, _lease_guard, waited_ms) = self
            .acquire_or_use_lease(
                &principal.0,
                &args.dut_id,
                action_label,
                args.lease_id.as_deref(),
                args.session_name.as_deref(),
                args.wait_ms,
                args.notify_on_blocked,
            )
            .await?;
        let timeout_ms = args.timeout_ms.unwrap_or(5_000).clamp(200, 30_000);
        let settle_window = args
            .settle_ms
            .map(|ms| Duration::from_millis(ms.clamp(200, 5_000)));
        let stall_threshold =
            Duration::from_millis(args.stall_timeout_ms.unwrap_or(3_000).clamp(200, 30_000));
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        // Bookmark the current serial sequence BEFORE resetting or writing so
        // no fast boot bytes are lost between reset and the first poll.
        let mut cursor = self
            .state
            .serial_read(&args.dut_id, 0, usize::MAX)
            .map_or(0, |res| res.last_seq);
        let mut captured = String::new();
        let append_chunks = |state: &Arc<dyn McpState>,
                             dut_id: &str,
                             cursor: &mut u64,
                             captured: &mut String|
         -> usize {
            let mut added_bytes = 0usize;
            if let Some(page) = state.serial_read(dut_id, *cursor, 400) {
                *cursor = page.last_seq;
                for line in page.lines {
                    added_bytes += line.text.len();
                    if captured.len() < MAX_CAPTURED_BYTES {
                        let remaining = MAX_CAPTURED_BYTES - captured.len();
                        if line.text.len() <= remaining {
                            captured.push_str(&line.text);
                        } else {
                            let mut cut = remaining;
                            while cut > 0 && !line.text.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            captured.push_str(&line.text[..cut]);
                        }
                    }
                }
            }
            added_bytes
        };

        let mut reset_triggered = false;
        if args.trigger_reset {
            let state = self.state.clone();
            let dut_id = args.dut_id.clone();
            let id_clone = identity.clone();
            tokio::task::spawn_blocking(move || {
                let tok = state.approval_issue(&dut_id, "control.reset", "{}");
                state.control_reset(&dut_id, &tok.token, id_clone)
            })
            .await
            .map_err(|_| state_err("Reset worker failed"))?
            .map_err(|e| state_err(&e))?;
            reset_triggered = true;
        }

        let mut wait_matched = false;
        let mut payload_dispatched = false;
        if let Some(wait_for) = args.wait_for.as_deref().filter(|s| !s.is_empty()) {
            loop {
                let _ = append_chunks(&self.state, &args.dut_id, &mut cursor, &mut captured);
                if captured.contains(wait_for) {
                    wait_matched = true;
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            if wait_matched {
                if let Some(ref raw_keys) = encoded_keys {
                    self.state
                        .serial_write_raw(&args.dut_id, raw_keys, identity.clone())
                        .map_err(|e| state_err(&e))?;
                    payload_dispatched = true;
                }
                if args.send.is_some() || encoded_keys.is_none() {
                    let payload = args.send.as_deref().unwrap_or("\r");
                    self.state
                        .serial_write(&args.dut_id, payload, identity.clone())
                        .map_err(|e| state_err(&e))?;
                    payload_dispatched = true;
                }
            }
        } else {
            if let Some(payload) = args.text.as_deref().or(args.send.as_deref()) {
                self.state
                    .serial_write(&args.dut_id, payload, identity.clone())
                    .map_err(|e| state_err(&e))?;
                payload_dispatched = true;
            }
            if let Some(ref raw_keys) = encoded_keys {
                self.state
                    .serial_write_raw(&args.dut_id, raw_keys, identity.clone())
                    .map_err(|e| state_err(&e))?;
                payload_dispatched = true;
            }
        }

        if let Some(then_cmd) = args.then_send.as_deref().filter(|s| !s.is_empty())
            && (args.wait_for.is_none() || wait_matched)
        {
            let settle =
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now()));
            if !settle.is_zero() {
                tokio::time::sleep(settle).await;
            }
            self.state
                .serial_write(&args.dut_id, then_cmd, identity.clone())
                .map_err(|e| state_err(&e))?;
            payload_dispatched = true;
        }

        let post_send_at = Instant::now();
        let mut bytes_observed: usize = 0;
        let mut last_rx_at: Option<Instant> = None;
        let mut expect_matched = false;
        let mut matched_by = "immediate";

        let expect_lit = args.expect.as_deref().filter(|s| !s.is_empty());
        let has_wait_condition =
            expect_lit.is_some() || expect_regex.is_some() || settle_window.is_some();

        if has_wait_condition {
            matched_by = "timeout";
            loop {
                let added = append_chunks(&self.state, &args.dut_id, &mut cursor, &mut captured);
                if added > 0 {
                    bytes_observed = bytes_observed.saturating_add(added);
                    last_rx_at = Some(Instant::now());
                }
                let cleaned = crate::keys::strip_ansi_escapes(&captured);
                if let Some(expect) = expect_lit
                    && (captured.contains(expect) || cleaned.contains(expect))
                {
                    expect_matched = true;
                    matched_by = "expect";
                    break;
                }
                if let Some(re) = expect_regex.as_ref()
                    && (re.is_match(&captured) || re.is_match(&cleaned))
                {
                    expect_matched = true;
                    matched_by = "expectRegex";
                    break;
                }
                if let Some(settle) = settle_window
                    && bytes_observed > 0
                    && last_rx_at.is_some_and(|last| last.elapsed() >= settle)
                {
                    expect_matched = true;
                    matched_by = "settled";
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        } else {
            let brief =
                Duration::from_millis(150).min(deadline.saturating_duration_since(Instant::now()));
            if !brief.is_zero() {
                tokio::time::sleep(brief).await;
            }
            bytes_observed = append_chunks(&self.state, &args.dut_id, &mut cursor, &mut captured);
        }

        let stalled = payload_dispatched
            && has_wait_condition
            && bytes_observed == 0
            && post_send_at.elapsed() >= stall_threshold;

        let clean_output = crate::keys::strip_ansi_codes(&captured);
        let mut response = serde_json::json!({
            "ok": true,
            "dutId": args.dut_id,
            "resetTriggered": reset_triggered,
            "waitMatched": wait_matched,
            "expectMatched": expect_matched,
            "matchedBy": matched_by,
            "bytesObserved": bytes_observed,
            "stalled": stalled,
            "waitedMs": waited_ms,
            "output": clean_output,
            "lastSeq": cursor,
        });
        if keys_sent_count > 0 {
            response["keysSent"] = serde_json::json!(keys_sent_count);
        }
        if matched_by == "timeout" || stalled {
            response["screenTail"] = serde_json::json!(
                self.render_dut_screen(&args.dut_id, Some(30))
                    .unwrap_or_default()
            );
        }
        Ok(Json(response))
    }
}

static FEEDBACK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn compile_bounded_regex(pattern: &str, field: &str) -> Result<regex::Regex, rmcp::ErrorData> {
    const MAX_REGEX_CHARS: usize = 256;
    if pattern.chars().count() > MAX_REGEX_CHARS {
        return Err(rmcp::ErrorData::invalid_params(
            format!("{field} exceeds {MAX_REGEX_CHARS} chars"),
            Some(serde_json::json!({ "code": "INVALID_REGEX" })),
        ));
    }
    regex::RegexBuilder::new(pattern)
        .size_limit(256 * 1024)
        .dfa_size_limit(256 * 1024)
        .build()
        .map_err(|err| {
            rmcp::ErrorData::invalid_params(
                format!("invalid {field} regex: {err}"),
                Some(serde_json::json!({ "code": "INVALID_REGEX" })),
            )
        })
}

fn clean_log_lines(lines: &mut [crate::state::LogLineDto]) {
    for line in lines {
        if line.text.as_bytes().contains(&0x1b) {
            line.text = crate::keys::strip_ansi_codes(&line.text);
        }
    }
}

fn tail_text_lines(text: &str, max_lines: usize) -> String {
    if max_lines == 0 || text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// Wire shape of the `notify_user` response. `queued` is derived from the
/// outcome; `reason` only exists for throttled results.
fn notify_response(outcome: NotifyOutcome) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "outcome": outcome.as_str(),
        "queued": matches!(outcome, NotifyOutcome::Queued),
        "source": AGENT_NOTICE_SOURCE,
    });
    if let NotifyOutcome::Throttled(reason) = outcome {
        payload["reason"] = serde_json::json!(reason.as_str());
    }
    payload
}

/// The 10 read-only Plan tools exposed by every authenticated duthub MCP listener.
pub const PLAN_TOOL_NAMES: &[&str] = &[
    "dut_list",
    "dut_status",
    "serial_read",
    "log_segments",
    "log_segment_read",
    "dut_files",
    "dut_file_read",
    "media_streams",
    "adb_list_devices",
    "skill_export",
];

pub(crate) fn plan_tool_allowed(name: &str) -> bool {
    PLAN_TOOL_NAMES.contains(&name)
}

pub(crate) fn plan_mode_error() -> rmcp::ErrorData {
    rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32000),
        "PLAN_MODE_ONLY: 当前只允许读取诊断证据和生成计划。",
        Some(serde_json::json!({ "kind": "execution_policy", "code": "PLAN_MODE_ONLY" })),
    )
}

/// Tools exposed on the A2A evidence surface ([`McpServerCore::for_evidence`]).
///
/// These are the read-only diagnostic tools: DUT discovery/status, serial
/// evidence and archived log segments. File browsing, media stream URLs, ADB
/// discovery, skill export and every interactive/control tool stay off this surface.
pub const EVIDENCE_TOOL_NAMES: [&str; 5] = [
    "dut_list",
    "dut_status",
    "serial_read",
    "log_segments",
    "log_segment_read",
];

fn evidence_scope_error(tool: &str) -> rmcp::ErrorData {
    rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32000),
        format!("EVIDENCE_SCOPE_ONLY: 证据端不开放工具 {tool}"),
        Some(serde_json::json!({ "kind": "execution_policy", "code": "EVIDENCE_SCOPE_ONLY" })),
    )
}

/// Clamp an attacker-controlled tool/DUT identifier before it enters an audit
/// line. Control characters are stripped so a crafted `dutId` cannot forge
/// extra audit fields or break JSONL framing.
#[must_use]
pub fn truncate_audit_field(value: &str) -> String {
    const MAX_CHARS: usize = 64;
    let mut out = String::new();
    let mut visible = 0usize;
    for ch in value.chars().filter(|ch| !ch.is_control()) {
        visible += 1;
        if visible <= MAX_CHARS {
            out.push(ch);
        }
    }
    if visible > MAX_CHARS {
        out.push_str("...");
    }
    out
}

/// Read the principal injected into the tool request by the server.
fn principal_in_context(
    context: &rmcp::service::RequestContext<rmcp::RoleServer>,
) -> Option<Principal> {
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<Principal>().cloned())
        .or_else(|| context.extensions.get::<Principal>().cloned())
}

/// Identify policy denials for the tool audit result.
fn tool_call_denied(error: &rmcp::ErrorData) -> bool {
    let kind = error
        .data
        .as_ref()
        .and_then(|data| data.get("kind"))
        .and_then(serde_json::Value::as_str);
    matches!(
        kind,
        Some("authorization" | "lease" | "approval" | "execution_policy")
    ) || error.message.starts_with("LeaseRequired/")
        || error.message.starts_with("ApprovalRequired/")
        || error.message.contains("PLAN_MODE_ONLY")
        || error.message.contains("UNAUTHORIZED")
}

/// Summarize results using counts or lengths without including content.
fn summarize_tool_result(result: &rmcp::model::CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(serde_json::Value::Object(map)) = &result.structured_content {
        for (key, value) in map.iter().take(8) {
            match value {
                serde_json::Value::Array(items) => parts.push(format!("{key}={}", items.len())),
                serde_json::Value::String(text) => {
                    parts.push(format!("{key}.len={}", text.chars().count()));
                }
                _ => {}
            }
        }
    }
    let content_bytes: usize = result
        .content
        .iter()
        .map(|block| match block {
            rmcp::model::ContentBlock::Text(text) => text.text.len(),
            _ => 0,
        })
        .sum();
    parts.push(format!("content_bytes={content_bytes}"));
    parts.join(" ")
}

fn audit_tool_result(result: &Result<CallToolResponse, rmcp::ErrorData>) -> (AuditOutcome, String) {
    match result {
        Ok(CallToolResponse::Complete(result)) => {
            if result.is_error == Some(true) {
                (
                    AuditOutcome::Error,
                    format!("is_error=true {}", summarize_tool_result(result)),
                )
            } else {
                (AuditOutcome::Ok, summarize_tool_result(result))
            }
        }
        Ok(_) => (AuditOutcome::Ok, "result=intermediate".into()),
        Err(error) => {
            let mut message: String = error.message.chars().take(160).collect();
            message = message.replace(['\r', '\n'], " ");
            (
                if tool_call_denied(error) {
                    AuditOutcome::Denied
                } else {
                    AuditOutcome::Error
                },
                format!("error={message}"),
            )
        }
    }
}

/// Read-only Plan surface: the 10 streamlined tools every authenticated listener exposes.
static PLAN_ROUTER: LazyLock<ToolRouter<McpServerCore>> = LazyLock::new(|| {
    let mut router = McpServerCore::tool_router();
    for tool in router.list_all() {
        if !plan_tool_allowed(&tool.name) {
            router.remove_route(&tool.name);
        }
    }
    router
});

/// User-configured listener surface: the 10 Plan tools plus `notify_user`,
/// `dut_control` and `serial_exec` (13 total). Desktop-issued Agent sessions,
/// stdio snapshots and A2A endpoints never attach a notifier and therefore only
/// see the 10 read-only Plan tools (or 5 evidence tools for A2A).
static PLAN_ROUTER_WITH_NOTIFY: LazyLock<ToolRouter<McpServerCore>> = LazyLock::new(|| {
    let mut router = McpServerCore::tool_router();
    for tool in router.list_all() {
        if !plan_tool_allowed(&tool.name) && !USER_EXTRA_TOOL_NAMES.contains(&tool.name.as_ref()) {
            router.remove_route(&tool.name);
        }
    }
    router
});

#[tool_handler(router = PLAN_ROUTER)]
impl ServerHandler for McpServerCore {
    /// Static route table for this instance: `PLAN_ROUTER_WITH_NOTIFY` when a
    /// notifier is attached, `PLAN_ROUTER` otherwise.
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "required by rmcp ServerHandler trait signature"
    )]
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        let tools = if self.evidence_scope {
            self.routes()
                .list_all()
                .into_iter()
                .filter(|tool| EVIDENCE_TOOL_NAMES.contains(&tool.name.as_ref()))
                .collect()
        } else {
            self.routes().list_all()
        };
        let mut result = rmcp::model::ListToolsResult::with_all_items(tools);
        result.ttl_ms = supports_cache_hints.then_some(0);
        result.cache_scope = supports_cache_hints.then_some(rmcp::model::CacheScope::Public);
        Ok(result)
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        if self.evidence_scope && !EVIDENCE_TOOL_NAMES.contains(&name) {
            return None;
        }
        self.routes().get(name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let started = Instant::now();
        let tool_name = request.name.to_string();
        let dut_id = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("dutId"))
            .and_then(serde_json::Value::as_str)
            .map(truncate_audit_field);
        let principal = principal_in_context(&context);
        let audit = self.access.clone();
        let router = self.routes();
        let result = if self.evidence_scope && !EVIDENCE_TOOL_NAMES.contains(&tool_name.as_str()) {
            Err(evidence_scope_error(&tool_name))
        } else if router.has_route(&tool_name) {
            router
                .call(ToolCallContext::new(self, request, context))
                .await
        } else {
            Err(plan_mode_error())
        };
        if let Some(access) = audit {
            let (outcome, summary) = audit_tool_result(&result);
            let identity = principal.as_ref().and_then(Principal::audit_identity);
            let (agent, credential_id) = match identity {
                Some((agent, credential_id)) => (Some(agent), Some(credential_id)),
                None => (None, None),
            };
            let tool_for_audit = truncate_audit_field(&tool_name);
            let detail = match dut_id.as_deref() {
                Some(dut) => format!(
                    "tool={tool_for_audit} dut={dut} duration_ms={} {summary}",
                    started.elapsed().as_millis()
                ),
                None => format!(
                    "tool={tool_for_audit} duration_ms={} {summary}",
                    started.elapsed().as_millis()
                ),
            };
            access.audit.record(
                "tool.call",
                agent,
                credential_id,
                dut_id.as_deref(),
                outcome,
                Some(&detail),
            );
        }
        result
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "required by rmcp ServerHandler trait signature"
    )]
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, rmcp::ErrorData> {
        let principal = self.resolve_principal(&context)?;
        self.authorize_any_read(&principal)?;
        let mut resources = Vec::new();
        for dut in self.authorized_duts(&principal) {
            resources.push(
                rmcp::model::Resource::new(
                    format!("duthub://dut/{}/serial/tail", dut.dut_id),
                    format!("{} serial tail", dut.name),
                )
                .with_description("Newest 150 lines of live serial output (read-only)")
                .with_mime_type("text/plain"),
            );
            resources.push(
                rmcp::model::Resource::new(
                    format!("duthub://dut/{}/serial/screen", dut.dut_id),
                    format!("{} serial screen", dut.name),
                )
                .with_description("Rendered VT100 terminal screen text (ANSI-free, read-only)")
                .with_mime_type("text/plain"),
            );
            if let Some(root) = self.state.dut_artifact_root()
                && let Ok(files) =
                    duthub_core::dut_artifacts::list_files(&root, &dut.dut_id, &dut.name)
            {
                for file in files.into_iter().take(20) {
                    let mime = mime_for_filename(&file.id);
                    resources.push(
                        rmcp::model::Resource::new(
                            format!("duthub://dut/{}/files/{}", dut.dut_id, file.id),
                            format!("{}/{}", dut.name, file.id),
                        )
                        .with_description("Archived DUT artifact inside duthub work directory")
                        .with_mime_type(mime)
                        .with_size(file.bytes),
                    );
                }
            }
        }
        Ok(rmcp::model::ListResourcesResult::with_all_items(resources))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "required by rmcp ServerHandler trait signature"
    )]
    async fn list_resource_templates(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourceTemplatesResult, rmcp::ErrorData> {
        let templates = vec![
            rmcp::model::ResourceTemplate::new(
                "duthub://dut/{dutId}/serial/tail",
                "DUT Live Serial Tail",
            )
            .with_description("Read the newest 150 chunks of live serial output for a DUT.")
            .with_mime_type("text/plain"),
            rmcp::model::ResourceTemplate::new(
                "duthub://dut/{dutId}/serial/screen",
                "DUT Rendered Terminal Screen",
            )
            .with_description("Read the rendered VT100 terminal screen as clean ANSI-free text.")
            .with_mime_type("text/plain"),
            rmcp::model::ResourceTemplate::new(
                "duthub://dut/{dutId}/logs/{segmentId}",
                "DUT Serial Log Segment",
            )
            .with_description("Read an archived serial log segment (up to 256 KiB; use log_segment_read for larger segments).")
            .with_mime_type("text/plain"),
            rmcp::model::ResourceTemplate::new(
                "duthub://dut/{dutId}/files/{fileId}",
                "DUT Artifact File (Screenshot / Log / Capture)",
            )
            .with_description("Read a screenshot PNG or artifact strictly inside the DUT folder (up to 4 MiB; use dut_file_read for larger files).")
            .with_mime_type("application/octet-stream"),
        ];
        Ok(rmcp::model::ListResourceTemplatesResult::with_all_items(
            templates,
        ))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "required by rmcp ServerHandler trait signature"
    )]
    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, rmcp::ErrorData> {
        const MAX_TEXT_RESOURCE_BYTES: usize = 256 * 1024;
        const MAX_BINARY_RESOURCE_BYTES: usize = 4 * 1024 * 1024;

        let principal = self.resolve_principal(&context)?;
        let uri = request.uri;
        let Some(rest) = uri.strip_prefix("duthub://dut/") else {
            return Err(rmcp::ErrorData::invalid_params(
                "URI must start with duthub://dut/{dutId}/...",
                None,
            ));
        };
        let Some((dut_id, subpath)) = rest.split_once('/') else {
            return Err(rmcp::ErrorData::invalid_params(
                "Invalid duthub resource URI; expected duthub://dut/{dutId}/(serial/tail|serial/screen|logs/{segmentId}|files/{fileId})",
                None,
            ));
        };
        if dut_id.is_empty() || dut_id.contains("..") || dut_id.contains('\\') {
            return Err(rmcp::ErrorData::invalid_params(
                "Invalid dutId in resource URI",
                None,
            ));
        }
        if subpath == "serial/screen" {
            self.authorize_read(&principal, dut_id)?;
            let Some(mut text) = self.render_dut_screen(dut_id, None) else {
                return Err(rmcp::ErrorData::invalid_params(
                    "DUT session is not open; read archived logs via duthub://dut/{dutId}/logs/{segmentId}",
                    None,
                ));
            };
            if text.len() > MAX_TEXT_RESOURCE_BYTES {
                let mut cut = MAX_TEXT_RESOURCE_BYTES;
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
            }
            return Ok(rmcp::model::ReadResourceResponse::Complete(
                rmcp::model::ReadResourceResult::new(vec![rmcp::model::ResourceContents::text(
                    text, uri,
                )]),
            ));
        }
        if subpath == "serial/tail" {
            self.authorize_read(&principal, dut_id)?;
            let Some(result) = self.state.serial_read(dut_id, 0, usize::MAX) else {
                return Err(rmcp::ErrorData::invalid_params(
                    "DUT session is not open; read archived logs via duthub://dut/{dutId}/logs/{segmentId}",
                    None,
                ));
            };
            let start = result.lines.len().saturating_sub(150);
            let mut text = String::new();
            for line in &result.lines[start..] {
                let clean = crate::keys::strip_ansi_codes(&line.text);
                if text.len() + clean.len() > MAX_TEXT_RESOURCE_BYTES {
                    break;
                }
                text.push_str(&clean);
            }
            return Ok(rmcp::model::ReadResourceResponse::Complete(
                rmcp::model::ReadResourceResult::new(vec![rmcp::model::ResourceContents::text(
                    text, uri,
                )]),
            ));
        }
        if let Some(segment_id) = subpath.strip_prefix("logs/") {
            self.authorize_scoped_read(&principal, dut_id)?;
            let dir = self
                .state
                .log_archive_dir()
                .ok_or_else(|| state_err("Log archive unavailable"))?;
            let mut out = String::new();
            let mut offset = 0u64;
            loop {
                let page = duthub_core::log_archive::read(&dir, dut_id, segment_id, offset)
                    .map_err(|err| state_err(&err.to_string()))?;
                let clean_page = crate::keys::strip_ansi_codes(&page.text);
                if !clean_page.is_empty() {
                    let remaining = MAX_TEXT_RESOURCE_BYTES.saturating_sub(out.len());
                    if clean_page.len() <= remaining {
                        out.push_str(&clean_page);
                    } else {
                        let mut cut = remaining;
                        while cut > 0 && !clean_page.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        out.push_str(&clean_page[..cut]);
                        break;
                    }
                }
                if page.eof || page.next_offset <= offset || out.len() >= MAX_TEXT_RESOURCE_BYTES {
                    break;
                }
                offset = page.next_offset;
            }
            return Ok(rmcp::model::ReadResourceResponse::Complete(
                rmcp::model::ReadResourceResult::new(vec![rmcp::model::ResourceContents::text(
                    out, uri,
                )]),
            ));
        }
        if let Some(file_id) = subpath.strip_prefix("files/") {
            self.authorize_scoped_read(&principal, dut_id)?;
            let root = self
                .state
                .dut_artifact_root()
                .ok_or_else(|| state_err("DUT artifact directory unavailable"))?;
            let dut = self
                .state
                .dut_info(dut_id)
                .ok_or_else(|| state_err("DUT not found"))?;
            let mut all = Vec::new();
            let mut offset = 0u64;
            loop {
                let (chunk, next, eof) = duthub_core::dut_artifacts::read_file(
                    &root,
                    &dut.dut_id,
                    &dut.name,
                    file_id,
                    offset,
                )
                .map_err(|err| state_err(&err.to_string()))?;
                if all.len() + chunk.len() > MAX_BINARY_RESOURCE_BYTES {
                    return Err(rmcp::ErrorData::invalid_params(
                        "File exceeds 4 MiB resource limit; use dut_file_read with offset pagination",
                        Some(serde_json::json!({ "code": "RESOURCE_TOO_LARGE" })),
                    ));
                }
                all.extend_from_slice(&chunk);
                if eof || next <= offset {
                    break;
                }
                offset = next;
            }
            let mime = mime_for_filename(file_id);
            let resource = if mime.starts_with("text/") || mime == "application/json" {
                let text = String::from_utf8_lossy(&all).into_owned();
                rmcp::model::ResourceContents::text(text, uri).with_mime_type(mime)
            } else {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&all);
                rmcp::model::ResourceContents::blob(b64, uri).with_mime_type(mime)
            };
            return Ok(rmcp::model::ReadResourceResponse::Complete(
                rmcp::model::ReadResourceResult::new(vec![resource]),
            ));
        }
        Err(rmcp::ErrorData::invalid_params(
            "Unknown duthub resource path; use serial/tail, logs/{segmentId}, or files/{fileId}",
            None,
        ))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "required by rmcp ServerHandler trait signature"
    )]
    async fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListPromptsResult, rmcp::ErrorData> {
        let dut_arg = rmcp::model::PromptArgument::new("dutId")
            .with_description("Target DUT ID from dut_list (optional; omit to discover first)")
            .with_required(false);
        let prompts = vec![
            rmcp::model::Prompt::new(
                "enter_flash_mode",
                Some(
                    "Guide the Agent to inspect relay bindings and put a DUT into Loader/MaskROM mode in one shot.",
                ),
                Some(vec![
                    dut_arg.clone(),
                    rmcp::model::PromptArgument::new("mode")
                        .with_description("Target flash mode: loader | maskrom (default: loader)")
                        .with_required(false),
                ]),
            ),
            rmcp::model::Prompt::new(
                "capture_and_diagnose",
                Some(
                    "Guide the Agent to collect serial tail, segmented logs, screenshot PNG resource, and optional screen recording.",
                ),
                Some(vec![dut_arg.clone()]),
            ),
            rmcp::model::Prompt::new(
                "read_dut_evidence",
                Some(
                    "Read-only diagnostic checklist: inspect DUT status, serial logs, files, and report findings.",
                ),
                Some(vec![dut_arg]),
            ),
        ];
        Ok(rmcp::model::ListPromptsResult::with_all_items(prompts))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "required by rmcp ServerHandler trait signature"
    )]
    async fn get_prompt(
        &self,
        request: rmcp::model::GetPromptRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::GetPromptResponse, rmcp::ErrorData> {
        let dut = request
            .arguments
            .as_ref()
            .and_then(|m| m.get("dutId"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<dutId>");
        let mode = request
            .arguments
            .as_ref()
            .and_then(|m| m.get("mode"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("loader");
        let (description, text) = match request.name.as_str() {
            "enter_flash_mode" => (
                "One-shot workflow for putting a DUT into flash mode (Loader or MaskROM)",
                format!(
                    "Follow this exact sequence (all JSON keys are camelCase):\n\
1. Call `dut_list` if `dutId` is unknown, then call `dut_status` with `{{\"dutId\":\"{dut}\"}}`.\n\
2. Check `relay.channels`, `adbSerial`, and `bootModes`:\n\
   - If target is `maskrom` and `bootModes.canEnterMaskrom == true`: call `dut_control` with `{{\"dutId\":\"{dut}\",\"action\":\"enter_maskrom\"}}`.\n\
   - If target is `loader` and `bootModes.canEnterLoaderByHardware == true`: call `dut_control` with `{{\"dutId\":\"{dut}\",\"action\":\"enter_loader\"}}`.\n\
   - If Android/ADB is online (`adbSerial` present): call `adb_exec` with `{{\"dutId\":\"{dut}\",\"op\":\"reboot_loader\"}}`.\n\
   - If only `reset` is wired (common on RK3576/embedded boards): call `serial_exec` ONCE with `{{\"dutId\":\"{dut}\",\"triggerReset\":true,\"waitFor\":\"Hit any key\",\"send\":\"\\r\",\"thenSend\":\"rockusb 0 mmc 0\\r\",\"expect\":\"rockusb\",\"markLabel\":\"enter-{mode}\",\"timeoutMs\":15000}}`, or if already in Linux shell call `serial_exec` with `{{\"dutId\":\"{dut}\",\"text\":\"reboot loader\\r\",\"markLabel\":\"enter-{mode}\"}}`.\n\
3. Confirm Loader/MaskROM detection with `dut_control` `{{\"dutId\":\"{dut}\",\"action\":\"flash_list\"}}` (`inLoader == true` or `inMaskrom == true`).\n\
4. Flash an image staged inside the DUT folder via `dut_control` `{{\"dutId\":\"{dut}\",\"action\":\"flash_update\",\"fileId\":\"<image.img>\"}}` (check `dut_files` first; never pass host paths).\n\
5. Verify boot logs via `serial_read` (`{{\"dutId\":\"{dut}\",\"tail\":80}}`) or resource `duthub://dut/{dut}/serial/tail`.\n\
Never attempt to access files outside duthub's directory or execute Windows host commands."
                ),
            ),
            "capture_and_diagnose" => (
                "Collect serial evidence, screenshot PNG resource, and optional recording",
                format!(
                    "Follow this exact sequence (camelCase arguments):\n\
1. `dut_status` `{{\"dutId\":\"{dut}\"}}` to inspect serial, relay, heartbeat, and capabilities.\n\
2. Read live serial via `duthub://dut/{dut}/serial/tail` (or `serial_read` `{{\"dutId\":\"{dut}\",\"tail\":150}}`).\n\
3. Mark a log boundary if needed: `serial_exec` `{{\"dutId\":\"{dut}\",\"markLabel\":\"diag-checkpoint\"}}`, then read segments via `log_segments` `{{\"dutId\":\"{dut}\"}}`.\n\
4. Capture a screenshot: `media_streams` `{{\"dutId\":\"{dut}\",\"op\":{{\"kind\":\"snapshot\"}}}}`, then read the returned `artifact.resourceUri` directly via `resources/read` to inspect the PNG.\n\
5. For screen recording: `media_streams` `{{\"dutId\":\"{dut}\",\"op\":{{\"kind\":\"recordStart\"}}}}`, perform actions, then pair with `{{\"dutId\":\"{dut}\",\"op\":{{\"kind\":\"recordStop\",\"recordingId\":<id>}}}}`.\n\
6. Notify the user on desktop if attention is needed: `notify_user` `{{\"dutId\":\"{dut}\",\"title\":\"...\",\"message\":\"...\",\"level\":\"warning\"}}`."
                ),
            ),
            "read_dut_evidence" => (
                "Read-only DUT evidence collection",
                format!(
                    "Collect evidence strictly via duthub MCP (all keys camelCase):\n\
1. `dut_list` → `dut_status` `{{\"dutId\":\"{dut}\"}}`\n\
2. Live logs: resource `duthub://dut/{dut}/serial/tail` or `serial_read` `{{\"dutId\":\"{dut}\",\"tail\":150}}`\n\
3. Archived segments: `log_segments` `{{\"dutId\":\"{dut}\"}}` → read `resourceUri` or `log_segment_read`\n\
4. Files/screenshots: `dut_files` `{{\"dutId\":\"{dut}\"}}` → read `resourceUri` (<=4 MiB) or `dut_file_read`."
                ),
            ),
            other => {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("Unknown prompt: {other}"),
                    None,
                ));
            }
        };
        Ok(rmcp::model::GetPromptResponse::Complete(
            rmcp::model::GetPromptResult::new(vec![rmcp::model::PromptMessage::new_text(
                rmcp::model::Role::User,
                text,
            )])
            .with_description(description),
        ))
    }

    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("duthub", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "duthub MCP for remote Linux & local Agents (all JSON arguments use camelCase: dutId, sinceSeq, markLabel, triggerReset, fileId, remotePath). Do NOT assume local filesystem access to the Windows host — read all logs, screenshots, recordings, and DUT files strictly via MCP Tools or duthub:// Resources.\n\
QUICK RECIPES (one-shot, zero guessing):\n\
1. Discover & inspect: `dut_list` -> `dut_status` {\"dutId\":\"<id>\"} (check `relay.channels`, `adbSerial`, `bootModes.recommendedLoaderFlow`).\n\
2. Live/archived serial & screenshots (Tools OR Resources):\n\
   - Live tail: resource `duthub://dut/{dutId}/serial/tail` OR `serial_read` {\"dutId\":\"<id>\",\"tail\":100}.\n\
   - Segmented logs: `serial_exec` {\"dutId\":\"<id>\",\"markLabel\":\"step-1\"} -> `log_segments` {\"dutId\":\"<id>\"} -> read `resourceUri` or `log_segment_read`.\n\
   - Screenshot/Recording: `media_streams` {\"dutId\":\"<id>\",\"op\":{\"kind\":\"snapshot\"}} (read returned PNG via `artifact.resourceUri`) or `recordStart`/`recordStop`.\n\
3. Control board, ADB debug & flash (user-configured listeners with matching DUT grants):\n\
   - Hardware control: `dut_control` {\"dutId\":\"<id>\",\"action\":\"reset\"|\"power_cycle\"|\"enter_maskrom\"|\"enter_loader\"|\"flash_list\"|\"flash_update\"|\"flash_reboot\"}.\n\
   - Multi-Agent arbitration: concurrent calls auto-queue in FIFO order (`waitMs`, default 5000ms). For multi-step atomic flows (enter Loader -> flash -> boot), acquire a session lease via `dut_control` {\"dutId\":\"<id>\",\"action\":\"acquire_lease\",\"waitMs\":15000}, pass `leaseId` to each step, and finish with `release_lease`. If access cannot be acquired (manual takeover, GUI holding control, another Agent busy, protected flash, or missing grant), duthub automatically raises a desktop human-intervention alert explaining why and returns `humanNotified`/`reason`/`holder` in error data (set `notifyOnBlocked: false` to probe silently).\n\
   - Only `reset` wired: `serial_exec` {\"dutId\":\"<id>\",\"triggerReset\":true,\"waitFor\":\"Hit any key\",\"send\":\"\\r\",\"thenSend\":\"rockusb 0 mmc 0\\r\",\"timeoutMs\":15000} or Linux `{\"dutId\":\"<id>\",\"text\":\"reboot loader\\r\"}`.\n\
   - ADB debugging: `adb_exec` {\"dutId\":\"<id>\",\"op\":\"shell\"|\"reboot\"|\"reboot_loader\"|\"logcat\"|\"push\"|\"pull\"} (`push`/`pull` strictly fenced inside the DUT folder).\n\
   - Flashing: check `dut_files` -> verify Loader via `dut_control` (`flash_list`) -> flash via `dut_control` (`flash_update`, `fileId` plain filename only; enters `Protected` during eMMC write).\n\
4. Alert desktop user: `notify_user` {\"message\":\"...\",\"title\":\"...\",\"level\":\"info|warning|error\",\"dutId\":\"<id>\"}（仅用户配置的 MCP 监听器提供，桌面发起的 Agent 会话没有该工具）.\n\
SECURITY: File access is strictly confined inside duthub's work directory; host OS commands and out-of-root paths are forbidden.",
        )
    }
}

fn mime_for_filename(name: &str) -> &'static str {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ts") => "video/mp2t",
        Some("mp4") => "video/mp4",
        Some("json") => "application/json",
        Some("log" | "txt" | "md") => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use crate::snapshot::SnapshotState;
    use duthub_core::agent_access::{
        AgentAccessRuntime, AgentAccessSettings, AgentCredential, DutGrant,
    };
    use duthub_core::audit::AuditSink;
    use duthub_core::lease::ControlLeaseManager;

    #[test]
    fn list_and_approval_are_limited_to_granted_duts_and_actions() {
        let snapshot =
            std::env::temp_dir().join(format!("duthub-mcp-scope-{}.json", std::process::id()));
        std::fs::write(&snapshot, r#"{"version":1,"tsUnixMs":0,"duts":[
             {"dutId":"dut-a","name":"Allowed","source":"idle","serial":{"host":"local","port":"COM30","baud":115200,"format":"8N1"},"adbSerial":"visible-device","lines":[]},
             {"dutId":"dut-b","name":"Other","source":"idle","serial":{"host":"local","port":"COM31","baud":115200,"format":"8N1"},"adbSerial":"other-device","lines":[]}
        ]}"#).unwrap();
        let state = Arc::new(SnapshotState::load(&snapshot));
        std::fs::remove_file(&snapshot).unwrap();
        let audit = AuditSink::memory();
        let access = AccessContext::new(
            AgentAccessRuntime::from_settings(AgentAccessSettings {
                enabled: true,
                agents: vec![AgentCredential {
                    listener_id: None,
                    id: "fixture".into(),
                    agent_name: "fixture".into(),
                    token: "test-only".into(),
                    enabled: true,
                    grants: vec![DutGrant {
                        dut_id: "dut-a".into(),
                        actions: vec![DutAction::Read, DutAction::SerialWrite, DutAction::Adb],
                    }],
                }],
                ..AgentAccessSettings::default()
            }),
            Arc::new(ControlLeaseManager::new(None)),
            audit,
        );
        let server = McpServerCore::new(state, Some(Arc::new(access)));
        let principal = Principal::Agent {
            agent_name: "fixture".into(),
            credential_id: "fixture".into(),
        };
        let Json(list) = server
            .dut_list(CurrentPrincipal(principal.clone()))
            .unwrap();
        let authorized = list["duts"].as_array().unwrap();
        assert_eq!(authorized.len(), 1);
        assert_eq!(authorized[0]["dut_id"], "dut-a");
        assert_eq!(
            server.readable_adb_serials(&principal),
            Some(["visible-device".to_owned()].into_iter().collect())
        );
        let Json(all) = server
            .dut_list(CurrentPrincipal(Principal::LocalAdmin))
            .unwrap();
        assert_eq!(all["duts"].as_array().unwrap().len(), 2);

        let request = |dut: &str, action: &str| {
            server.approval_request(
                Parameters(ApprovalRequestArgs {
                    dut_id: dut.into(),
                    action: action.into(),
                    args: serde_json::json!({}),
                }),
                CurrentPrincipal(principal.clone()),
            )
        };
        assert!(request("dut-a", "serial.write").is_ok());
        assert!(request("dut-a", "control.power").is_err());
        assert!(request("dut-b", "serial.write").is_err());
        let unavailable = request("dut-a", "adb.shell").err().unwrap();
        assert_eq!(unavailable.data.unwrap()["code"], "TOOL_UNAVAILABLE");
    }

    #[tokio::test]
    async fn dut_bound_listener_reads_only_its_dut() {
        let snapshot = std::env::temp_dir().join(format!(
            "duthub-mcp-bound-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&snapshot, r#"{"version":1,"tsUnixMs":0,"duts":[
             {"dutId":"dut-a","name":"Bound","source":"idle","serial":{"host":"local","port":"COM30","baud":115200,"format":"8N1"},"adbSerial":"bound-device","lines":[]},
             {"dutId":"dut-b","name":"Other","source":"idle","serial":{"host":"local","port":"COM31","baud":115200,"format":"8N1"},"adbSerial":"other-device","lines":[]}
        ]}"#).unwrap();
        let state = Arc::new(SnapshotState::load(&snapshot));
        std::fs::remove_file(&snapshot).unwrap();
        let access = AccessContext::new(
            AgentAccessRuntime::from_settings(AgentAccessSettings {
                enabled: true,
                agents: vec![AgentCredential {
                    listener_id: Some("dut-a-mcp".into()),
                    id: "fixture".into(),
                    agent_name: "fixture".into(),
                    token: "test-only".into(),
                    enabled: true,
                    grants: vec![DutGrant {
                        dut_id: "dut-a".into(),
                        actions: vec![DutAction::Read],
                    }],
                }],
                ..AgentAccessSettings::default()
            }),
            Arc::new(ControlLeaseManager::new(None)),
            AuditSink::memory(),
        );
        let server = McpServerCore::new(state, Some(Arc::new(access)))
            .with_listener_dut_arc(Some(Arc::from("dut-a")));
        let principal = Principal::Agent {
            agent_name: "fixture".into(),
            credential_id: "fixture".into(),
        };

        let Json(list) = server
            .dut_list(CurrentPrincipal(principal.clone()))
            .unwrap();
        let duts = list["duts"].as_array().unwrap();
        assert_eq!(duts.len(), 1);
        assert_eq!(duts[0]["dut_id"], "dut-a");

        let other_info = server
            .dut_status(
                Parameters(DutInfoArgs {
                    dut_id: "dut-b".into(),
                }),
                CurrentPrincipal(principal.clone()),
            )
            .await
            .err()
            .expect("cross-DUT status must be rejected");
        assert!(
            other_info.message.contains("UNAUTHORIZED"),
            "{other_info:?}"
        );

        let other_logs = server
            .log_segments(
                Parameters(LogSegmentsArgs {
                    dut_id: "dut-b".into(),
                    offset: 0,
                    limit: 10,
                }),
                CurrentPrincipal(principal.clone()),
            )
            .err()
            .expect("cross-DUT logs must be rejected");
        assert!(
            other_logs.message.contains("UNAUTHORIZED"),
            "{other_logs:?}"
        );

        let other_files = server
            .dut_files(
                Parameters(DutFilesArgs {
                    dut_id: Some("dut-b".into()),
                    category: None,
                    offset: 0,
                    limit: 10,
                }),
                CurrentPrincipal(principal.clone()),
            )
            .err()
            .expect("cross-DUT files must be rejected");
        assert!(
            other_files.message.contains("UNAUTHORIZED"),
            "{other_files:?}"
        );

        assert_eq!(server.scoped_dut(None), Some("dut-a"));
        assert_eq!(server.scoped_dut(Some("dut-b")), Some("dut-b"));
    }

    fn overlay_access() -> Arc<AccessContext> {
        let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
        auth.set_runtime_listener(
            duthub_core::agent_access::AgentListener {
                id: "session-a".into(),
                port: 0,
                dut_id: Some("dut-a".into()),
            },
            AgentCredential {
                listener_id: Some("session-a".into()),
                id: "cred-session".into(),
                agent_name: "session-agent".into(),
                token: duthub_core::agent_access::generate_token(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-a".into(),
                    actions: vec![DutAction::Read],
                }],
            },
        )
        .unwrap();
        Arc::new(AccessContext::new(
            auth,
            Arc::new(ControlLeaseManager::new(None)),
            AuditSink::memory(),
        ))
    }

    /// A read grant in the runtime overlay satisfies the read check.
    #[test]
    fn any_read_uses_merged_overlay_grants() {
        let server = McpServerCore::new(Arc::new(SnapshotState::empty()), Some(overlay_access()));
        let session = Principal::Agent {
            agent_name: "session-agent".into(),
            credential_id: "cred-session".into(),
        };
        server
            .authorize_any_read(&session)
            .expect("overlay read grant must satisfy any-read");
        let unknown = Principal::Agent {
            agent_name: "unknown".into(),
            credential_id: "cred-unknown".into(),
        };
        let error = server
            .authorize_any_read(&unknown)
            .expect_err("unknown credential must be rejected");
        assert!(error.message.contains("UNAUTHORIZED"), "{error:?}");
        server
            .authorize_any_read(&Principal::LocalAdmin)
            .expect("LocalAdmin must be unrestricted");
    }

    /// Stdio local access and an authenticated read grant allow skill export. A zero-grant credential is refused.
    #[test]
    fn skill_export_requires_any_read_grant_but_allows_local_admin() {
        let state = Arc::new(SnapshotState::empty());
        let stdio = McpServerCore::new(state.clone(), None);
        let Json(data) = stdio
            .skill_export(CurrentPrincipal(Principal::LocalAdmin))
            .expect("stdio LocalAdmin can read SKILL.md");
        assert_eq!(data["ok"], serde_json::json!(true));

        let zero_grant_auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
            enabled: true,
            agents: vec![AgentCredential {
                listener_id: None,
                id: "cred-none".into(),
                agent_name: "no-grant".into(),
                token: duthub_core::agent_access::generate_token(),
                enabled: true,
                grants: Vec::new(),
            }],
            ..Default::default()
        });
        let zero_grant = McpServerCore::new(
            state.clone(),
            Some(Arc::new(AccessContext::new(
                zero_grant_auth,
                Arc::new(ControlLeaseManager::new(None)),
                AuditSink::memory(),
            ))),
        );
        let Err(denied) = zero_grant.skill_export(CurrentPrincipal(Principal::Agent {
            agent_name: "no-grant".into(),
            credential_id: "cred-none".into(),
        })) else {
            panic!("Reject skill_export when no grants are present");
        };
        assert!(denied.message.contains("UNAUTHORIZED"), "{denied:?}");

        let granted = McpServerCore::new(state, Some(overlay_access()));
        granted
            .skill_export(CurrentPrincipal(Principal::Agent {
                agent_name: "session-agent".into(),
                credential_id: "cred-session".into(),
            }))
            .expect("overlay read grant can read SKILL.md");
    }

    /// The audit summary contains sizes and counts without result content.
    #[test]
    fn tool_call_summary_counts_but_does_not_leak_content() {
        use rmcp::model::{CallToolResult, ContentBlock, TextContent};

        let mut result = CallToolResult::success(Vec::new());
        result.structured_content = Some(serde_json::json!({
            "lines": [{"text": "secret body"}, {"text": "more"}],
            "session_id": "sess-secret"
        }));
        result.content = vec![ContentBlock::Text(TextContent::new("secret body"))];
        let summary = summarize_tool_result(&result);
        assert!(summary.contains("lines=2"), "{summary}");
        assert!(summary.contains("content_bytes=11"), "{summary}");
        assert!(!summary.contains("secret"), "{summary}");
    }
}
