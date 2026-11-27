//! `media_streams` with an `op` (`docs/design/CAPTURE_STREAM_BACKENDS.md` §5 / §7).
//!
//! - The argument contract: no `op` = today's status query; unknown keys
//!   (notably a legacy `seconds`) are rejected because there is no duration
//!   field by design.
//! - The authorization matrix: the query branch still needs only `Read`, the
//!   op branch needs `Capture`, and a DUT-bound listener rejects cross-DUT
//!   capture requests before they reach the bridge.
//! - The bridge mapping: state sources without capture answer
//!   `MEDIA_CAPTURE_UNAVAILABLE`, a dropped receiver answers
//!   `MEDIA_OP_DROPPED`, and replies map onto the documented JSON shapes.
//! - The handler never blocks a runtime worker: a pending op does not delay
//!   other tool calls on the same two-worker endpoint.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, CallToolResponse, ClientConfig};
use rmcp::service::{RunningService, ServiceError, serve_client};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::{Value, json};

use duthub_core::agent_access::{AgentAccessSettings, AgentCredential, DutAction, DutGrant};
use duthub_core::audit::AuditSink;
use duthub_core::lease::ControlLeaseManager;
use duthub_core::media_op::{MediaArtifact, MediaOp, MediaOpReply, MediaOpRequest, OpReceiver};
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::server::{
    MEDIA_RECORD_START_TIMEOUT, MEDIA_RECORD_STOP_TIMEOUT, MEDIA_SNAPSHOT_TIMEOUT, MediaOpArgs,
    MediaStreamsArgs,
};
use duthub_mcp::snapshot::SnapshotState;
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, DutInfo, DutSummary, LogReadResult, McpState,
    ProcessResultDto, SerialBindInfo,
};
use duthub_mcp::transport::http_router_on_dut;

// ── fake state source ───────────────────────────────────────────────────────

/// What the fake capture port should do with one submitted op.
#[derive(Clone)]
enum Reply {
    /// No capture capability at all (stdio snapshot / desktop not running).
    Unavailable,
    /// Accept the request but drop the reply sender: `MEDIA_OP_DROPPED`.
    Dropped,
    Snapshot,
    RecordingHandle,
    RecordingArtifact,
    Failed(&'static str),
    /// Accept, then answer after the delay (proves the handler yields).
    DelayedSnapshot(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Submitted {
    dut_id: String,
    source: Option<String>,
    kind: &'static str,
    note: Option<String>,
}

struct FakeState {
    reply: Reply,
    submitted: Mutex<Vec<Submitted>>,
}

impl FakeState {
    fn new(reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            reply,
            submitted: Mutex::new(Vec::new()),
        })
    }

    fn submitted(&self) -> Vec<Submitted> {
        self.submitted.lock().unwrap().clone()
    }
}

fn op_kind(op: &MediaOp) -> &'static str {
    match op {
        MediaOp::Snapshot { .. } => "snapshot",
        MediaOp::RecordStart { .. } => "recordStart",
        MediaOp::RecordStop { .. } => "recordStop",
    }
}

fn op_note(op: &MediaOp) -> Option<String> {
    match op {
        MediaOp::Snapshot { note } | MediaOp::RecordStart { note } => note.clone(),
        MediaOp::RecordStop { .. } => None,
    }
}

fn snapshot_artifact() -> MediaArtifact {
    MediaArtifact {
        path: r"C:\duthub\Alpha\0f2c.png".into(),
        first_path: None,
        rotated: false,
        bytes: 4096,
        width: Some(1024),
        height: Some(600),
        duration_secs: None,
        note: Some("boot-logo".into()),
        source: Some("hdmi".into()),
    }
}

fn recording_artifact() -> MediaArtifact {
    MediaArtifact {
        path: r"C:\duthub\Alpha\9a11.ts".into(),
        first_path: Some(r"C:\duthub\Alpha\4b7e.ts".into()),
        rotated: true,
        bytes: 8192,
        width: None,
        height: None,
        duration_secs: Some(95),
        note: Some("boot-banner".into()),
        source: Some("camera".into()),
    }
}

