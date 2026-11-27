//! End-to-end tests for the duthub-a2a loopback surface (W3 acceptance).
//!
//! Covers: Agent Card minimalism, Bearer fail-closed, A2A-Version negotiation
//! with `Vary`, the 3-method strict gate, task lifecycle (SendMessage →
//! GetTask Completed, CancelTask), messageId dedup, cross-DUT / evidence
//! scope rejections, the 256 KiB body limit, port gates and live token swap.

use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use duthub_core::ApprovalError;
use duthub_core::lease::WriteIdentity;
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, DutInfo, DutSummary, HeartbeatViewDto,
    LogLineDto, LogReadResult, McpState, ProcessResultDto,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use duthub_a2a::auth::{InMemoryTokenSource, SharedToken};
use duthub_a2a::executor::DuthubExecutor;
use duthub_a2a::listener::{A2aDutConfig, A2aRuntime, A2aStatus, build_listener_with_executor};
use duthub_a2a::ports::PortOwner;

const DUT_A: &str = "dut-a";
const DUT_NAME: &str = "RK3576";
const TOKEN: &str = "tok-a-3f9c";

// ── Stub McpState (counts evidence tool calls) ───────────────────────────────

#[derive(Debug, Clone, Default)]
struct StubState {
    evidence_calls: Arc<AtomicUsize>,
}

impl StubState {
    fn counted() -> (Arc<Self>, Arc<dyn McpState>) {
        let state = Arc::new(Self::default());
        let as_trait: Arc<dyn McpState> = state.clone();
        (state, as_trait)
    }

    fn calls(&self) -> usize {
        self.evidence_calls.load(Ordering::SeqCst)
    }
}

impl McpState for StubState {
    fn list_duts(&self) -> Vec<DutSummary> {
        self.evidence_calls.fetch_add(1, Ordering::SeqCst);
        vec![DutSummary {
            dut_id: DUT_A.to_owned(),
            name: DUT_NAME.to_owned(),
            source: "device".to_owned(),
            port: "COM8".to_owned(),
            baud: 115200,
        }]
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        if dut_id != DUT_A {
            return None;
        }
        Some(DutInfo {
            dut_id: DUT_A.to_owned(),
            name: DUT_NAME.to_owned(),
            source: "device".to_owned(),
            serial: duthub_mcp::state::SerialBindInfo {
                host: "local".to_owned(),
                port: "COM8".to_owned(),
                baud: 115200,
                format: "8N1".to_owned(),
            },
            adb_serial: None,
            relay: None,
            mcp_label: None,
            session_lines: Some(2),
        })
    }

    fn serial_read(&self, dut_id: &str, _since_seq: u64, _limit: usize) -> Option<LogReadResult> {
        if dut_id != DUT_A {
            return None;
        }
        Some(LogReadResult {
            dut_id: DUT_A.to_owned(),
            session_id: Some("sess-dut-a".to_owned()),
            last_seq: 2,
            lines: vec![
                LogLineDto {
                    seq: 1,
                    text: "kernel boot".to_owned(),
                },
                LogLineDto {
                    seq: 2,
                    text: "login:".to_owned(),
                },
            ],
        })
    }

    fn heartbeat_get(&self, dut_id: &str) -> Result<HeartbeatViewDto, String> {
        Ok(HeartbeatViewDto::new(
            dut_id,
            duthub_core::heartbeat::HeartbeatConfig::default(),
            duthub_core::heartbeat::HeartbeatState::default(),
        ))
    }

    fn serial_write(
        &self,
        _dut_id: &str,
        _text: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub rejects writes".to_owned())
    }

    fn adb_list(&self) -> Vec<AdbDeviceDto> {
        Vec::new()
    }

    fn adb_shell(
        &self,
        _dut_id: &str,
        _argv: &[String],
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("stub rejects adb shell".to_owned())
    }

    fn control_power(
        &self,
        _dut_id: &str,
        _on: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub rejects power".to_owned())
    }

