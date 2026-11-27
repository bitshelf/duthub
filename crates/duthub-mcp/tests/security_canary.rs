//! Token theft and boundary-bypass canaries (W1 / T19):
//!
//! 1. The same OS user can read `settings.json`, whose **general** token can read all DUTs;
//!    this is an existing contract (not a bug to fix), and the canary verifies it.
//! 2. Persistent tokens and session (runtime overlay) tokens are not interchangeable: a
//!    session token cannot open the primary listener, a persistent token cannot open a
//!    session listener, and revocation returns 401 immediately.
//! 3. `tool.call` audit entries record metadata only (`tool`, `dutId`, `credential`,
//!    duration, result summary) and never record tokens or response bodies.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duthub_core::agent_access::{
    AgentAccessSettings, AgentCredential, AgentListener, DutAction, DutGrant, generate_token,
};
use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::lease::ControlLeaseManager;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::snapshot::SnapshotState;
use duthub_mcp::transport::{ListenerHandle, spawn_http_listener_loopback};

fn snapshot_path() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "duthub-canary-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(
        &path,
        r#"{"version":1,"tsUnixMs":0,"duts":[
             {"dutId":"dut-a","name":"Alpha","source":"idle","serial":{"host":"local","port":"COM30","baud":115200,"format":"8N1"},"adbSerial":"serial-a","lines":[]},
             {"dutId":"dut-b","name":"Beta","source":"idle","serial":{"host":"local","port":"COM31","baud":115200,"format":"8N1"},"adbSerial":"serial-b","lines":[]}
        ]}"#,
    )
    .unwrap();
    path
}

fn state() -> Arc<SnapshotState> {
    let path = snapshot_path();
    let state = SnapshotState::load(&path);
    std::fs::remove_file(path).ok();
    Arc::new(state)
}

fn general_credential(token: &str) -> AgentCredential {
    AgentCredential {
        id: "cred-general".into(),
        listener_id: None,
        agent_name: "general-agent".into(),
        token: token.into(),
        enabled: true,
        grants: vec![
            DutGrant {
                dut_id: "dut-a".into(),
                actions: vec![DutAction::Read],
            },
            DutGrant {
                dut_id: "dut-b".into(),
                actions: vec![DutAction::Read],
            },
        ],
    }
}

fn session_registration(token: &str) -> (AgentListener, AgentCredential) {
    (
        AgentListener {
            id: "session-a".into(),
            port: 0,
            dut_id: Some("dut-a".into()),
        },
        AgentCredential {
            id: "cred-session".into(),
            listener_id: Some("session-a".into()),
            agent_name: "session-agent".into(),
            token: token.into(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-a".into(),
                actions: vec![DutAction::Read],
            }],
        },
    )
}

async fn post_initialize(endpoint: SocketAddr, bearer: Option<&str>) -> Option<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "canary", "version": "0"}
        }
    })
    .to_string();
    let mut stream = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(endpoint),
    )
    .await
    .ok()?
    .ok()?;
    let auth_line = bearer.map_or_default(|token| format!("Authorization: Bearer {token}\r\n"));
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {endpoint}\r\n{auth_line}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut buffer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buffer)).await;
    String::from_utf8_lossy(&buffer)
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
}

async fn initialize_status(endpoint: SocketAddr, bearer: Option<&str>) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = post_initialize(endpoint, bearer).await {
            return status;
        }
        assert!(Instant::now() < deadline, "listener not ready: {endpoint}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn spawn(
    state: Arc<SnapshotState>,
    access: AccessContext,
    listener_id: Option<&str>,
    listener_dut: Option<&str>,
) -> ListenerHandle {
    spawn_http_listener_loopback(
        state,
        Some(Arc::new(access)),
        listener_id.map(Arc::from),
        listener_dut.map(Arc::from),
    )
    .expect("spawn loopback listener")
}