impl McpState for FakeState {
    fn media_streams(&self, _dut_id: &str) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn media_op(&self, dut_id: String, source: Option<String>, op: MediaOp) -> Option<OpReceiver> {
        let (kind, note) = (op_kind(&op), op_note(&op));
        self.submitted.lock().unwrap().push(Submitted {
            dut_id: dut_id.clone(),
            source: source.clone(),
            kind,
            note,
        });
        if matches!(self.reply, Reply::Unavailable) {
            return None;
        }
        let (request, receiver) = MediaOpRequest::channel(1, dut_id, source, op);
        // The real port keeps the request (and therefore the sender).
        let MediaOpRequest { reply, .. } = request;
        match self.reply.clone() {
            Reply::Unavailable => unreachable!("handled above"),
            Reply::Dropped => drop(reply),
            Reply::Snapshot => {
                let _ = reply.send(MediaOpReply::Artifact(snapshot_artifact()));
            }
            Reply::RecordingHandle => {
                let _ = reply.send(MediaOpReply::Recording(
                    duthub_core::media_op::RecordingHandle {
                        recording_id: 7,
                        source: "camera".into(),
                        first_path: r"C:\duthub\Alpha\4b7e.ts".into(),
                        started_at_ms: 1_760_000_000_000,
                        note: Some("boot-banner".into()),
                    },
                ));
            }
            Reply::RecordingArtifact => {
                let _ = reply.send(MediaOpReply::Artifact(recording_artifact()));
            }
            Reply::Failed(code) => {
                let _ = reply.send(MediaOpReply::Err(duthub_core::media_op::MediaOpError {
                    code,
                    message: "采集已结束".into(),
                }));
            }
            Reply::DelayedSnapshot(delay) => {
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = reply.send(MediaOpReply::Artifact(snapshot_artifact()));
                });
            }
        }
        Some(receiver)
    }

    fn list_duts(&self) -> Vec<DutSummary> {
        vec![DutSummary {
            dut_id: "dut-a".into(),
            name: "Alpha".into(),
            source: "idle".into(),
            port: "COM30".into(),
            baud: 115_200,
        }]
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        (dut_id == "dut-a").then(|| DutInfo {
            dut_id: "dut-a".into(),
            name: "Alpha".into(),
            source: "idle".into(),
            serial: SerialBindInfo {
                host: "local".into(),
                port: "COM30".into(),
                baud: 115_200,
                format: "8N1".into(),
            },
            adb_serial: None,
            relay: None,
            mcp_label: None,
            session_lines: None,
        })
    }

    fn serial_read(&self, _dut_id: &str, _since_seq: u64, _limit: usize) -> Option<LogReadResult> {
        None
    }

    fn serial_write(
        &self,
        _dut_id: &str,
        _text: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("fixture is read-only".into())
    }

    fn adb_list(&self) -> Vec<AdbDeviceDto> {
        Vec::new()
    }

    fn adb_shell(
        &self,
        _dut_id: &str,
        _argv: &[String],
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("fixture is read-only".into())
    }

    fn control_power(
        &self,
        _dut_id: &str,
        _on: bool,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("fixture is read-only".into())
    }

    fn control_reset(
        &self,
        _dut_id: &str,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("fixture is read-only".into())
    }

    fn control_power_key(
        &self,
        _dut_id: &str,
        _long_press: bool,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("fixture is read-only".into())
    }

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("fixture is read-only".into())
    }

    fn approval_issue(&self, dut_id: &str, action: &str, _args_json: &str) -> ApprovalIssueResult {
        ApprovalIssueResult {
            token: "apr-fixture".into(),
            dut_id: dut_id.into(),
            action: action.into(),
            expires_in_secs: 300,
        }
    }

    fn approval_validate(
        &self,
        _token: &str,
        _dut_id: &str,
        _action: &str,
        _args_json: &str,
    ) -> Result<(), ApprovalErr> {
        Err(ApprovalErr {
            kind: duthub_core::ApprovalError::Unknown,
        })
    }
}

// ── harness ─────────────────────────────────────────────────────────────────