    fn control_reset(
        &self,
        _dut_id: &str,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub rejects reset".to_owned())
    }

    fn control_power_key(
        &self,
        _dut_id: &str,
        _long_press: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub rejects power key".to_owned())
    }

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("stub rejects flash".to_owned())
    }

    fn approval_issue(&self, dut_id: &str, action: &str, _args_json: &str) -> ApprovalIssueResult {
        ApprovalIssueResult {
            token: "apr-stub".to_owned(),
            dut_id: dut_id.to_owned(),
            action: action.to_owned(),
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
            kind: ApprovalError::Missing,
        })
    }
}

// ── Router-level harness ─────────────────────────────────────────────────────

/// A stable card port for router-level tests (nothing actually binds it).
const CARD_PORT: u16 = 8790;

struct TestApp {
    app: axum::Router,
    state: Arc<StubState>,
    token: SharedToken,
}

fn test_app() -> TestApp {
    test_app_with_executor(|executor| executor)
}

fn test_app_with_executor(arm: impl FnOnce(DuthubExecutor) -> DuthubExecutor) -> TestApp {
    let (state, state_dyn) = StubState::counted();
    let token = duthub_a2a::auth::shared_token(TOKEN);
    let executor = arm(DuthubExecutor::new(state_dyn, DUT_A));
    let listener = build_listener_with_executor(
        executor,
        token.clone(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        CARD_PORT,
    );
    TestApp {
        app: listener.router,
        state,
        token,
    }
}

async fn post_json(
    app: axum::Router,
    body: &str,
    auth: Option<&str>,
    version: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = auth {
        builder = builder.header(header::AUTHORIZATION, token);
    }
    if let Some(version) = version {
        builder = builder.header("a2a-version", version);
    }
    app.oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body buffered");
    serde_json::from_slice(&bytes).expect("valid JSON body")
}

fn rpc_request(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()
}

fn send_message_params(message_id: &str, data_part: Value) -> Value {
    json!({
        "message": {
            "messageId": message_id,
            "role": "ROLE_USER",
            "contextId": "ctx-e2e",
            "parts": [{ "data": data_part }]
        }
    })
}

fn skill_part(skill: &str, dut_id: Option<&str>, args: Value) -> Value {
    let mut part = json!({ "skill": skill });
    if let Some(dut_id) = dut_id {
        part["dutId"] = json!(dut_id);
    }
    part["args"] = args;
    part
}

async fn rpc_error_code(response: axum::response::Response) -> Option<i64> {
    let value = body_json(response).await;
    value["error"]["code"].as_i64()
}

// ── Card ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn agent_card_is_minimal_public_discovery() {
    let TestApp { app, .. } = test_app();
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let card = body_json(response).await;

    assert_eq!(card["name"], json!("duthub"));
    assert_eq!(
        card["supportedInterfaces"][0]["protocolBinding"],
        json!("JSONRPC")
    );
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        json!(format!("http://127.0.0.1:{CARD_PORT}"))
    );
    assert_eq!(card["capabilities"]["streaming"], json!(false));
    assert_eq!(card["capabilities"]["pushNotifications"], json!(false));
    assert_eq!(card["capabilities"]["extendedAgentCard"], json!(false));
    assert_eq!(
        card["securitySchemes"]["duthub-bearer"]["httpAuthSecurityScheme"]["scheme"],
        json!("bearer")
    );
    // Wire format is protojson-flavoured: [{"schemes": {"duthub-bearer": {"list": []}}}].
    assert_eq!(
        card["securityRequirements"][0]["schemes"]["duthub-bearer"]["list"],
        json!([])
    );
    assert_eq!(card["skills"].as_array().map(Vec::len), Some(5));

    // A2A 0.2.x legacy keys must be gone (ADR-8).
    for key in ["url", "preferredTransport", "security"] {
        assert!(card.get(key).is_none(), "card must not carry {key}");
    }

    // No DUT identity, COM ports, paths, hostnames or token values.
    let text = serde_json::to_string(&card).unwrap();
    assert!(text.len() <= 16 * 1024, "card is {} bytes", text.len());
    for forbidden in [
        DUT_NAME,
        DUT_A,
        "COM8",
        "C:\\",
        "/home/",
        "192.168.",
        "10.0.",
        "localhost",
        "sk-",
        "tok-a",
    ] {
        assert!(
            !text.contains(forbidden),
            "card must not contain {forbidden:?}: {text}"
        );
    }
}