fn assert_port_released(endpoint: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(listener) = std::net::TcpListener::bind(endpoint) {
            drop(listener);
            return;
        }
        assert!(Instant::now() < deadline, "port not released: {endpoint}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Canary: the general token read from `settings.json` by the same OS user can read every DUT
/// on the general listener. This is an existing contract (ADR-7: external general listener
/// retains global read), not a boundary to remove.
#[tokio::test]
async fn general_token_in_settings_json_can_read_every_dut() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let token = generate_token();
    let dir = tempfile::tempdir().unwrap();
    let mut settings_file = duthub_core::store::default_settings();
    settings_file.agent_access = AgentAccessSettings {
        enabled: true,
        agents: vec![general_credential(&token)],
        ..Default::default()
    };
    duthub_core::store::save_settings_atomic(dir.path(), &settings_file).unwrap();
    // Same OS user can read the token directly from disk (canary premise; no file ACL assumed).
    let raw = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
    assert!(
        raw.contains(&token),
        "general token is stored in plaintext in settings.json as required"
    );
    let loaded = duthub_core::store::load_settings(dir.path()).unwrap();
    assert_eq!(loaded.agent_access, settings_file.agent_access);

    let state = state();
    let access = AccessContext::new(
        duthub_core::AgentAccessRuntime::from_settings(loaded.agent_access),
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    );
    let handle = spawn(state.clone(), access, None, None);
    let endpoint = handle.endpoint;
    assert_ne!(initialize_status(endpoint, Some(&token)).await, 401);
    assert_eq!(initialize_status(endpoint, None).await, 401);

    let transport =
        rmcp::transport::streamable_http_client::StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                format!("http://{endpoint}/mcp"),
            )
            .auth_header(token),
        );
    let client = rmcp::service::serve_client(rmcp::model::ClientConfig::default(), transport)
        .await
        .expect("general client init");
    let call = client
        .call_tool_once(rmcp::model::CallToolRequestParams::new("dut_list"))
        .await
        .expect("dut_list");
    let rmcp::model::CallToolResponse::Complete(result) = call else {
        panic!("expected complete result");
    };
    let data = result.structured_content.expect("structured content");
    assert_eq!(
        data["duts"].as_array().unwrap().len(),
        2,
        "general token can read all DUTs (existing contract): {data}"
    );
    client.cancel().await.ok();

    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
    assert_port_released(endpoint);
}

/// Stolen-token canary: persistent and session tokens cannot cross listeners, and revocation returns 401 immediately.
#[tokio::test]
async fn stolen_persistent_or_session_token_cannot_cross_listeners() {
    let persistent_token = generate_token();
    let session_token = generate_token();
    let settings = AgentAccessSettings {
        enabled: true,
        agents: vec![general_credential(&persistent_token)],
        ..Default::default()
    };
    let auth = duthub_core::AgentAccessRuntime::from_settings(settings);
    let (listener, credential) = session_registration(&session_token);
    auth.set_runtime_listener(listener, credential).unwrap();

    let access = AccessContext::new(
        auth.clone(),
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    );
    let primary = spawn(state(), access.clone(), None, None);
    let session = spawn(state(), access, Some("session-a"), Some("dut-a"));
    let primary_endpoint = primary.endpoint;
    let session_endpoint = session.endpoint;

    // Each token is valid on its own listener.
    assert_ne!(
        initialize_status(primary_endpoint, Some(&persistent_token)).await,
        401
    );
    assert_ne!(
        initialize_status(session_endpoint, Some(&session_token)).await,
        401
    );
    // Cross-listener use always returns 401 (session token rejected by primary, persistent token rejected by session).
    assert_eq!(
        initialize_status(primary_endpoint, Some(&session_token)).await,
        401,
        "session token must never open the persistent listener"
    );
    assert_eq!(
        initialize_status(session_endpoint, Some(&persistent_token)).await,
        401,
        "persistent token must never pass internal auth"
    );

    // Revoking the session listener immediately returns 401 for its token without affecting the primary listener.
    auth.remove_runtime_listener("session-a").unwrap();
    assert_eq!(
        initialize_status(session_endpoint, Some(&session_token)).await,
        401
    );
    assert_ne!(
        initialize_status(primary_endpoint, Some(&persistent_token)).await,
        401,
        "revoking the session listener must not affect the primary listener"
    );

    assert!(primary.shutdown_and_join_with_timeout(Duration::from_secs(5)));
    assert!(session.shutdown_and_join_with_timeout(Duration::from_secs(5)));
    assert_port_released(primary_endpoint);
    assert_port_released(session_endpoint);
}