struct Harness {
    client: RunningService<rmcp::RoleClient, ClientConfig>,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn shutdown(self) {
        self.client.cancel().await.ok();
        self.task.abort();
    }
}

fn access_context(agents: Vec<AgentCredential>) -> Arc<AccessContext> {
    Arc::new(AccessContext::new(
        duthub_core::AgentAccessRuntime::from_settings(AgentAccessSettings {
            enabled: true,
            agents,
            ..Default::default()
        }),
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    ))
}

fn credential(id: &str, token: &str, actions: Vec<DutAction>) -> AgentCredential {
    AgentCredential {
        id: id.into(),
        listener_id: None,
        agent_name: format!("agent-{id}"),
        token: token.into(),
        enabled: true,
        grants: vec![DutGrant {
            dut_id: "dut-a".into(),
            actions,
        }],
    }
}

async fn start(
    state: Arc<dyn McpState>,
    access: Arc<AccessContext>,
    bound_dut: Option<&str>,
    token: &str,
) -> Harness {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let router = http_router_on_dut(state, Some(access), None, bound_dut.map(Arc::from));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve router");
    });
    let client = serve_client(
        ClientConfig::default(),
        StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token.to_string()),
        ),
    )
    .await
    .expect("client init");
    Harness { client, task }
}

/// Every HTTP listener needs an auth context (access = None is a 401-only
/// endpoint, as in production), so the shape tests use this capture-capable
/// credential.
const CAPTURE_TOKEN: &str = "dth-media-ops-capture";

fn capture_access() -> Arc<AccessContext> {
    access_context(vec![credential(
        "cred-capture",
        CAPTURE_TOKEN,
        vec![DutAction::Read, DutAction::Capture],
    )])
}

async fn start_capture(state: Arc<dyn McpState>, bound_dut: Option<&str>) -> Harness {
    start(state, capture_access(), bound_dut, CAPTURE_TOKEN).await
}

async fn call(
    client: &RunningService<rmcp::RoleClient, ClientConfig>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let params = CallToolRequestParams::new(name.to_string())
        .with_arguments(arguments.as_object().expect("object args").clone());
    match client.call_tool_once(params).await {
        Ok(CallToolResponse::Complete(result)) => {
            if result.is_error.unwrap_or(false) {
                Err(format!("{result:?}"))
            } else {
                Ok(result.structured_content.unwrap_or(Value::Null))
            }
        }
        Ok(other) => Err(format!("unexpected intermediate response: {other:?}")),
        Err(ServiceError::McpError(error)) => Err(reformat_error(&error)),
        Err(other) => Err(format!("transport error: {other}")),
    }
}

async fn call_error(
    client: &RunningService<rmcp::RoleClient, ClientConfig>,
    name: &str,
    arguments: Value,
) -> rmcp::ErrorData {
    let params = CallToolRequestParams::new(name.to_string())
        .with_arguments(arguments.as_object().expect("object args").clone());
    match client.call_tool_once(params).await {
        Err(ServiceError::McpError(error)) => error,
        Err(other) => panic!("{name} failed outside the MCP error channel: {other}"),
        Ok(other) => panic!("{name} must fail, got {other:?}"),
    }
}

/// `ErrorData` keeps the machine-readable `data.code`; the assertion helpers
/// only need the message, so render both consistently.
fn reformat_error(error: &rmcp::ErrorData) -> String {
    match &error.data {
        Some(data) => format!("{} {data}", error.message),
        None => error.message.to_string(),
    }
}

