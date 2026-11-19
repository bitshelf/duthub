//! `notify_user` contract: tool surface, authorization, parameter validation
//! and the honest outcome plumbing between MCP and the host notifier.
//!
//! Every case goes through the real HTTP router (Bearer auth + rmcp client) so
//! the assertions describe what a client actually sees.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResponse, ClientConfig};
use rmcp::service::RunningService;
use rmcp::service::serve_client;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::{Value, json};

use duthub_core::agent_access::{
    AgentAccessSettings, AgentCredential, AgentListener, DutAction, DutGrant,
};
use duthub_core::audit::AuditSink;
use duthub_core::lease::ControlLeaseManager;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::notify::{NotifyLevel, NotifyOutcome, NotifyRequest, ThrottleReason, UserNotifier};
use duthub_mcp::server::McpServerCore;
use duthub_mcp::snapshot::SnapshotState;
use duthub_mcp::transport::{http_router_on_dut, http_router_on_dut_with_notifier};
use rmcp::ServerHandler;

const NOTIFY: &str = "notify_user";

/// Host stand-in: records every request and answers a scripted outcome.
struct ScriptedNotifier {
    outcomes: Mutex<VecDeque<NotifyOutcome>>,
    requests: Mutex<Vec<NotifyRequest>>,
}

impl ScriptedNotifier {
    fn queued() -> Arc<Self> {
        Self::with_outcomes(Vec::new())
    }

    fn with_outcomes(outcomes: Vec<NotifyOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<NotifyRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl UserNotifier for ScriptedNotifier {
    fn notify(&self, request: &NotifyRequest) -> NotifyOutcome {
        self.requests.lock().unwrap().push(request.clone());
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(NotifyOutcome::Queued)
    }
}

fn credential(
    id: &str,
    listener_id: Option<&str>,
    token: &str,
    grants: Vec<DutGrant>,
) -> AgentCredential {
    AgentCredential {
        listener_id: listener_id.map(str::to_owned),
        id: id.into(),
        agent_name: format!("agent-{id}"),
        token: token.into(),
        enabled: true,
        grants,
    }
}

fn read_grant(dut_id: &str) -> DutGrant {
    DutGrant {
        dut_id: dut_id.into(),
        actions: vec![DutAction::Read],
    }
}

fn access_context(
    agents: Vec<AgentCredential>,
    listeners: Vec<AgentListener>,
) -> Arc<AccessContext> {
    let auth = duthub_core::agent_access::AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents,
        listeners,
        ..Default::default()
    });
    Arc::new(AccessContext::new(
        auth,
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    ))
}

/// Generic (non DUT-bound) listener with a notifier attached.
fn generic_router(access: Arc<AccessContext>, notifier: Arc<ScriptedNotifier>) -> axum::Router {
    http_router_on_dut_with_notifier(
        Arc::new(SnapshotState::empty()),
        Some(access),
        None,
        None,
        Some(notifier),
    )
}

/// DUT-bound listener with a notifier attached.
fn bound_router(
    access: Arc<AccessContext>,
    listener_id: &str,
    dut_id: &str,
    notifier: Arc<ScriptedNotifier>,
) -> axum::Router {
    http_router_on_dut_with_notifier(
        Arc::new(SnapshotState::empty()),
        Some(access),
        Some(Arc::from(listener_id)),
        Some(Arc::from(dut_id)),
        Some(notifier),
    )
}

async fn connect(
    router: axum::Router,
    token: &str,
) -> (
    RunningService<rmcp::RoleClient, ClientConfig>,
    tokio::task::JoinHandle<()>,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token.to_string()),
    );
    let client = tokio::time::timeout(
        Duration::from_secs(10),
        serve_client(ClientConfig::default(), transport),
    )
    .await
    .expect("client init timeout")
    .expect("client init");
    (client, task)
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
        Ok(other) => Err(format!("unexpected tool response: {other:?}")),
        Err(error) => Err(error.to_string()),
    }
}

fn tool_names(tools: &[rmcp::model::Tool]) -> Vec<String> {
    tools.iter().map(|tool| tool.name.to_string()).collect()
}

