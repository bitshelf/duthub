//! MCP authorization/transport hardening (W1).
//!
//! - A zero-grant general token cannot read DUT files or archived log segments.
//! - Disabling a credential after a successful authentication invalidates it.
//! - The A2A evidence server rejects everything outside `EVIDENCE_TOOL_NAMES`
//!   while keeping `dut_list` available.
//! - Missing HTTP `Parts` on an access-bearing server fail closed (401) instead
//!   of degrading to `LocalAdmin`.
//! - 61 failed Bearer attempts answer 429 and aggregate the `auth.fail` audit.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResponse, ClientConfig};
use rmcp::service::serve_client;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::json;

use duthub_core::Principal;
use duthub_core::agent_access::{
    AgentAccessSettings, AgentCredential, DutAction, DutGrant, generate_token,
};
use duthub_core::audit::AuditSink;
use duthub_core::lease::ControlLeaseManager;

use duthub_mcp::auth_http::{AUTH_FAILURE_LIMIT, AccessContext, principal_from_parts};
use duthub_mcp::server::McpServerCore;
use duthub_mcp::snapshot::SnapshotState;
use duthub_mcp::transport::{ListenerHandle, spawn_http_listener_loopback};

fn credential(id: &str, token: &str, enabled: bool, grants: Vec<DutGrant>) -> AgentCredential {
    AgentCredential {
        id: id.into(),
        listener_id: None,
        agent_name: format!("agent-{id}"),
        token: token.into(),
        enabled,
        grants,
    }
}

fn read_grant(dut_id: &str) -> DutGrant {
    DutGrant {
        dut_id: dut_id.into(),
        actions: vec![DutAction::Read],
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

fn snapshot_state() -> Arc<SnapshotState> {
    let path = std::env::temp_dir().join(format!(
        "duthub-auth-hardening-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(
        &path,
        r#"{"version":1,"tsUnixMs":0,"duts":[
             {"dutId":"dut-a","name":"Alpha","source":"idle","serial":{"host":"local","port":"COM30","baud":115200,"format":"8N1"},"adbSerial":null,"lines":[]},
             {"dutId":"dut-b","name":"Beta","source":"idle","serial":{"host":"local","port":"COM31","baud":115200,"format":"8N1"},"adbSerial":null,"lines":[]}
        ]}"#,
    )
    .unwrap();
    let state = SnapshotState::load(&path);
    std::fs::remove_file(path).ok();
    Arc::new(state)
}

fn spawn(state: Arc<SnapshotState>, access: Arc<AccessContext>) -> ListenerHandle {
    spawn_http_listener_loopback(state, Some(access), None, None).expect("spawn loopback listener")
}

fn initialize_body() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "hardening-probe", "version": "0"}
        }
    })
    .to_string()
}

/// Blocking HTTP status probe (the listener runs its own runtime thread).
fn initialize_status(endpoint: SocketAddr, bearer: Option<&str>) -> u16 {
    use std::io::{Read, Write};

    let body = initialize_body();
    let mut stream = std::net::TcpStream::connect_timeout(&endpoint, Duration::from_secs(2))
        .expect("listener must accept connections");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let auth_line = bearer
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {endpoint}\r\n{auth_line}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    raw.split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or(0)
}

async fn connect(
    endpoint: SocketAddr,
    token: &str,
) -> rmcp::service::RunningService<rmcp::RoleClient, ClientConfig> {
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(format!("http://{endpoint}/mcp"))
            .auth_header(token),
    );
    serve_client(ClientConfig::default(), transport)
        .await
        .expect("client init")
}

fn call(name: &str, arguments: serde_json::Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name.to_owned())
        .with_arguments(arguments.as_object().unwrap().clone())
}

async fn tool_error(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ClientConfig>,
    name: &str,
    arguments: serde_json::Value,
) -> String {
    client
        .call_tool_once(call(name, arguments))
        .await
        .expect_err("tool call must be denied")
        .to_string()
}