fn error_code(error: &rmcp::ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

// ── argument contract ───────────────────────────────────────────────────────

#[test]
fn query_without_op_parses_to_none() {
    let args: MediaStreamsArgs = serde_json::from_value(json!({ "dutId": "dut-a" })).unwrap();
    assert_eq!(args.dut_id, "dut-a");
    assert!(args.op.is_none());
}

#[test]
fn every_op_kind_parses() {
    let args: MediaStreamsArgs = serde_json::from_value(json!({
        "dutId": "dut-a",
        "op": { "kind": "snapshot", "source": "hdmi", "note": "boot-logo" }
    }))
    .unwrap();
    assert!(matches!(args.op, Some(MediaOpArgs::Snapshot { .. })));

    let args: MediaStreamsArgs = serde_json::from_value(json!({
        "dutId": "dut-a",
        "op": { "kind": "recordStart", "source": "camera" }
    }))
    .unwrap();
    assert!(matches!(
        args.op,
        Some(MediaOpArgs::RecordStart { note: None, .. })
    ));

    let args: MediaStreamsArgs = serde_json::from_value(json!({
        "dutId": "dut-a",
        "op": { "kind": "recordStart" }
    }))
    .unwrap();
    let Some(MediaOpArgs::RecordStart { source, note }) = args.op else {
        panic!("recordStart must parse without source/note");
    };
    assert!(source.is_none() && note.is_none());

    let args: MediaStreamsArgs = serde_json::from_value(json!({
        "dutId": "dut-a",
        "op": { "kind": "recordStop", "recordingId": 7 }
    }))
    .unwrap();
    assert!(matches!(
        args.op,
        Some(MediaOpArgs::RecordStop { recording_id: 7 })
    ));
}

#[test]
fn unknown_or_missing_op_fields_are_rejected() {
    for arguments in [
        // No duration field exists; a legacy `seconds` must be an error.
        json!({"dutId": "dut-a", "op": {"kind": "snapshot", "seconds": 5}}),
        json!({"dutId": "dut-a", "op": {"kind": "recordStart", "seconds": 5}}),
        // recordStop takes the id only.
        json!({"dutId": "dut-a", "op": {"kind": "recordStop"}}),
        json!({"dutId": "dut-a", "op": {"kind": "recordStop", "recordingId": 7, "note": "x"}}),
        // Snake_case is not the wire form.
        json!({"dutId": "dut-a", "op": {"kind": "recordStop", "recording_id": 7}}),
        // Unknown op kind.
        json!({"dutId": "dut-a", "op": {"kind": "clip", "recordingId": 7}}),
        // Unknown top-level key.
        json!({"dutId": "dut-a", "seconds": 5}),
    ] {
        assert!(
            serde_json::from_value::<MediaStreamsArgs>(arguments.clone()).is_err(),
            "must be rejected: {arguments}"
        );
    }
}

#[tokio::test]
async fn tool_schema_advertises_the_op_contract_without_a_duration() {
    let harness = start_capture(FakeState::new(Reply::Dropped), None).await;
    let tools = harness.client.list_tools(None).await.expect("tools/list");
    let media = tools
        .tools
        .iter()
        .find(|tool| tool.name == "media_streams")
        .expect("media_streams stays registered");
    let schema = serde_json::to_string(&*media.input_schema).unwrap();
    for expected in ["op", "snapshot", "recordStart", "recordStop", "recordingId"] {
        assert!(
            schema.contains(expected),
            "schema lacks {expected}: {schema}"
        );
    }
    assert!(
        !schema.contains("seconds"),
        "no duration field may be advertised: {schema}"
    );
    assert!(
        schema.contains("\"additionalProperties\":false"),
        "deny_unknown_fields must surface as additionalProperties=false: {schema}"
    );
    harness.shutdown().await;
}

// ── response shapes ─────────────────────────────────────────────────────────

#[tokio::test]
async fn query_response_matches_the_original_shape() {
    let state = FakeState::new(Reply::Dropped);
    let harness = start_capture(state.clone(), None).await;

    let data = call(&harness.client, "media_streams", json!({"dutId": "dut-a"}))
        .await
        .expect("query succeeds");
    assert_eq!(data, json!({ "streams": {} }));
    assert!(
        state.submitted().is_empty(),
        "query must not reach the bridge"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn snapshot_reply_maps_to_the_documented_artifact() {
    let harness = start_capture(FakeState::new(Reply::Snapshot), None).await;
    let data = call(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "snapshot", "note": "boot-logo"}}),
    )
    .await
    .expect("snapshot succeeds");
    assert_eq!(
        data,
        json!({
            "artifact": {
                "fileId": "0f2c.png",
                "resourceUri": "duthub://dut/dut-a/files/0f2c.png",
                "path": r"C:\duthub\Alpha\0f2c.png",
                "width": 1024,
                "height": 600,
                "bytes": 4096,
                "note": "boot-logo",
                "source": "hdmi"
            }
        })
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn record_start_reply_returns_the_recording_id() {
    let harness = start_capture(FakeState::new(Reply::RecordingHandle), None).await;
    let data = call(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "recordStart", "source": "camera", "note": "boot-banner"}}),
    )
    .await
    .expect("recordStart succeeds");
    assert_eq!(
        data,
        json!({
            "recording": {
                "recordingId": 7,
                "source": "camera",
                "firstPath": r"C:\duthub\Alpha\4b7e.ts",
                "note": "boot-banner",
                "startedAtMs": 1_760_000_000_000u64
            }
        })
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn record_stop_reply_reports_first_last_and_measured_duration() {
    let harness = start_capture(FakeState::new(Reply::RecordingArtifact), None).await;
    let data = call(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "recordStop", "recordingId": 7}}),
    )
    .await
    .expect("recordStop succeeds");
    assert_eq!(
        data,
        json!({
            "artifact": {
                "fileId": "9a11.ts",
                "resourceUri": "duthub://dut/dut-a/files/9a11.ts",
                "path": r"C:\duthub\Alpha\9a11.ts",
                "firstPath": r"C:\duthub\Alpha\4b7e.ts",
                "rotated": true,
                "durationSecs": 95,
                "bytes": 8192,
                "note": "boot-banner",
                "source": "camera"
            }
        })
    );
    harness.shutdown().await;
}