/// `tool.call` audit records `tool`, `dutId`, `credential`, duration, and summary for both
/// allowed and denied calls, without recording tokens or response bodies.
#[tokio::test]
async fn tool_call_audit_records_metadata_but_no_token_or_body() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let token = generate_token();
    let auth = duthub_core::AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let (listener, credential) = session_registration(&token);
    auth.set_runtime_listener(listener, credential).unwrap();
    let audit = AuditSink::memory();
    let access = AccessContext::new(
        auth,
        Arc::new(ControlLeaseManager::new(None)),
        audit.clone(),
    );
    let handle = spawn(state(), access, Some("session-a"), Some("dut-a"));
    let endpoint = handle.endpoint;
    assert_ne!(initialize_status(endpoint, Some(&token)).await, 401);

    let transport =
        rmcp::transport::streamable_http_client::StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                format!("http://{endpoint}/mcp"),
            )
            .auth_header(token.clone()),
        );
    let client = rmcp::service::serve_client(rmcp::model::ClientConfig::default(), transport)
        .await
        .expect("session client init");

    // Ok: dut_list (bound DUT, returns 1 entry).
    client
        .call_tool_once(rmcp::model::CallToolRequestParams::new("dut_list"))
        .await
        .expect("dut_list");
    // Denied: cross-DUT read.
    let denied = client
        .call_tool_once(
            rmcp::model::CallToolRequestParams::new("dut_status").with_arguments(
                serde_json::json!({"dutId": "dut-b"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(denied.is_err(), "cross-DUT read must be rejected");
    client.cancel().await.ok();

    let events = audit.recent(100);
    let tool_calls: Vec<_> = events
        .iter()
        .filter(|event| event.action == "tool.call")
        .collect();
    let ok = tool_calls
        .iter()
        .find(|event| {
            event
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("tool=dut_list"))
        })
        .expect("dut_list must emit a tool.call audit entry");
    assert_eq!(ok.result, AuditOutcome::Ok);
    assert_eq!(ok.credential_id.as_deref(), Some("cred-session"));
    assert_eq!(ok.agent.as_deref(), Some("session-agent"));
    let detail = ok.detail.as_deref().unwrap();
    assert!(detail.contains("duration_ms="), "{detail}");
    assert!(detail.contains("content_bytes="), "{detail}");
    // Response body (DUT name or log text) must not appear in audit details.
    assert!(
        !detail.contains("Alpha"),
        "audit must not record response body: {detail}"
    );
    assert!(!detail.contains("alpha ready"), "{detail}");

    let denied = tool_calls
        .iter()
        .find(|event| {
            event
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("tool=dut_status"))
        })
        .expect("dut_status denial must emit a tool.call audit entry");
    assert_eq!(denied.result, AuditOutcome::Denied);
    assert_eq!(denied.dut.as_deref(), Some("dut-b"));
    assert!(denied.detail.as_deref().unwrap().contains("UNAUTHORIZED"));

    // No audit bytes may contain the session token.
    let raw = format!("{events:?}");
    assert!(!raw.contains(&token), "audit leaked token: {raw}");

    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
    assert_port_released(endpoint);
}

/// Regression guard: `settings.json` serialization never includes the runtime overlay.
#[test]
fn settings_json_shape_never_contains_runtime_overlay() {
    let rt = duthub_core::AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let token = generate_token();
    let (listener, credential) = session_registration(&token);
    rt.set_runtime_listener(listener, credential).unwrap();
    let value = serde_json::to_value(rt.settings()).unwrap();
    assert!(value.get("agents").is_none(), "{value}");
    assert!(value.get("listeners").is_none(), "{value}");
    let encoded = serde_json::to_string(&rt.settings()).unwrap();
    assert!(!encoded.contains(&token));
}