// ── Bearer fail-closed ───────────────────────────────────────────────────────

#[tokio::test]
async fn jsonrpc_requires_bearer_and_rejects_wrong_token() {
    let TestApp { app, .. } = test_app();
    let body = rpc_request("ListTasks", json!({}));

    let missing = post_json(app.clone(), &body, None, Some("1.0")).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        missing
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer")
    );

    let wrong = post_json(app.clone(), &body, Some("Bearer nope"), Some("1.0")).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let wrong_scheme = post_json(app.clone(), &body, Some("Basic nope"), Some("1.0")).await;
    assert_eq!(wrong_scheme.status(), StatusCode::UNAUTHORIZED);

    let ok = post_json(
        app.clone(),
        &body,
        Some(&format!("Bearer {TOKEN}")),
        Some("1.0"),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);
}

#[tokio::test]
async fn empty_bearer_never_matches() {
    let TestApp { app, token, .. } = test_app();
    // An empty live token must authenticate nothing (fail closed).
    {
        let mut guard = token.write().unwrap();
        *guard = Arc::from("");
    }
    let body = rpc_request("ListTasks", json!({}));
    let response = post_json(app, &body, Some("Bearer "), Some("1.0")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ── A2A-Version negotiation + Vary ───────────────────────────────────────────

#[tokio::test]
async fn a2a_version_missing_or_unsupported_rejected_with_vary() {
    let TestApp { app, .. } = test_app();
    let body = rpc_request("ListTasks", json!({}));

    for version in [None, Some("99.0"), Some("0.3")] {
        let response = post_json(
            app.clone(),
            &body,
            Some(&format!("Bearer {TOKEN}")),
            version,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let vary: Vec<String> = response
            .headers()
            .get_all(header::VARY)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect();
        assert!(
            vary.iter().any(|value| value == "A2A-Version"),
            "JSON-RPC responses must Vary on A2A-Version: {vary:?}"
        );
        let code = rpc_error_code(response).await;
        assert_eq!(code, Some(-32009), "version {version:?}");
    }

    let supported = post_json(
        app.clone(),
        &body,
        Some(&format!("Bearer {TOKEN}")),
        Some("1.0"),
    )
    .await;
    // ListTasks is gated with -32601 (below), but importantly NOT -32009.
    assert_eq!(rpc_error_code(supported).await, Some(-32601));
}

#[tokio::test]
async fn card_keeps_origin_vary_and_appends_a2a_version() {
    let TestApp { app, .. } = test_app();
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/.well-known/agent-card.json")
                .header(header::ORIGIN, "https://example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let vary: Vec<String> = response
        .headers()
        .get_all(header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_owned))
        .collect();
    assert!(
        vary.iter().any(|value| value == "Origin"),
        "card Vary must keep Origin: {vary:?}"
    );
    assert!(
        vary.iter().any(|value| value == "A2A-Version"),
        "card Vary must append A2A-Version: {vary:?}"
    );
}

// ── 3-method strict gate ─────────────────────────────────────────────────────

#[tokio::test]
async fn gated_methods_answer_method_not_found() {
    let TestApp { app, .. } = test_app();
    let auth = format!("Bearer {TOKEN}");
    let cases: Vec<(&str, Value)> = vec![
        ("ListTasks", json!({})),
        ("GetExtendedAgentCard", json!({})),
        ("SubscribeToTask", json!({ "id": "t-1" })),
        (
            "CreateTaskPushNotificationConfig",
            json!({ "taskId": "t-1", "url": "http://127.0.0.1:9000/hook" }),
        ),
        (
            "GetTaskPushNotificationConfig",
            json!({ "taskId": "t-1", "id": "p-1" }),
        ),
        (
            "ListTaskPushNotificationConfigs",
            json!({ "taskId": "t-1" }),
        ),
        (
            "DeleteTaskPushNotificationConfig",
            json!({ "taskId": "t-1", "id": "p-1" }),
        ),
        (
            "SendStreamingMessage",
            send_message_params("m-g", skill_part("dut_list", None, json!({}))),
        ),
    ];
    for (method, params) in cases {
        let response = post_json(
            app.clone(),
            &rpc_request(method, params),
            Some(&auth),
            Some("1.0"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        let code = rpc_error_code(response).await;
        assert_eq!(code, Some(-32601), "method {method} must be gated");
    }
}

// ── Task lifecycle ───────────────────────────────────────────────────────────

#[tokio::test]
async fn send_message_runs_dut_list_and_get_task_returns_completed() {
    let TestApp { app, state, .. } = test_app();
    let auth = format!("Bearer {TOKEN}");

    let response = post_json(
        app.clone(),
        &rpc_request(
            "SendMessage",
            send_message_params("m-1", skill_part("dut_list", None, json!({}))),
        ),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert!(value["error"].is_null(), "unexpected error: {value}");
    let task = &value["result"]["task"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_COMPLETED"));
    let data = &task["artifacts"][0]["parts"][0]["data"];
    assert_eq!(data["duts"][0]["dut_id"], json!(DUT_A));
    assert_eq!(data["duts"][0]["name"], json!(DUT_NAME));
    let task_id = task["id"].as_str().expect("task id").to_owned();

    // GetTask round-trips the same task through the authorizer ownership gate.
    let response = post_json(
        app.clone(),
        &rpc_request("GetTask", json!({ "id": task_id })),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert!(value["error"].is_null(), "unexpected error: {value}");
    assert_eq!(
        value["result"]["status"]["state"],
        json!("TASK_STATE_COMPLETED")
    );
    assert_eq!(value["result"]["id"], json!(task_id));

    assert_eq!(state.calls(), 1);
}

#[tokio::test]
async fn cancel_task_transitions_working_task_to_canceled() {
    let TestApp { app, .. } =
        test_app_with_executor(|executor| executor.with_test_delay(Duration::from_secs(30)));
    let auth = format!("Bearer {TOKEN}");

    // returnImmediately: response returns on the first status event (Working).
    let params = send_message_params("m-cancel", skill_part("dut_list", None, json!({})));
    let with_config = json!({
        "message": params["message"],
        "configuration": { "returnImmediately": true }
    });
    let response = post_json(
        app.clone(),
        &rpc_request("SendMessage", with_config),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    let task = &value["result"]["task"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_WORKING"));
    let task_id = task["id"].as_str().expect("task id").to_owned();

    let response = post_json(
        app.clone(),
        &rpc_request("CancelTask", json!({ "id": task_id })),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert!(value["error"].is_null(), "cancel failed: {value}");
    assert_eq!(
        value["result"]["status"]["state"],
        json!("TASK_STATE_CANCELED")
    );

    let response = post_json(
        app,
        &rpc_request("GetTask", json!({ "id": task_id })),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    let value = body_json(response).await;
    assert_eq!(
        value["result"]["status"]["state"],
        json!("TASK_STATE_CANCELED")
    );
}

#[tokio::test]
async fn duplicate_message_id_replays_recorded_result_without_execution() {
    let TestApp { app, state, .. } = test_app();
    let auth = format!("Bearer {TOKEN}");
    let params = send_message_params("m-dup", skill_part("dut_list", None, json!({})));

    let first = post_json(
        app.clone(),
        &rpc_request("SendMessage", params.clone()),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    let first = body_json(first).await;
    assert_eq!(
        first["result"]["task"]["status"]["state"],
        json!("TASK_STATE_COMPLETED")
    );
    let first_task_id = first["result"]["task"]["id"].as_str().unwrap().to_owned();

    // Same messageId: no second MCP call, recorded artifact comes back on a
    // fresh task id (ADR-9: no promise of identical task id).
    let replay = post_json(
        app.clone(),
        &rpc_request("SendMessage", params),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    let replay = body_json(replay).await;
    assert_eq!(
        replay["result"]["task"]["status"]["state"],
        json!("TASK_STATE_COMPLETED")
    );
    let replay_task_id = replay["result"]["task"]["id"].as_str().unwrap().to_owned();
    assert_ne!(replay_task_id, first_task_id);
    assert_eq!(
        replay["result"]["task"]["artifacts"][0]["parts"][0]["data"]["duts"][0]["dut_id"],
        json!(DUT_A)
    );
    assert_eq!(state.calls(), 1, "replay must not execute the tool again");

    // A different messageId executes again.
    let fresh = post_json(
        app,
        &rpc_request(
            "SendMessage",
            send_message_params("m-fresh", skill_part("dut_list", None, json!({}))),
        ),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    let fresh = body_json(fresh).await;
    assert_eq!(
        fresh["result"]["task"]["status"]["state"],
        json!("TASK_STATE_COMPLETED")
    );
    assert_eq!(state.calls(), 2);
}

// ── Policy rejections land in TASK_STATE_FAILED ──────────────────────────────

async fn expect_policy_failure(app: axum::Router, data_part: Value, kind: &str) {
    let auth = format!("Bearer {TOKEN}");
    let message_id = format!("m-policy-{kind}");
    let response = post_json(
        app,
        &rpc_request("SendMessage", send_message_params(&message_id, data_part)),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    let task = &value["result"]["task"];
    assert_eq!(
        task["status"]["state"],
        json!("TASK_STATE_FAILED"),
        "{kind}: {value}"
    );
    let reason = task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(reason.contains("POLICY"), "{kind}: reason = {reason:?}");
}

#[tokio::test]
async fn cross_dut_request_is_rejected() {
    let TestApp { app, .. } = test_app();
    expect_policy_failure(
        app,
        skill_part("dut_status", Some("dut-b"), json!({})),
        "cross-dut",
    )
    .await;
}

#[tokio::test]
async fn smuggled_dut_in_args_is_rejected() {
    let TestApp { app, .. } = test_app();
    expect_policy_failure(
        app,
        json!({ "skill": "dut_status", "args": { "dutId": "dut-b" } }),
        "args-dut",
    )
    .await;
}

#[tokio::test]
async fn non_evidence_skills_are_rejected() {
    let TestApp { app, .. } = test_app();
    expect_policy_failure(
        app.clone(),
        skill_part("dut_files", None, json!({})),
        "dut_files",
    )
    .await;
    expect_policy_failure(
        app.clone(),
        skill_part("media_streams", None, json!({})),
        "media",
    )
    .await;
    expect_policy_failure(app, skill_part("flash_now", None, json!({})), "unknown").await;
}

#[tokio::test]
async fn text_part_only_message_is_rejected() {
    let TestApp { app, .. } = test_app();
    let auth = format!("Bearer {TOKEN}");
    let response = post_json(
        app,
        &rpc_request(
            "SendMessage",
            json!({
                "message": {
                    "messageId": "m-text",
                    "role": "ROLE_USER",
                    "parts": [{ "text": "please dump everything" }]
                }
            }),
        ),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    let task = &value["result"]["task"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_FAILED"));
    let reason = task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("DataPart"), "reason = {reason:?}");
}

// ── Body limit ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn oversized_body_is_rejected_with_413() {
    let TestApp { app, .. } = test_app();
    let padding = "a".repeat(300 * 1024);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "SendMessage",
        "params": {
            "message": {
                "messageId": "m-big",
                "role": "ROLE_USER",
                "parts": [{ "text": padding }]
            }
        }
    })
    .to_string();
    let response = post_json(app, &body, Some(&format!("Bearer {TOKEN}")), Some("1.0")).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ── Unknown tasks / unknown methods / envelope ───────────────────────────────

#[tokio::test]
async fn unknown_task_id_is_task_not_found() {
    let TestApp { app, .. } = test_app();
    let auth = format!("Bearer {TOKEN}");
    let response = post_json(
        app,
        &rpc_request("GetTask", json!({ "id": "no-such-task" })),
        Some(&auth),
        Some("1.0"),
    )
    .await;
    let code = rpc_error_code(response).await;
    assert_eq!(code, Some(-32001), "TASK_NOT_FOUND expected");
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let TestApp { app, .. } = test_app();
    let response = post_json(
        app,
        &rpc_request("FlashNow", json!({})),
        Some(&format!("Bearer {TOKEN}")),
        Some("1.0"),
    )
    .await;
    let code = rpc_error_code(response).await;
    assert_eq!(code, Some(-32601));
}

#[tokio::test]
async fn broken_jsonrpc_envelope_is_invalid_request() {
    let TestApp { app, .. } = test_app();
    let body =
        json!({ "jsonrpc": "1.0", "id": 1, "method": "SendMessage", "params": {} }).to_string();
    let response = post_json(app, &body, Some(&format!("Bearer {TOKEN}")), Some("1.0")).await;
    let code = rpc_error_code(response).await;
    assert_eq!(code, Some(-32600));
}

// ── Runtime: port gates, sign-off, missing token, token rotation ─────────────

fn free_port() -> u16 {
    let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
    listener.local_addr().expect("local addr").port()
}

fn config(dut_id: &str, port: u16, bind_addr: Option<&str>) -> A2aDutConfig {
    A2aDutConfig {
        dut_id: dut_id.to_owned(),
        port,
        bind_addr: bind_addr.map(str::to_owned),
        agent_name: String::new(),
    }
}

#[tokio::test]
async fn runtime_port_gates_and_signoff() {
    let runtime = A2aRuntime::with_token_source(Arc::new(InMemoryTokenSource::new([
        (DUT_A, TOKEN),
        ("dut-c", TOKEN),
    ])));
    let (_, state) = StubState::counted();

    // Duplicate port: first binds, second conflicts.
    let port = free_port();
    let outcomes = runtime
        .start(
            vec![config(DUT_A, port, None), config("dut-b", port, None)],
            state.clone(),
            vec![],
        )
        .await;
    assert_eq!(
        outcomes[0].1,
        A2aStatus::Running {
            endpoint: format!("http://127.0.0.1:{port}")
        }
    );
    assert_eq!(outcomes[1].1, A2aStatus::PortConflict);

    // Reserved port (seeded like MCP listener ports) is refused.
    let reserved = free_port();
    let outcomes = runtime
        .start(
            vec![config("dut-c", reserved, None)],
            state.clone(),
            vec![reserved],
        )
        .await;
    assert_eq!(outcomes[0].1, A2aStatus::PortConflict);

    // Port 0 is refused outright.
    let outcomes = runtime
        .start(vec![config("dut-c", 0, None)], state.clone(), vec![])
        .await;
    assert_eq!(outcomes[0].1, A2aStatus::PortConflict);

    // Non-loopback bind_addr needs product sign-off (ADR-2) and never binds.
    let port = free_port();
    let outcomes = runtime
        .start(
            vec![config("dut-c", port, Some("0.0.0.0"))],
            state.clone(),
            vec![],
        )
        .await;
    match &outcomes[0].1 {
        A2aStatus::NeedsProductSignoff { reason } => {
            assert!(reason.contains("ADR-2"), "reason = {reason:?}");
        }
        other => panic!("expected NeedsProductSignoff, got {other:?}"),
    }
    // Nothing took the port: a plain bind still succeeds.
    let probe = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
    assert!(probe.is_ok(), "sign-off rejection must not bind the port");

    runtime.shutdown().await;
    assert_eq!(runtime.status(DUT_A), A2aStatus::Disabled);
}

#[tokio::test]
async fn missing_token_fails_closed_without_binding() {
    let runtime = A2aRuntime::with_token_source(Arc::new(InMemoryTokenSource::default()));
    let port = free_port();
    let outcomes = runtime
        .start(
            vec![config(DUT_A, port, None)],
            StubState::counted().1,
            vec![],
        )
        .await;
    let reason = match &outcomes[0].1 {
        A2aStatus::Failed { reason } => reason.clone(),
        other => panic!("expected Failed, got {other:?}"),
    };
    assert!(reason.contains("not configured"), "reason = {reason:?}");
    assert_eq!(
        runtime.status(DUT_A),
        A2aStatus::Failed {
            reason: reason.clone()
        }
    );

    // The listener did NOT bind: the port is free for anyone else.
    let probe = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
    assert!(probe.is_ok(), "failed start must not hold the port");
    runtime.shutdown().await;
}

// Minimal HTTP/1.1 client for live-socket assertions (no HTTP client dep).
async fn http_post(
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, Vec<(String, String)>, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to A2A listener");
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    let (head, payload) = text
        .split_once("\r\n\r\n")
        .expect("HTTP response with headers");
    let mut lines = head.lines();
    let status_line = lines.next().expect("status line");
    let status = StatusCode::from_u16(
        status_line
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(500),
    )
    .unwrap();
    let response_headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            (name.trim().to_owned(), value.trim().to_owned())
        })
        .collect::<Vec<_>>();
    let value = serde_json::from_str::<Value>(payload.trim()).unwrap_or(Value::Null);
    (status, response_headers, value)
}

#[tokio::test]
async fn set_token_swaps_credential_without_rebind() {
    let runtime =
        A2aRuntime::with_token_source(Arc::new(InMemoryTokenSource::new([(DUT_A, TOKEN)])));
    let port = free_port();
    let outcomes = runtime
        .start(
            vec![config(DUT_A, port, None)],
            StubState::counted().1,
            vec![],
        )
        .await;
    assert!(matches!(outcomes[0].1, A2aStatus::Running { .. }));

    let body = rpc_request("ListTasks", json!({}));
    let (status, _, value) = http_post(
        port,
        "/",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("a2a-version", "1.0"),
        ],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        value["error"]["code"],
        json!(-32601),
        "gated but authorized"
    );

    runtime.set_token(DUT_A, "tok-a-rotated").await.unwrap();

    // Old token is dead immediately.
    let (status, _, _) = http_post(
        port,
        "/",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("a2a-version", "1.0"),
        ],
        &body,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "old token must be rejected"
    );

    // New token works.
    let (status, _, _) = http_post(
        port,
        "/",
        &[
            ("Authorization", "Bearer tok-a-rotated"),
            ("a2a-version", "1.0"),
        ],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Missing header stays 401.
    let (status, _, _) = http_post(port, "/", &[("a2a-version", "1.0")], &body).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    runtime.shutdown().await;
}

// ── Port registry unit-level surface ─────────────────────────────────────────

#[test]
fn port_registry_external_seed_blocks_a2a() {
    let registry = duthub_a2a::ports::PortRegistry::new();
    registry
        .reserve(
            8797,
            PortOwner::External {
                label: "mcp".to_owned(),
            },
        )
        .unwrap();
    assert!(registry.is_reserved(8797));
    assert!(
        registry
            .reserve(
                8797,
                PortOwner::A2a {
                    dut_id: DUT_A.to_owned(),
                },
            )
            .is_err()
    );
}