// ── authorization matrix ────────────────────────────────────────────────────

#[tokio::test]
async fn read_only_credential_queries_but_cannot_operate_capture() {
    let state = FakeState::new(Reply::Dropped);
    let access = access_context(vec![credential(
        "cred-read",
        "tok-read",
        vec![DutAction::Read],
    )]);
    let harness = start(state.clone(), access, None, "tok-read").await;

    let data = call(&harness.client, "media_streams", json!({"dutId": "dut-a"}))
        .await
        .expect("read grant still answers the status query");
    assert_eq!(data, json!({ "streams": {} }));

    let error = call_error(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "snapshot"}}),
    )
    .await;
    assert!(
        error.message.contains("UNAUTHORIZED"),
        "read-only token must not capture: {error}"
    );
    assert_eq!(error_code(&error), "UNAUTHORIZED");
    assert!(
        state.submitted().is_empty(),
        "a denied op must never reach the bridge"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn capture_grant_enters_the_bridge_and_dropped_replies_are_reported() {
    let state = FakeState::new(Reply::Dropped);
    let access = access_context(vec![credential(
        "cred-capture",
        "tok-capture",
        vec![DutAction::Read, DutAction::Capture],
    )]);
    let harness = start(state.clone(), access, None, "tok-capture").await;

    let error = call_error(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "snapshot", "source": "hdmi", "note": "a\nb"}}),
    )
    .await;
    assert_eq!(
        error_code(&error),
        "MEDIA_OP_DROPPED",
        "a dropped receiver is an op-drop, not a timeout: {error}"
    );

    let submitted = state.submitted();
    assert_eq!(
        submitted,
        vec![Submitted {
            dut_id: "dut-a".into(),
            source: Some("hdmi".into()),
            kind: "snapshot",
            // Metadata only: control characters are stripped before the DTO
            // leaves the MCP layer.
            note: Some("ab".into()),
        }]
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn notes_are_capped_before_they_enter_the_bridge() {
    let state = FakeState::new(Reply::Dropped);
    let harness = start_capture(state.clone(), None).await;
    let long_note = "x".repeat(200);
    let _ = call_error(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "recordStart", "note": long_note}}),
    )
    .await;
    let submitted = state.submitted();
    assert_eq!(submitted.len(), 1);
    assert_eq!(
        submitted[0].note.as_deref().map(str::len),
        Some(64),
        "note is capped at 64 characters"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn bridge_errors_keep_their_media_code() {
    let harness = start_capture(FakeState::new(Reply::Failed("MEDIA_RECORDING_ENDED")), None).await;
    let error = call_error(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "recordStop", "recordingId": 7}}),
    )
    .await;
    assert_eq!(error_code(&error), "MEDIA_RECORDING_ENDED");
    assert!(
        error.message.contains("MEDIA_RECORDING_ENDED"),
        "the code stays in the message for prefix parsing: {error}"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn bound_listener_rejects_cross_dut_capture() {
    let state = FakeState::new(Reply::Dropped);
    let access = access_context(vec![credential(
        "cred-capture",
        "tok-capture",
        vec![DutAction::Read, DutAction::Capture],
    )]);
    let harness = start(state.clone(), access, Some("dut-a"), "tok-capture").await;

    let error = call_error(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-b", "op": {"kind": "snapshot"}}),
    )
    .await;
    assert!(
        error.message.contains("UNAUTHORIZED"),
        "cross-DUT capture must be denied by the bound listener: {error}"
    );
    assert_eq!(state.submitted(), [] as [Submitted; 0]);

    // The bound DUT itself still works through the same listener.
    let error = call_error(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "snapshot"}}),
    )
    .await;
    assert_eq!(error_code(&error), "MEDIA_OP_DROPPED");
    assert_eq!(state.submitted().len(), 1);

    harness.shutdown().await;
}