#[tokio::test]
async fn zero_grant_general_token_cannot_read_files_or_logs() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let token = generate_token();
    let ctx = access_context(vec![credential("cred-zero", &token, true, Vec::new())]);
    let handle = spawn(snapshot_state(), ctx);
    let client = connect(handle.endpoint, &token).await;

    for (tool, arguments) in [
        ("dut_files", json!({})),
        ("log_segments", json!({"dutId": "dut-a"})),
        (
            "dut_file_read",
            json!({"dutId": "dut-a", "fileId": "trace.txt"}),
        ),
    ] {
        let error = tool_error(&client, tool, arguments).await;
        assert!(error.contains("UNAUTHORIZED"), "{tool}: {error}");
    }

    client.cancel().await.ok();
    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}

#[test]
fn disabling_a_credential_after_a_successful_auth_is_rejected() {
    let token = generate_token();
    let ctx = access_context(vec![credential(
        "cred-a",
        &token,
        true,
        vec![read_grant("dut-a")],
    )]);
    let handle = spawn(snapshot_state(), ctx.clone());
    let endpoint = handle.endpoint;
    let first = initialize_status(endpoint, Some(&token));
    assert!(
        first == 200 || first == 202,
        "valid token must authenticate (got {first})"
    );

    let mut settings = ctx.auth.settings();
    settings.agents[0].enabled = false;
    ctx.auth.update(settings);

    assert_eq!(
        initialize_status(endpoint, Some(&token)),
        401,
        "disabled credential must be rejected even after a prior success"
    );
    assert!(!ctx.auth.credential_enabled("cred-a"));
    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}

#[test]
fn repeated_bad_tokens_trip_429_and_aggregate_auth_fail_audit() {
    let good = generate_token();
    let ctx = access_context(vec![credential(
        "cred-a",
        &good,
        true,
        vec![read_grant("dut-a")],
    )]);
    let audit = ctx.audit.clone();
    let handle = spawn(snapshot_state(), ctx);
    let endpoint = handle.endpoint;

    for attempt in 0..AUTH_FAILURE_LIMIT {
        assert_eq!(
            initialize_status(endpoint, Some("dth-wrong")),
            401,
            "failure {attempt} must stay 401"
        );
    }
    // The attempt that crosses the budget is answered with 429.
    assert_eq!(initialize_status(endpoint, Some("dth-wrong")), 429);
    // Subsequent attempts inside the window stay limited and are not audited.
    for _ in 0..5 {
        assert_eq!(initialize_status(endpoint, Some("dth-wrong")), 429);
    }

    let failures: Vec<_> = audit
        .recent(200)
        .into_iter()
        .filter(|event| event.action == "auth.fail")
        .collect();
    assert_eq!(
        failures.len(),
        AUTH_FAILURE_LIMIT + 1,
        "only the budget plus one aggregated trip may be audited"
    );
    assert!(
        failures.iter().any(|event| {
            event
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("rate_limited"))
        }),
        "the trip must be audited with an aggregated marker"
    );
    let raw = format!("{failures:?}");
    assert!(!raw.contains("dth-wrong"), "audit leaked the bearer token");
    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}

#[test]
fn missing_http_parts_on_an_access_server_fails_closed() {
    let error = principal_from_parts(None, true).expect_err("access server must fail closed");
    assert!(error.message.contains("UNAUTHORIZED"), "{}", error.message);
    assert_eq!(
        principal_from_parts(None, false).unwrap(),
        Principal::LocalAdmin
    );
}

#[tokio::test]
async fn evidence_scope_allows_dut_list_but_rejects_files_and_media() {
    let server = McpServerCore::for_evidence(snapshot_state(), "dut-a");
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

    let listed = client
        .call_tool_once(CallToolRequestParams::new("dut_list"))
        .await
        .expect("dut_list is part of the evidence surface");
    let CallToolResponse::Complete(listed) = listed else {
        panic!("expected complete result")
    };
    let data = listed.structured_content.expect("structured content");
    assert_eq!(
        data["duts"].as_array().unwrap().len(),
        1,
        "evidence server stays bound to dut-a: {data}"
    );
    assert_eq!(data["duts"][0]["dut_id"], "dut-a");

    for (tool, arguments) in [
        ("dut_files", json!({})),
        ("media_streams", json!({"dutId": "dut-a"})),
        ("skill_export", json!({})),
    ] {
        let error = tool_error(&client, tool, arguments).await;
        assert!(error.contains("EVIDENCE_SCOPE_ONLY"), "{tool}: {error}");
    }

    client.cancel().await.ok();
    server_task.abort();
}
