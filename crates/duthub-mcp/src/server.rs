//! MCP ServerHandler 实现：工具集与 `docs/API_CONTRACT.md` 对齐。
//!
//! MCP 先检查工具白名单，再进行凭据和 DUT 授权。
//! 内部设备服务保留原有权限、租约与审批校验。
//!
//! 授权三层（SPEC §14 / 方案 §3 TOCTOU）：
//! - [B] 授权（Grant）：auth 开启时写工具按 DutAction 校验凭据授权，deny by default；
//! - [C] 租约：写工具凭 `dut_acquire_control` 取得的 lease_id（归属当前凭据 +
//!   generation 复核）构造写身份，device 写入通道再做 [D]/[E] 双点复核；
//! - 审批令牌保留为高危动作二次确认（Power/Reset/PowerKey/relay_test/无租约写）。
//!
//! 校验失败返回 isError 工具结果（`LeaseRequired/{code}` / `ApprovalRequired/{code}`）。

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

/// Name of the user-notification tool. It is the only tool outside the
/// read-only Plan surface and exists only on listeners with a notifier.
pub const NOTIFY_TOOL_NAME: &str = "notify_user";

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
    /// 只返回 seq > since_seq 的行（游标续读）
    #[serde(default)]
    pub since_seq: u64,
    #[serde(default = "default_limit")]
    pub limit: usize,
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
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
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
    /// 高危二次确认令牌：未携带 lease_id 时必填（现状兼容）；
    /// 携带有效 lease_id 时租约即控制凭据（审批豁免，方案 §6.2）
    #[serde(default)]
    pub approval_token: Option<String>,
    /// 控制租约句柄（dut_acquire_control 返回；归属当前凭据 + generation 复核）
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialWriteRequest {
    #[serde(flatten)]
    args: SerialWriteArgs,
    /// 每次新操作生成唯一值。连接重建或请求超时后，在五分钟内用相同 ID 和 text 重试。
    request_id: Option<String>,
}