/// No bearer, and a wrong bearer, are rejected at the HTTP layer before any
/// tool dispatch.
#[tokio::test]
async fn unauthenticated_requests_are_rejected_with_401() {
    let token = duthub_core::agent_access::generate_token();
    let access = access_context(
        vec![credential(
            "cred-a",
            None,
            &token,
            vec![read_grant("dut-a")],
        )],
        Vec::new(),
    );
    let router = generic_router(access, ScriptedNotifier::queued());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let client = reqwest::Client::new();
    let without = client
        .post(&url)
        .header("content-type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(without.status(), reqwest::StatusCode::UNAUTHORIZED);
    let wrong = client
        .post(&url)
        .header("content-type", "application/json")
        .bearer_auth("dth-wrong-token")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

    task.abort();
}

/// Without a notifier the endpoint keeps the 13-tool read-only surface and the
/// tool is not callable at all (Plan-mode rejection, not a disabled outcome).
#[tokio::test]
async fn endpoint_without_a_notifier_hides_and_rejects_notify_user() {
    let token = duthub_core::agent_access::generate_token();
    let access = access_context(
        vec![credential(
            "cred-a",
            None,
            &token,
            vec![read_grant("dut-a")],
        )],
        Vec::new(),
    );
    let router = http_router_on_dut(Arc::new(SnapshotState::empty()), Some(access), None, None);
    let (client, task) = connect(router, &token).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    let names = tool_names(&tools);
    assert_eq!(
        names.len(),
        13,
        "no notifier keeps the 13-tool surface: {names:?}"
    );
    assert!(!names.iter().any(|name| name == NOTIFY), "{names:?}");

    let error = call(&client, NOTIFY, json!({"message": "hello"}))
        .await
        .expect_err("notify_user must be unreachable without a notifier");
    assert!(error.contains("PLAN_MODE_ONLY"), "{error}");

    client.cancel().await.ok();
    task.abort();
}

/// With a notifier the endpoint exposes 14 tools and a notice without a DUT
/// binds to the listener's DUT.
#[tokio::test]
async fn dut_bound_endpoint_lists_fourteen_tools_and_queues_a_notice() {
    let token = duthub_core::agent_access::generate_token();
    let access = access_context(
        vec![credential(
            "cred-session",
            Some("dut-a-mcp"),
            &token,
            vec![read_grant("dut-a")],
        )],
        vec![AgentListener {
            id: "dut-a-mcp".into(),
            port: 0,
            dut_id: Some("dut-a".into()),
        }],
    );
    let notifier = ScriptedNotifier::queued();
    let router = bound_router(access, "dut-a-mcp", "dut-a", notifier.clone());
    let (client, task) = connect(router, &token).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    let names = tool_names(&tools);
    assert_eq!(names.len(), 14, "{names:?}");
    assert!(names.iter().any(|name| name == NOTIFY), "{names:?}");

    let data = call(&client, NOTIFY, json!({"message": "boot finished"}))
        .await
        .expect("notify_user on a notifier endpoint");
    assert_eq!(data["outcome"], "queued");
    assert_eq!(data["queued"], true);
    assert_eq!(data["source"], "agentNotice");
    assert!(data.get("reason").is_none(), "{data}");

    let requests = notifier.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].message, "boot finished");
    assert_eq!(requests[0].dut_id.as_deref(), Some("dut-a"));
    assert_eq!(requests[0].level, NotifyLevel::Info);
    assert_eq!(requests[0].credential_id.as_deref(), Some("cred-session"));

    // A foreign DUT is refused before the host sees anything.
    let error = call(&client, NOTIFY, json!({"message": "x", "dutId": "dut-b"}))
        .await
        .expect_err("cross-DUT notify must be rejected");
    assert!(error.contains("UNAUTHORIZED"), "{error}");
    assert_eq!(notifier.requests().len(), 1);

    client.cancel().await.ok();
    task.abort();
}

