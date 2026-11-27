//! `ListenerHandle` lifecycle regression (W1 / T07):
//! usable after spawn -> shutdown + join releases the port and thread; `Drop` sends
//! the shutdown signal when not shut down explicitly; revoking the runtime overlay
//! immediately returns 401 on the same listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duthub_core::agent_access::{
    AgentAccessSettings, AgentCredential, AgentListener, DutAction, DutGrant, generate_token,
};
use duthub_core::audit::AuditSink;
use duthub_core::lease::ControlLeaseManager;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::snapshot::SnapshotState;
use duthub_mcp::transport::{ListenerHandle, spawn_http_listener_loopback};

fn session_access(listener_id: &str, credential_id: &str, token: &str) -> AccessContext {
    let auth = duthub_core::AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    auth.set_runtime_listener(
        AgentListener {
            id: listener_id.into(),
            port: 0,
            dut_id: Some("dut-a".into()),
        },
        AgentCredential {
            id: credential_id.into(),
            listener_id: Some(listener_id.into()),
            agent_name: format!("agent-{credential_id}"),
            token: token.into(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-a".into(),
                actions: vec![DutAction::Read],
            }],
        },
    )
    .unwrap();
    AccessContext::new(
        auth,
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    )
}

fn spawn(access: AccessContext, listener_id: &str) -> ListenerHandle {
    spawn_http_listener_loopback(
        Arc::new(SnapshotState::empty()),
        Some(Arc::new(access)),
        Some(Arc::from(listener_id)),
        Some(Arc::from("dut-a")),
    )
    .expect("spawn loopback listener")
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
            "clientInfo": {"name": "probe", "version": "0"}
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
    let auth_line = match bearer {
        Some(token) => format!("Authorization: Bearer {token}\r\n"),
        None => String::new(),
    };
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

/// Race between server thread startup and accept: retry until a status code arrives before the deadline.
async fn initialize_status(endpoint: SocketAddr, bearer: Option<&str>) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = post_initialize(endpoint, bearer).await {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "listener did not begin accepting connections within 5s: {endpoint}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The port must be re-bindable after shutdown/join completes (thread and port back to baseline).
fn assert_port_released(endpoint: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(listener) = std::net::TcpListener::bind(endpoint) {
            drop(listener);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "port was not released within 5s after shutdown: {endpoint}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn spawned_listener_serves_shuts_down_and_releases_the_port() {
    let token = generate_token();
    let handle = spawn(
        session_access("session-a", "cred-session", &token),
        "session-a",
    );
    let endpoint = handle.endpoint;
    assert!(endpoint.ip().is_loopback(), "{endpoint}");

    // Missing or wrong Bearer -> 401; session token -> passes through to rmcp.
    assert_eq!(initialize_status(endpoint, None).await, 401);
    assert_eq!(initialize_status(endpoint, Some("dth-wrong")).await, 401);
    let status = initialize_status(endpoint, Some(&token)).await;
    assert!(
        status == 200 || status == 202,
        "session token must pass internal authentication (got {status})"
    );

    assert!(
        handle.shutdown_and_join_with_timeout(Duration::from_secs(5)),
        "server thread must join after shutdown"
    );
    assert_port_released(endpoint);
}

/// Even without an explicit shutdown call, `Drop` must signal shutdown and release the port.
#[tokio::test]
async fn dropping_the_handle_stops_the_listener() {
    let token = generate_token();
    let handle = spawn(
        session_access("session-a", "cred-session", &token),
        "session-a",
    );
    let endpoint = handle.endpoint;
    assert_ne!(initialize_status(endpoint, Some(&token)).await, 401);
    drop(handle);
    assert_port_released(endpoint);
}

/// Revoking a runtime listener (removing it from the overlay) immediately returns 401 for its token.
#[tokio::test]
async fn revoked_runtime_listener_is_rejected_immediately() {
    let token = generate_token();
    let access = session_access("session-a", "cred-session", &token);
    let auth = access.auth.clone();
    let handle = spawn(access, "session-a");
    let endpoint = handle.endpoint;
    assert_ne!(initialize_status(endpoint, Some(&token)).await, 401);

    auth.remove_runtime_listener("session-a").unwrap();
    assert_eq!(
        initialize_status(endpoint, Some(&token)).await,
        401,
        "token must be rejected immediately after revocation"
    );

    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
    assert_port_released(endpoint);
}

/// Multiple listeners shut down independently: stopping one does not affect the other, and both ports return to baseline.
#[tokio::test]
async fn multiple_listeners_shut_down_independently() {
    let first_token = generate_token();
    let second_token = generate_token();

    let first_handle = spawn(
        session_access("session-a", "cred-session-a", &first_token),
        "session-a",
    );
    let second_handle = spawn(
        session_access("session-b", "cred-session-b", &second_token),
        "session-b",
    );
    let first_endpoint = first_handle.endpoint;
    let second_endpoint = second_handle.endpoint;

    assert_ne!(
        initialize_status(first_endpoint, Some(&first_token)).await,
        401
    );
    assert_ne!(
        initialize_status(second_endpoint, Some(&second_token)).await,
        401
    );
    assert!(first_handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
    // The first listener is shut down; the second remains available.
    assert_ne!(
        initialize_status(second_endpoint, Some(&second_token)).await,
        401
    );
    assert!(second_handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
    assert_port_released(first_endpoint);
    assert_port_released(second_endpoint);
}