// ── state sources without capture ───────────────────────────────────────────

#[tokio::test]
async fn snapshot_state_default_port_answers_capture_unavailable() {
    // The stdio snapshot / self-test state source keeps the trait default: no
    // capture capability, so the tool answers without entering the bridge.
    let state = SnapshotState::load(std::path::Path::new(
        "duthub-mcp-media-ops-test-missing-snapshot.json",
    ));
    let harness = start_capture(Arc::new(state), None).await;

    let error = call_error(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "snapshot"}}),
    )
    .await;
    assert_eq!(error_code(&error), "MEDIA_CAPTURE_UNAVAILABLE");

    let data = call(&harness.client, "media_streams", json!({"dutId": "dut-a"}))
        .await
        .expect("the query branch stays available");
    assert_eq!(data, json!({ "streams": {} }));

    harness.shutdown().await;
}

#[tokio::test]
async fn explicit_unavailable_port_answers_capture_unavailable() {
    let state = FakeState::new(Reply::Unavailable);
    let harness = start_capture(state.clone(), None).await;
    let error = call_error(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "recordStart"}}),
    )
    .await;
    assert_eq!(error_code(&error), "MEDIA_CAPTURE_UNAVAILABLE");
    // The request was submitted before the port refused it.
    assert_eq!(state.submitted().len(), 1);
    harness.shutdown().await;
}

// ── non-blocking + client ceilings ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_capture_does_not_block_other_tool_calls() {
    let delay = Duration::from_millis(400);
    let harness = start_capture(FakeState::new(Reply::DelayedSnapshot(delay)), None).await;

    let started = Instant::now();
    let pending = call(
        &harness.client,
        "media_streams",
        json!({"dutId": "dut-a", "op": {"kind": "snapshot"}}),
    );
    let other = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let call_started = Instant::now();
        let result = call(&harness.client, "dut_list", json!({})).await;
        (result, call_started.elapsed())
    };
    let (pending, (other, other_elapsed)) = tokio::join!(pending, other);

    assert!(
        pending.is_ok(),
        "the delayed snapshot still succeeds: {pending:?}"
    );
    assert!(other.is_ok(), "dut_list must keep working: {other:?}");
    assert!(
        other_elapsed < delay,
        "dut_list waited {other_elapsed:?} behind a pending op (runtime worker blocked)"
    );
    assert!(
        started.elapsed() >= delay,
        "the snapshot call must actually have waited for its reply"
    );

    harness.shutdown().await;
}

#[test]
fn client_timeouts_are_thirty_seconds_and_do_not_depend_on_recording_length() {
    assert_eq!(MEDIA_SNAPSHOT_TIMEOUT, Duration::from_secs(30));
    assert_eq!(MEDIA_RECORD_START_TIMEOUT, Duration::from_secs(30));
    assert_eq!(MEDIA_RECORD_STOP_TIMEOUT, Duration::from_secs(30));
}