/// A generic listener token may only attach a notice to a DUT it holds a read
/// grant for; `authorize_any_read` alone would let it label any DUT.
#[tokio::test]
async fn generic_listener_rejects_an_explicit_foreign_dut() {
    let token = duthub_core::agent_access::generate_token();
    let access = access_context(
        vec![credential(
            "cred-a",
            None,
            &token,
            vec![read_grant("dut-a")],
        )],
        Vec::new(),
    );
    let notifier = ScriptedNotifier::queued();
    let router = generic_router(access, notifier.clone());
    let (client, task) = connect(router, &token).await;

    let error = call(&client, NOTIFY, json!({"message": "x", "dutId": "dut-b"}))
        .await
        .expect_err("explicit foreign DUT must be rejected");
    assert!(error.contains("UNAUTHORIZED"), "{error}");
    assert!(notifier.requests().is_empty());

    // The granted DUT and a global notice both work.
    let data = call(&client, NOTIFY, json!({"message": "x", "dutId": "dut-a"}))
        .await
        .expect("granted DUT is accepted");
    assert_eq!(data["outcome"], "queued");
    let data = call(&client, NOTIFY, json!({"message": "y"}))
        .await
        .expect("global notice is accepted");
    assert_eq!(data["outcome"], "queued");
    let requests = notifier.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].dut_id.as_deref(), Some("dut-a"));
    assert_eq!(requests[1].dut_id, None);

    client.cancel().await.ok();
    task.abort();
}

/// Zero-grant credentials are rejected even though the tool is not a device
/// action (the generic listener keeps the any-read gate).
#[tokio::test]
async fn generic_listener_without_grants_is_rejected() {
    let token = duthub_core::agent_access::generate_token();
    let access = access_context(
        vec![credential("cred-none", None, &token, Vec::new())],
        Vec::new(),
    );
    let notifier = ScriptedNotifier::queued();
    let router = generic_router(access, notifier.clone());
    let (client, task) = connect(router, &token).await;

    let error = call(&client, NOTIFY, json!({"message": "hello"}))
        .await
        .expect_err("zero grant must be rejected");
    assert!(error.contains("UNAUTHORIZED"), "{error}");
    assert!(notifier.requests().is_empty());

    client.cancel().await.ok();
    task.abort();
}

/// The response repeats what the host actually did, including the throttle
/// reason; nothing is upgraded to "queued".
#[tokio::test]
async fn outcomes_are_reported_verbatim() {
    let token = duthub_core::agent_access::generate_token();
    let access = access_context(
        vec![credential(
            "cred-a",
            None,
            &token,
            vec![read_grant("dut-a")],
        )],
        Vec::new(),
    );
    let notifier = ScriptedNotifier::with_outcomes(vec![
        NotifyOutcome::Suppressed,
        NotifyOutcome::Throttled(ThrottleReason::Dedup),
        NotifyOutcome::Throttled(ThrottleReason::Capacity),
        NotifyOutcome::Throttled(ThrottleReason::Rate),
        NotifyOutcome::Disabled,
        NotifyOutcome::Queued,
    ]);
    let router = generic_router(access, notifier.clone());
    let (client, task) = connect(router, &token).await;

    let expected = [
        ("suppressed", None),
        ("throttled", Some("dedup")),
        ("throttled", Some("capacity")),
        ("throttled", Some("rate")),
        ("disabled", None),
        ("queued", None),
    ];
    for (index, (outcome, reason)) in expected.into_iter().enumerate() {
        let data = call(
            &client,
            NOTIFY,
            json!({"message": format!("notice {index}")}),
        )
        .await
        .expect("notify_user succeeds even when not queued");
        assert_eq!(data["outcome"], outcome, "{data}");
        assert_eq!(data["queued"], outcome == "queued", "{data}");
        assert_eq!(data["source"], "agentNotice", "{data}");
        match reason {
            Some(reason) => assert_eq!(data["reason"], reason, "{data}"),
            None => assert!(data.get("reason").is_none(), "{data}"),
        }
    }
    assert_eq!(notifier.requests().len(), 6);

    client.cancel().await.ok();
    task.abort();
}