const WRITE_RECEIPT_TTL: Duration = Duration::from_secs(300);
const WRITE_RECEIPT_LIMIT: usize = 4096;
const MAX_LEASE_WAIT_MS: u64 = 120_000;
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
    /// argv 形式（不含 adb 本体与 -s），例如 ["shell", "getprop", "ro.product.model"]
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
    /// false=短按（开机）true=长按（强制断电）
    pub long_press: bool,
    pub approval_token: String,
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RelayTestArgs {
    pub dut_id: String,
    /// 继电器通道号（1 起始；仅测试该单通道，其他通道不动）
    pub channel: u8,
    pub approval_token: String,
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcquireControlArgs {
    pub dut_id: String,
    /// 忙时是否排队等待（FIFO）
    #[serde(default)]
    pub wait: bool,
    /// 等待上限毫秒（wait=true 时生效；0 = 不等待，忙即拒）
    #[serde(default)]
    pub deadline_ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaseIdArgs {
    pub dut_id: String,
    /// dut_acquire_control 返回的租约句柄
    pub lease_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutLabelArgs {
    pub dut_id: String,
    /// 分段标记文本（`[mark] {label}`；示例「烧录前」「重启后第 1 次采样」）
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
    /// settings.flashTools[].id（稳定预设 id，非可执行路径）
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
    /// 被授权操作的参数（JSON 对象，用于绑定校验；control.test 绑定 {"channel": N}）
    pub args: serde_json::Value,
}

/// 审批校验失败 → isError 工具结果（含引导提示）
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
fn media_artifact_json(artifact: MediaArtifact) -> serde_json::Value {
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
    object.insert("path".to_owned(), path.into());
    match first_path {
        Some(first_path) => {
            object.insert("firstPath".to_owned(), first_path.into());
            object.insert("rotated".to_owned(), rotated.into());
            if let Some(duration_secs) = duration_secs {
                object.insert("durationSecs".to_owned(), duration_secs.into());
            }
        }
        None => {
            if let Some(width) = width {
                object.insert("width".to_owned(), width.into());
            }
            if let Some(height) = height {
                object.insert("height".to_owned(), height.into());
            }
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

/// MCP Server 核心。`state` 由宿主注入（进程内 Runtime 或快照文件）；
/// `access` 为 None 时（stdio 快照）按 LocalAdmin 语义、不做授权/租约复核
/// （写工具由状态源只读限制拒绝）。
#[derive(Clone)]
pub struct McpServerCore {
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    /// 监听器对外地址（SKILL.md 生成用；局域网模式为探测到的局域网 IP）。
    listener_endpoint: Option<SocketAddr>,
    /// DUT 专用监听器绑定的 DUT；`None` = 通用监听器（LLM 全局读取）。
    listener_dut: Option<String>,
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
        server.listener_dut = Some(dut_id.to_owned());
        server.evidence_scope = true;
        server
    }

    pub(crate) fn with_listener_endpoint(mut self, endpoint: Option<SocketAddr>) -> Self {
        self.listener_endpoint = endpoint;
        self
    }

    pub(crate) fn with_listener_dut(mut self, dut_id: Option<&str>) -> Self {
        self.listener_dut = dut_id.map(str::to_owned);
        self
    }

    /// Attach (or clear) the host notifier for `notify_user`.
    ///
    /// Only user-configured MCP listeners pass `Some`; every desktop-issued
    /// Agent session listener keeps `None`, so those Agents cannot reach the
    /// tool at all.
    pub fn with_user_notifier(mut self, notifier: Option<Arc<dyn UserNotifier>>) -> Self {
        self.notifier = notifier;
        self
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

    /// [B] 授权（SPEC §5.2 deny by default）：LocalAdmin（auth 关/stdio）全通过；
    /// auth 开启时按 DutAction 校验凭据 Grant，拒绝记审计 grant.deny。
    fn authorize(
        &self,
        principal: &Principal,
        dut_id: &str,
        action: DutAction,
    ) -> Result<(), rmcp::ErrorData> {
        // DUT 专用监听器：跨 DUT 一律拒绝，且不确认目标 DUT 是否存在。
        if let Some(bound) = &self.listener_dut
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

    /// 读类工具授权（dut 作用域）。
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
        let status = if !streams.is_empty() {
            "streaming"
        } else {
            match self.state.has_media_binding(dut_id) {
                Some(true) => "bound_not_started",
                Some(false) => "not_bound",
                None => return Ok(Json(serde_json::json!({ "streams": streams }))),
            }
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

    /// DUT 专用端点省略参数时的默认 DUT。
    fn scoped_dut<'a>(&'a self, requested: Option<&'a str>) -> Option<&'a str> {
        requested.or(self.listener_dut.as_deref())
    }

    /// 全局清单类工具授权（dut_list / adb_list_devices / skill_export）：
    /// 凭据需至少对一个 DUT 持有 read 授权。涉及 DUT 记录的结果仍按 DUT 过滤。
    fn authorize_any_read(&self, principal: &Principal) -> Result<(), rmcp::ErrorData> {
        if let Some(bound) = &self.listener_dut {
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
        // 合并授权视图（持久表 + 每会话 runtime overlay）：会话凭据的 read
        // 授权与持久凭据同样可见，停用/未知凭据一律拒绝。
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

    /// acquire 前置授权（SPEC §6.4 失败路径 Unauthorized）：该 DUT 任一写类 Grant。
    fn authorize_any_write(
        &self,
        principal: &Principal,
        dut_id: &str,
    ) -> Result<(), rmcp::ErrorData> {
        if let Some(bound) = &self.listener_dut
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

    /// [C] 租约复核（归属当前凭据 + generation）：构造携带 (lease_id, generation)
    /// 的写身份，device 写入通道再做 [D]/[E] 双点复核。
    /// - auth ON：lease_id 必填（先 dut_acquire_control），归属当前凭据；
    /// - auth OFF（偏差 7 过渡）：lease_id 可省（GUI 租约在场仍被 device 拒）；
    ///   提供时同样校验归属。
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

    /// renew/release 的持有者归属复核（Agent 凭据必须与租约持有者一致）。
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
}

#[tool_router]
impl McpServerCore {
    #[tool(
        description = "查询指定 DUT 的视频绑定、采集状态及已开启的 HTTP MPEG-TS 拉流地址。带 op 时按需取数：kind=snapshot 截取静帧并返回 artifact；kind=recordStart 开始录屏并返回 recordingId；kind=recordStop 凭 recordingId 取回 artifact（recordStart 与 recordStop 必须配对，录屏时长不设上限）。op 需要该 DUT 的 capture 授权；采集由桌面端执行。拉流地址需保密。"
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
        let Some(receiver) = self.state.media_op(args.dut_id, source, request_op) else {
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
                "artifact": media_artifact_json(artifact),
            }))),
            MediaOpReply::Recording(recording) => Ok(Json(serde_json::json!({
                "recording": media_recording_json(recording),
            }))),
            MediaOpReply::Err(error) => Err(media_op_error(error.code, &error.message)),
        }
    }

    #[tool(description = "列出当前凭据有权读取的 DUT 及连接状态（只读，不含凭据）")]
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
            .map(|d| serde_json::to_value(d).unwrap_or(serde_json::Value::Null))
            .collect();
        Ok(Json(serde_json::json!({ "duts": duts })))
    }

    #[tool(description = "返回单个 DUT 的绑定与状态（只读，不含凭据）")]
    fn dut_info(
        &self,
        Parameters(args): Parameters<DutInfoArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(&principal.0, &args.dut_id)?;
        match self.state.dut_info(&args.dut_id) {
            Some(info) => {
                let session_id = (info.source != "idle").then(|| format!("sess-{}", info.dut_id));
                let mut payload = serde_json::to_value(info).unwrap_or(serde_json::Value::Null);
                payload["session_id"] = serde_json::json!(session_id);
                payload["session_open"] = serde_json::json!(session_id.is_some());
                Ok(Json(payload))
            }
            None => Err(rmcp::ErrorData::invalid_params(
                format!("DUT 未找到: {}（会话未打开）", args.dut_id),
                None,
            )),
        }
    }

    #[tool(
        description = "读取 DUT 接收数据块，lines 中的元素可能含多行或部分行。sinceSeq 使用上次返回的 last_seq，序号可能因其他事件占用而跳跃。关闭会话的历史请使用 log_segments。"
    )]
    fn serial_read(
        &self,
        Parameters(args): Parameters<SerialReadArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize_read(&principal.0, &args.dut_id)?;
        let limit = args.limit.clamp(1, 400);
        match self.state.serial_read(&args.dut_id, args.since_seq, limit) {
            Some(result) => {
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
        Ok(Json(
            serde_json::json!({ "segments": segments, "nextOffset": next_offset }),
        ))
    }

    #[tool(
        description = "读取已落盘的 DUT 日志段。segmentId 来自 log_segments，offset 使用上次返回的 nextOffset。"
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
        let page =
            duthub_core::log_archive::read(&dir, &args.dut_id, &args.segment_id, args.offset)
                .map_err(|err| state_err(&err.to_string()))?;
        Ok(Json(serde_json::json!(page)))
    }

    #[tool(
        description = "读取工作目录内各 DUT 的资料目录。省略 dutId 时列出 DUT（DUT 专用端点默认到绑定 DUT），指定 dutId 时分页列出该目录内的普通文件。"
    )]
    fn dut_files(
        &self,
        Parameters(args): Parameters<DutFilesArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        let limit = args.limit.clamp(1, 200);
        // DUT 专用端点省略 dutId 时默认到绑定 DUT，避免误读别的 DUT 资料。
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
            let files = match duthub_core::dut_artifacts::list_files(&root, &dut.dut_id, &dut.name)
            {
                Ok(files) => files,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(state_err(&error.to_string())),
            };
            let next =
                (files.len() > args.offset.saturating_add(limit)).then(|| args.offset + limit);
            let files: Vec<_> = files.into_iter().skip(args.offset).take(limit).collect();
            return Ok(Json(
                serde_json::json!({ "files": files, "nextOffset": next }),
            ));
        }
        // 通用监听器保持既有全局只读语义（API_CONTRACT/SECURITY）：省略 dutId
        // 时列出全部 DUT，但仍要求凭据至少持有一个 read grant（零 grant 拒绝）。
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
        description = "获取 DUT 独占控制租约，按 expiresAtUnixMs 处理到期。忙时可设置 wait 和 deadlineMs（最多 120000）；等待超时返回 DUT_BUSY。"
    )]
    async fn dut_acquire_control(
        &self,
        Parameters(args): Parameters<AcquireControlArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
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
            self.state.dut_acquire_control(&args.dut_id, holder, 0)
        } else {
            let permit = self.lease_waits.clone().try_acquire_owned().map_err(|_| {
                rmcp::ErrorData::new(
                    rmcp::model::ErrorCode(-32000),
                    "控制租约等待队列已满，请稍后重试",
                    Some(serde_json::json!({ "kind": "lease", "code": "DUT_BUSY" })),
                )
            })?;
            let state = self.state.clone();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                state.dut_acquire_control(&args.dut_id, holder, wait_ms)
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
            "holder": grant.holder,
            "queued": wait_ms > 0,
        })))
    }

    #[tool(
        description = "续租控制租约（TTL 重置，generation 不变；持有者本人）：过期/撤销返回 LeaseRequired/LEASE_EXPIRED，Protected 返回 DUT_PROTECTED"
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
        let mut receipts = self.write_receipts.lock().unwrap();
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
        if allowed.as_ref().is_some_and(|serials| serials.is_empty()) {
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
    fn control_power(
        &self,
        Parameters(args): Parameters<ControlPowerArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::Power)?;
        let identity = self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        // 授权（审批令牌）在共用设备操作入口（ControlService）统一校验并消费；
        // 租约复核也在该入口（admit 先于令牌消费，不通过不烧令牌）。
        self.state
            .control_power(&args.dut_id, args.on, &args.approval_token, identity)
            .map_err(|e| state_err(&e))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }

    #[tool(
        description = "DUT 电源按键（继电器脉冲 PWR_KEY；long_press=true 为长按强制断电；审批令牌必填二次确认，auth 开启时还需 leaseId）"
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
    fn relay_test(
        &self,
        Parameters(args): Parameters<RelayTestArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        self.authorize(&principal.0, &args.dut_id, DutAction::Relay)?;
        let identity = self.lease_identity(&principal.0, &args.dut_id, args.lease_id.as_deref())?;
        // 授权（审批令牌）与租约复核在共用设备操作入口
        // （ControlService::channel_test_as）统一校验并消费。
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
    fn flash_test(
        &self,
        Parameters(args): Parameters<FlashTestArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        // 只读白名单测试：按 read 授权放行（烧录预设 id 全局唯一）
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
    fn approval_request(
        &self,
        Parameters(args): Parameters<ApprovalRequestArgs>,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        // 可签发的动作清单必须与共用设备操作入口接受的动作一致
        //（control.* 在 duthub-device::control::ControlAction 中校验与消费，
        // control.test 在 ControlService::channel_test 中校验与消费）
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
    fn skill_export(
        &self,
        principal: CurrentPrincipal,
    ) -> Result<Json<serde_json::Value>, rmcp::ErrorData> {
        // 零 grant 的 HTTP 凭据不得读取 SKILL.md（stdio LocalAdmin 仍可用）。
        self.authorize_any_read(&principal.0)?;
        let mut markdown = crate::skill::skill_for_endpoint(self.listener_endpoint);
        if let Some(bound) = &self.listener_dut {
            markdown.push_str(&format!(
                "\n> 本端点仅可读 DUT `{bound}`；跨 DUT 请求返回 `LeaseRequired/UNAUTHORIZED`，且不开放写工具。\n"
            ));
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

/// The 13 read-only Plan tools exposed by every authenticated duthub MCP listener.
pub const PLAN_TOOL_NAMES: &[&str] = &[
    "dut_list",
    "dut_info",
    "dut_control_status",
    "serial_read",
    "analyze_serial",
    "log_segments",
    "log_segment_read",
    "dut_files",
    "dut_file_read",
    "heartbeat_get",
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
/// evidence, archived log segments and heartbeat health. File browsing,
/// media stream URLs, ADB discovery, skill export and every write stay off
/// this surface.
pub const EVIDENCE_TOOL_NAMES: [&str; 8] = [
    "dut_list",
    "dut_info",
    "dut_control_status",
    "serial_read",
    "analyze_serial",
    "log_segments",
    "log_segment_read",
    "heartbeat_get",
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

/// 从请求上下文取服务端注入的主体（HTTP 经 AuthSvc 注入 `Principal`）。
fn principal_in_context(
    context: &rmcp::service::RequestContext<rmcp::RoleServer>,
) -> Option<Principal> {
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<Principal>().cloned())
        .or_else(|| context.extensions.get::<Principal>().cloned())
}

/// 失败是否属于授权/租约/审批/执行策略拒绝（`tool.call` 审计的 Denied 口径）。
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

/// 结果摘要：只记长度/条数（数组项数、字符串字符数、文本字节数），绝不记正文。
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

/// Read-only Plan surface: the 13 tools every authenticated listener exposes.
static PLAN_ROUTER: LazyLock<ToolRouter<McpServerCore>> = LazyLock::new(|| {
    let mut router = McpServerCore::tool_router();
    for tool in router.list_all() {
        if !plan_tool_allowed(&tool.name) {
            router.remove_route(&tool.name);
        }
    }
    router
});

/// The Plan surface plus `notify_user`, selected only when a notifier is
/// attached. The tool count is 14 here and 13 everywhere else, so the visible
/// surface always matches the usable one.
static PLAN_ROUTER_WITH_NOTIFY: LazyLock<ToolRouter<McpServerCore>> = LazyLock::new(|| {
    let mut router = McpServerCore::tool_router();
    for tool in router.list_all() {
        if !plan_tool_allowed(&tool.name) && tool.name != NOTIFY_TOOL_NAME {
            router.remove_route(&tool.name);
        }
    }
    router
});

#[tool_handler(router = PLAN_ROUTER)]
impl ServerHandler for McpServerCore {
    /// Static route table for this instance: `PLAN_ROUTER_WITH_NOTIFY` when a
    /// notifier is attached, `PLAN_ROUTER` otherwise.
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

    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(Implementation::new("duthub", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Duthub 只支持 Plan 模式。HTTP 要求 Bearer Token，只能读取凭据获准的 DUT。用诊断证据生成计划。服务不会运行 Shell、写串口、烧录、控制继电器或改动配置。用户配置的 MCP 监听器额外提供 notify_user（可在用户界面弹出通知，受宿主限流与抑制约束）；桌面发起的 Agent 会话没有该工具。",
        )
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

    #[test]
    fn dut_bound_listener_reads_only_its_dut() {
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
        let server =
            McpServerCore::new(state, Some(Arc::new(access))).with_listener_dut(Some("dut-a"));
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
            .dut_info(
                Parameters(DutInfoArgs {
                    dut_id: "dut-b".into(),
                }),
                CurrentPrincipal(principal.clone()),
            )
            .err()
            .expect("跨 DUT 必须拒绝");
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
            .expect("跨 DUT 日志必须拒绝");
        assert!(
            other_logs.message.contains("UNAUTHORIZED"),
            "{other_logs:?}"
        );

        let other_files = server
            .dut_files(
                Parameters(DutFilesArgs {
                    dut_id: Some("dut-b".into()),
                    offset: 0,
                    limit: 10,
                }),
                CurrentPrincipal(principal.clone()),
            )
            .err()
            .expect("跨 DUT 文件必须拒绝");
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

    /// any-read 判定走合并授权：runtime overlay 的 read grant 同样可见。
    #[test]
    fn any_read_uses_merged_overlay_grants() {
        let server = McpServerCore::new(Arc::new(SnapshotState::empty()), Some(overlay_access()));
        let session = Principal::Agent {
            agent_name: "session-agent".into(),
            credential_id: "cred-session".into(),
        };
        server
            .authorize_any_read(&session)
            .expect("overlay read grant 必须放行 any-read");
        let unknown = Principal::Agent {
            agent_name: "unknown".into(),
            credential_id: "cred-unknown".into(),
        };
        let error = server
            .authorize_any_read(&unknown)
            .expect_err("未知凭据必须拒绝");
        assert!(error.message.contains("UNAUTHORIZED"), "{error:?}");
        server
            .authorize_any_read(&Principal::LocalAdmin)
            .expect("LocalAdmin 不受限");
    }

    /// skill_export：stdio LocalAdmin 可用；零 grant 的 HTTP 凭据拒绝；read grant 放行。
    #[test]
    fn skill_export_requires_any_read_grant_but_allows_local_admin() {
        let state = Arc::new(SnapshotState::empty());
        let stdio = McpServerCore::new(state.clone(), None);
        let Json(data) = stdio
            .skill_export(CurrentPrincipal(Principal::LocalAdmin))
            .expect("stdio LocalAdmin 可读 SKILL.md");
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
        let denied = match zero_grant.skill_export(CurrentPrincipal(Principal::Agent {
            agent_name: "no-grant".into(),
            credential_id: "cred-none".into(),
        })) {
            Ok(_) => panic!("零 grant 必须拒绝 skill_export"),
            Err(error) => error,
        };
        assert!(denied.message.contains("UNAUTHORIZED"), "{denied:?}");

        let granted = McpServerCore::new(state, Some(overlay_access()));
        granted
            .skill_export(CurrentPrincipal(Principal::Agent {
                agent_name: "session-agent".into(),
                credential_id: "cred-session".into(),
            }))
            .expect("overlay read grant 可读 SKILL.md");
    }

    /// tool.call 摘要只含长度/条数，不落正文。
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