/// Parameter validation happens before the host call: an unknown level is an
/// error (never silently `info`) and an empty message is rejected.
#[tokio::test]
async fn invalid_level_and_empty_message_never_reach_the_host() {
    let token = duthub_core::agent_access::generate_token();
    let access = access_context(
        vec![credential(
            "cred-a",
            None,
            &token,
            vec![read_grant("dut-a")],
        )],
        Vec::new(),
    );
    let notifier = ScriptedNotifier::queued();
    let router = generic_router(access, notifier.clone());
    let (client, task) = connect(router, &token).await;

    let error = call(
        &client,
        NOTIFY,
        json!({"message": "x", "level": "critical"}),
    )
    .await
    .expect_err("unknown level must fail");
    assert!(error.contains("level"), "{error}");
    assert!(
        error.contains("invalid params") || error.contains("-32602"),
        "{error}"
    );

    for message in ["", "   ", "\u{0}\n\t"] {
        let error = call(&client, NOTIFY, json!({"message": message}))
            .await
            .expect_err("empty message must fail");
        assert!(error.contains("message"), "{error}");
    }

    // Unknown keys are rejected rather than ignored.
    let error = call(&client, NOTIFY, json!({"message": "x", "extra": true}))
        .await
        .expect_err("unknown keys must fail");
    assert!(error.contains("unknown field"), "{error}");

    assert!(notifier.requests().is_empty(), "host must not be called");

    client.cancel().await.ok();
    task.abort();
}

/// Control characters are stripped and both fields are capped by characters.
#[tokio::test]
async fn message_and_title_are_sanitized_and_capped() {
    let token = duthub_core::agent_access::generate_token();
    let access = access_context(
        vec![credential(
            "cred-a",
            None,
            &token,
            vec![read_grant("dut-a")],
        )],
        Vec::new(),
    );
    let notifier = ScriptedNotifier::queued();
    let router = generic_router(access, notifier.clone());
    let (client, task) = connect(router, &token).await;

    let message = format!("\u{0}{}", "测".repeat(260));
    let title = format!("\u{7}{}", "标".repeat(60));
    let data = call(
        &client,
        NOTIFY,
        json!({"message": message, "title": title, "level": "warning"}),
    )
    .await
    .expect("sanitized call succeeds");
    assert_eq!(data["outcome"], "queued");

    let requests = notifier.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.message.chars().count(), 200);
    assert!(request.message.chars().all(|ch| !ch.is_control()));
    assert_eq!(request.title.as_deref().unwrap().chars().count(), 48);
    assert_eq!(request.level, NotifyLevel::Warning);

    client.cancel().await.ok();
    task.abort();
}

/// The exported SKILL.md and the initialize instructions both state that
/// `notify_user` exists only on user-configured listeners.
#[test]
fn skill_and_instructions_document_notify_user_scope() {
    let markdown = duthub_mcp::skill::generate_skill_md();
    assert!(markdown.contains("`notify_user`"), "{markdown}");
    assert!(
        markdown.contains("仅用户配置的 MCP 监听器提供"),
        "SKILL.md must state the availability limit"
    );
    assert!(
        markdown.contains("queued") && markdown.contains("suppressed"),
        "SKILL.md must describe the truthful outcomes"
    );

    let server = McpServerCore::new(Arc::new(SnapshotState::empty()), None);
    let instructions = server.get_info().instructions.unwrap_or_default();
    assert!(instructions.contains("notify_user"), "{instructions}");
    assert!(
        instructions.contains("桌面发起的 Agent 会话没有该工具"),
        "{instructions}"
    );
}

/// The A2A evidence surface rejects the notification tool outright.
#[tokio::test]
async fn evidence_scope_rejects_notify_user() {
    let server = McpServerCore::for_evidence(Arc::new(SnapshotState::empty()), "dut-a");
    let (client_side, server_side) = tokio::io::duplex(1024 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let (server_read, server_write) = tokio::io::split(server_side);
    let server_task = tokio::spawn(async move {
        let running = rmcp::serve_server(
            server,
            AsyncRwTransport::new_server(server_read, server_write),
        )
        .await
        .expect("evidence server init");
        running.waiting().await
    });
    let client = serve_client(
        ClientConfig::default(),
        AsyncRwTransport::new_client(client_read, client_write),
    )
    .await
    .expect("evidence client init");

    let error = call(&client, NOTIFY, json!({"message": "hi"}))
        .await
        .expect_err("evidence surface has no notify tool");
    assert!(error.contains("EVIDENCE_SCOPE_ONLY"), "{error}");

    client.cancel().await.ok();
    server_task.abort();
}
