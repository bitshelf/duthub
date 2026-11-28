//! MCP HTTP authentication, authorization, and lease end-to-end tests (N8; `docs/SECURITY.md`):
//! uniform 401 rejection, unauthorized DUT write rejection (`UNAUTHORIZED`), missing lease
//! rejection (`LeaseRequired/Missing`), expired lease rejection (`LeaseRequired/LEASE_EXPIRED`),
//! and valid lease admission (holder ownership + generation fencing).
//!
//! Constructed entirely in memory without disk writes: `AuthSvc` runs behind a real `axum`
//! `Router` on an ephemeral loopback port, while tool-layer tests invoke `#[tool]` methods
//! directly with `rmcp` extractors.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::{Json, Parameters};

use duthub_core::Principal;
use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings, DutAction, DutGrant};
use duthub_core::audit::AuditSink;
use duthub_core::lease::{ControlLeaseManager, LeaseHolder, WriteIdentity};

use duthub_mcp::auth_http::{AccessContext, CurrentPrincipal};
use duthub_mcp::server::McpServerCore;
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, ControlStatusDto, DutInfo, DutSummary,
    LeaseGrantDto, LogReadResult, McpState, ProcessResultDto, SerialBindInfo,
};

/// Recording test state source: write tools record the received write identity, and
/// the control-lease methods delegate to a real lease manager (matching `RuntimeMcpState`).
struct RecordingState {
    lease: Arc<ControlLeaseManager>,
    approval: duthub_core::ApprovalManager,
    writes: std::sync::Mutex<Vec<(String, WriteIdentity)>>,
    artifact_root: Option<std::path::PathBuf>,
    scoped_duts: bool,
}

impl RecordingState {
    fn new(lease: Arc<ControlLeaseManager>) -> Arc<Self> {
        Arc::new(Self {
            lease,
            approval: duthub_core::ApprovalManager::new(),
            writes: std::sync::Mutex::new(Vec::new()),
            artifact_root: None,
            scoped_duts: false,
        })
    }

    fn grant_of(&self, holder: &LeaseHolder, wait_ms: u64) -> duthub_core::lease::LeaseGrant {
        self.lease
            .acquire(
                "dut-1",
                holder,
                if wait_ms > 0 {
                    Some(Duration::from_millis(wait_ms))
                } else {
                    None
                },
            )
            .expect("acquire")
    }
}

fn map_grant(g: duthub_core::lease::LeaseGrant) -> LeaseGrantDto {
    LeaseGrantDto {
        lease_id: g.lease_id,
        dut_id: g.dut_id,
        generation: g.generation,
        expires_at_unix_ms: g.expires_at_unix_ms,
        holder: g.holder,
        session_name: g.session_name,
        acquired_at_unix_ms: g.acquired_at_unix_ms,
    }
}

impl McpState for RecordingState {
    fn list_duts(&self) -> Vec<DutSummary> {
        if !self.scoped_duts {
            return Vec::new();
        }
        vec![
            DutSummary {
                dut_id: "dut-a".into(),
                name: "Alpha".into(),
                source: "idle".into(),
                port: "COM30".into(),
                baud: 115_200,
            },
            DutSummary {
                dut_id: "dut-b".into(),
                name: "Beta".into(),
                source: "idle".into(),
                port: "COM31".into(),
                baud: 115_200,
            },
        ]
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        if self.scoped_duts {
            let (name, port) = match dut_id {
                "dut-a" => ("Alpha", "COM30"),
                "dut-b" => ("Beta", "COM31"),
                _ => return None,
            };
            return Some(DutInfo {
                dut_id: dut_id.to_owned(),
                name: name.into(),
                source: "idle".into(),
                serial: SerialBindInfo {
                    host: "local".into(),
                    port: port.into(),
                    baud: 115_200,
                    format: "8N1".into(),
                },
                adb_serial: None,
                relay: None,
                mcp_label: None,
                session_lines: None,
            });
        }
        if self.artifact_root.is_some() && dut_id == "dut-2" {
            return Some(DutInfo {
                dut_id: dut_id.to_owned(),
                name: "OtherDut".into(),
                source: "idle".into(),
                serial: SerialBindInfo::default(),
                adb_serial: None,
                relay: None,
                mcp_label: None,
                session_lines: None,
            });
        }
        None
    }

    fn serial_read(&self, _dut_id: &str, _since_seq: u64, _limit: usize) -> Option<LogReadResult> {
        None
    }

    fn dut_artifact_root(&self) -> Option<std::path::PathBuf> {
        self.artifact_root.clone()
    }

    fn serial_write(&self, dut_id: &str, _text: &str, writer: WriteIdentity) -> Result<(), String> {
        self.writes
            .lock()
            .unwrap()
            .push((dut_id.to_string(), writer));
        Ok(())
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
        Err("test double：未开放".into())
    }

    fn control_power(
        &self,
        _dut_id: &str,
        _on: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("test double：未开放".into())
    }

    fn control_reset(
        &self,
        _dut_id: &str,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("test double：未开放".into())
    }

    fn control_power_key(
        &self,
        _dut_id: &str,
        _long_press: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("test double：未开放".into())
    }

    fn dut_acquire_control(
        &self,
        dut_id: &str,
        holder: LeaseHolder,
        wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        let wait = if wait_ms > 0 {
            Some(Duration::from_millis(wait_ms))
        } else {
            None
        };
        self.lease
            .acquire(dut_id, &holder, wait)
            .map(map_grant)
            .map_err(|e| format!("LeaseRequired/DUT_BUSY: {}", e.message()))
    }

    fn dut_acquire_control_with_session(
        &self,
        dut_id: &str,
        holder: LeaseHolder,
        session_name: &str,
        wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        let wait = if wait_ms > 0 {
            Some(Duration::from_millis(wait_ms))
        } else {
            None
        };
        self.lease
            .acquire_with_session(dut_id, &holder, session_name, wait)
            .map(map_grant)
            .map_err(|e| format!("LeaseRequired/DUT_BUSY: {}", e.message()))
    }

    fn dut_renew_control(&self, dut_id: &str, lease_id: &str) -> Result<LeaseGrantDto, String> {
        self.lease
            .renew(dut_id, lease_id)
            .map(map_grant)
            .map_err(|e| format!("LeaseRequired/LEASE_EXPIRED: {}", e.message()))
    }

    fn dut_release_control(&self, dut_id: &str, lease_id: &str) -> Result<(), String> {
        self.lease
            .release(dut_id, lease_id)
            .map_err(|e| format!("LeaseRequired/DUT_PROTECTED: {}", e.message()))
    }

    fn dut_control_status(&self, dut_id: &str) -> Result<ControlStatusDto, String> {
        let s = self.lease.status(dut_id);
        let state = match s.phase {
            duthub_core::lease::LeasePhase::Free => "free",
            duthub_core::lease::LeasePhase::Held => "held",
            duthub_core::lease::LeasePhase::Protected => "protected",
            duthub_core::lease::LeasePhase::NeedsInspection => "needs_inspection",
        };
        Ok(ControlStatusDto {
            dut_id: s.dut_id,
            state: state.into(),
            holder: s.holder,
            session_name: s.session_name,
            acquired_at_unix_ms: s.acquired_at_unix_ms,
            occupied_secs: s.occupied_secs,
            remaining_secs: s.remaining_secs,
            generation: s.generation,
            queue_len: s.queue_len,
            protected_op: s.protected_op,
            protected_file: s.protected_file,
        })
    }

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("test double：未开放".into())
    }

    fn approval_issue(&self, dut_id: &str, action: &str, args_json: &str) -> ApprovalIssueResult {
        let t = self.approval.issue(dut_id, action, args_json);
        ApprovalIssueResult {
            token: t.token,
            dut_id: t.dut_id,
            action: t.action,
            expires_in_secs: t.expires_in_secs,
        }
    }

    fn approval_validate(
        &self,
        token: &str,
        dut_id: &str,
        action: &str,
        args_json: &str,
    ) -> Result<(), ApprovalErr> {
        self.approval
            .validate(token, dut_id, action, args_json)
            .map_err(|kind| ApprovalErr { kind })
    }
}

/// Runtime with auth enabled and `dut-1` `read`/`serialWrite` grants: `cred-a` (normal
/// flow) plus `cred-x` (same grants, non-holder, for ownership verification). Returns
/// `(ctx, cred-a token)`.
fn access_ctx(auth_enabled: bool, ttl: Duration) -> (AccessContext, String) {
    let token = duthub_core::agent_access::generate_token();
    let intruder_token = duthub_core::agent_access::generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: auth_enabled,
        agents: vec![
            duthub_core::AgentCredential {
                listener_id: None,
                id: "cred-a".into(),
                agent_name: "bot".into(),
                token: token.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-1".into(),
                    actions: vec![DutAction::Read, DutAction::SerialWrite],
                }],
            },
            duthub_core::AgentCredential {
                listener_id: None,
                id: "cred-x".into(),
                agent_name: "intruder".into(),
                token: intruder_token,
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-1".into(),
                    actions: vec![DutAction::Read, DutAction::SerialWrite],
                }],
            },
        ],
        ..Default::default()
    });
    let ctx = AccessContext::new(
        auth,
        Arc::new(ControlLeaseManager::new(Some(AuditSink::memory())).with_ttl(ttl)),
        AuditSink::memory(),
    );
    (ctx, token)
}

fn agent_principal() -> Principal {
    Principal::Agent {
        agent_name: "bot".into(),
        credential_id: "cred-a".into(),
    }
}

fn write_args(dut_id: &str, lease_id: Option<&str>) -> duthub_mcp::server::SerialWriteArgs {
    duthub_mcp::server::SerialWriteArgs {
        dut_id: dut_id.to_string(),
        text: "uname -a".into(),
        approval_token: None,
        lease_id: lease_id.map(str::to_string),
    }
}

#[test]
fn serial_write_retry_reuses_the_enqueue_receipt() {
    let (ctx, _) = access_ctx(true, Duration::from_secs(120));
    let state = RecordingState::new(ctx.lease.clone());
    let core = McpServerCore::new(state.clone(), Some(Arc::new(ctx)));
    let grant = state.grant_of(
        &LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred-a".into(),
        },
        0,
    );
    let send = |server: &McpServerCore, command: &str, lease| {
        server.serial_write_tool(
            Parameters(
                serde_json::from_value(serde_json::json!({
                    "dutId": "dut-1",
                    "text": command,
                    "leaseId": lease,
                    "requestId": "command-1"
                }))
                .unwrap(),
            ),
            CurrentPrincipal(agent_principal()),
        )
    };
    assert!(send(&core, "uname -a", None).is_err());
    let Json(first) = send(&core, "uname -a", Some(grant.lease_id.as_str())).unwrap();
    assert_eq!(first["status"], "queued");
    assert_eq!(first["replayed"], false);
    // Replaying without a valid lease is rejected so a revoked caller cannot probe receipts.
    assert!(send(&core.clone(), "uname -a", None).is_err());
    let Json(replay) = send(&core.clone(), "uname -a", Some(grant.lease_id.as_str())).unwrap();
    assert_eq!(replay["replayed"], true);
    let err = send(&core, "uptime", Some(grant.lease_id.as_str()))
        .err()
        .unwrap();
    assert_eq!(err.data.unwrap()["code"], "REQUEST_ID_CONFLICT");
    assert_eq!(state.writes.lock().unwrap().len(), 1);
}

/// Unauthorized DUT write rejection: with auth ON and grants limited to `dut-1`, writing
/// `dut-2` must return `UNAUTHORIZED` and must never reach the state source.
#[test]
fn unauthorized_dut_write_rejected() {
    let (ctx, _token) = access_ctx(true, Duration::from_secs(120));
    let lease = ctx.lease.clone();
    let state = RecordingState::new(lease);
    let core = McpServerCore::new(state.clone(), Some(Arc::new(ctx)));
    let err = core
        .serial_write(
            Parameters(write_args("dut-2", None)),
            CurrentPrincipal(agent_principal()),
        )
        .map(|_| ())
        .unwrap_err();
    assert!(
        err.message.starts_with("LeaseRequired/UNAUTHORIZED"),
        "{}",
        err.message
    );
    assert!(
        state.writes.lock().unwrap().is_empty(),
        "unauthorized write must not reach the state source"
    );
}

/// Missing lease write rejection: with auth ON and an authorized DUT, omitting `leaseId`
/// returns `LeaseRequired/Missing`.
#[test]
fn no_lease_write_rejected() {
    let (ctx, _token) = access_ctx(true, Duration::from_secs(120));
    let state = RecordingState::new(ctx.lease.clone());
    let core = McpServerCore::new(state.clone(), Some(Arc::new(ctx)));
    let err = core
        .serial_write(
            Parameters(write_args("dut-1", None)),
            CurrentPrincipal(agent_principal()),
        )
        .map(|_| ())
        .unwrap_err();
    assert!(
        err.message.starts_with("LeaseRequired/Missing"),
        "{}",
        err.message
    );
    assert!(state.writes.lock().unwrap().is_empty());
}

/// Expired lease write rejection: using an old handle after TTL expiry returns
/// `LeaseRequired/LEASE_EXPIRED` or `LeaseRequired/LEASE_UNKNOWN` (INV-04 chain).
#[test]
fn expired_lease_write_rejected() {
    let (ctx, _token) = access_ctx(true, Duration::from_millis(60));
    let state = RecordingState::new(ctx.lease.clone());
    let core = McpServerCore::new(state.clone(), Some(Arc::new(ctx)));
    let grant = state.grant_of(
        &LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred-a".into(),
        },
        0,
    );
    std::thread::sleep(Duration::from_millis(90));
    let err = core
        .serial_write(
            Parameters(write_args("dut-1", Some(&grant.lease_id))),
            CurrentPrincipal(agent_principal()),
        )
        .map(|_| ())
        .unwrap_err();
    // Lazy reclamation after TTL expiry removes the handle (unlike renew right at expiry,
    // which returns Expired directly, verification sees LEASE_UNKNOWN after lazy cleanup).
    assert!(
        err.message.starts_with("LeaseRequired/LEASE_UNKNOWN"),
        "{}",
        err.message
    );
    assert!(state.writes.lock().unwrap().is_empty());
}

/// Valid lease flow: acquire -> write (holder + generation check) -> release; another
/// authorized credential cannot use the same lease (`NOT_HOLDER`).
#[test]
fn valid_lease_flow_admits_and_fences() {
    let (ctx, _token) = access_ctx(true, Duration::from_secs(120));
    let state = RecordingState::new(ctx.lease.clone());
    let core = McpServerCore::new(state.clone(), Some(Arc::new(ctx)));
    let grant = state.grant_of(
        &LeaseHolder::Agent {
            agent_name: "bot".into(),
            credential_id: "cred-a".into(),
        },
        0,
    );
    core.serial_write(
        Parameters(write_args("dut-1", Some(&grant.lease_id))),
        CurrentPrincipal(agent_principal()),
    )
    .expect("valid lease write must succeed");
    let writes = state.writes.lock().unwrap();
    let (dut, identity) = writes.last().expect("write should be recorded");
    assert_eq!(dut, "dut-1");
    match identity {
        WriteIdentity::Agent {
            lease_id: Some(lease_id),
            generation: Some(expected_gen),
        } => {
            assert_eq!(lease_id, &grant.lease_id);
            assert_eq!(
                *expected_gen, grant.generation,
                "generation must come from holder verification"
            );
        }
        other => panic!("Expected a lease identity: {other:?}"),
    }
    drop(writes);
    // Another credential (authorized but not the lease holder) using the same handle -> NOT_HOLDER.
    let other = Principal::Agent {
        agent_name: "intruder".into(),
        credential_id: "cred-x".into(),
    };
    let err = core
        .serial_write(
            Parameters(write_args("dut-1", Some(&grant.lease_id))),
            CurrentPrincipal(other),
        )
        .map(|_| ())
        .unwrap_err();
    assert!(
        err.message.starts_with("LeaseRequired/NOT_HOLDER"),
        "{}",
        err.message
    );
}

/// Auth-off (default) compatibility path: no `leaseId` + valid `approvalToken` passes
/// a lease-less write identity to the state source (GUI priority checks remain active).
#[test]
fn auth_off_transition_write_passes_without_lease() {
    let (ctx, _token) = access_ctx(false, Duration::from_secs(120));
    let state = RecordingState::new(ctx.lease.clone());
    let core = McpServerCore::new(state.clone(), Some(Arc::new(ctx)));
    let token = state
        .approval
        .issue("dut-1", "serial.write", r#"{"text":"uname -a"}"#);
    let args = duthub_mcp::server::SerialWriteArgs {
        dut_id: "dut-1".into(),
        text: "uname -a".into(),
        approval_token: Some(token.token),
        lease_id: None,
    };
    core.serial_write(Parameters(args), CurrentPrincipal(Principal::LocalAdmin))
        .expect("auth off: LocalAdmin lease-less transition write should succeed");
    let writes = state.writes.lock().unwrap();
    assert!(matches!(
        writes.last().map(|(_, w)| w),
        Some(WriteIdentity::Agent {
            lease_id: None,
            generation: None
        })
    ));
}

/// HTTP 401 handling: with auth ON, missing or wrong Bearer tokens return a uniform 401
/// while a valid Bearer token passes through to `rmcp`.
#[tokio::test]
async fn http_bearer_401_and_pass_through() {
    let (ctx_on, good_token) = access_ctx(true, Duration::from_secs(120));
    let router = duthub_mcp::transport::http_router(
        RecordingState::new(ctx_on.lease.clone()),
        Some(Arc::new(ctx_on)),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // tokio::net::TcpListener::from_std requires non-blocking mode to avoid stalling the runtime.
    listener.set_nonblocking(true).unwrap();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        axum::serve(listener, router).await.unwrap();
    });
    let initialize_body = serde_json::json!({
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
    // Missing Bearer -> uniform 401 (rmcp is not reached).
    let status = post_json(addr, None, &initialize_body).await;
    assert_eq!(status, 401, "missing Bearer must return 401");
    // Wrong Bearer -> uniform 401 with fixed unauthorized body.
    let (status, body) = post_json_with_body(addr, Some("dth-wrong"), &initialize_body).await;
    assert_eq!(status, 401);
    assert!(body.contains("{\"error\":\"unauthorized\"}"), "{body}");
    // Valid Bearer -> passes through to rmcp initialize (non-401 confirms auth succeeded).
    let (status, _) = post_json_with_body(addr, Some(&good_token), &initialize_body).await;
    assert_ne!(status, 401, "valid Bearer must pass through (got {status})");
    assert!(
        status == 200 || status == 202,
        "initialize should be accepted by rmcp (got {status})"
    );
}

/// With remote access disabled, loopback HTTP must not fall back to local admin.
#[tokio::test]
async fn http_auth_off_rejects_without_bearer() {
    let (ctx, _token) = access_ctx(false, Duration::from_secs(120));
    let router = duthub_mcp::transport::http_router(
        RecordingState::new(ctx.lease.clone()),
        Some(Arc::new(ctx)),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // tokio::net::TcpListener::from_std requires non-blocking mode to avoid stalling the runtime.
    listener.set_nonblocking(true).unwrap();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        axum::serve(listener, router).await.unwrap();
    });
    let initialize_body = serde_json::json!({
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
    let (status, _) = post_json_with_body(addr, None, &initialize_body).await;
    assert_eq!(
        status, 401,
        "HTTP must reject requests when access is disabled"
    );
}

#[tokio::test]
async fn stateless_http_discovery_and_tools_use_json_and_require_bearer() {
    use serde_json::{Value, json};

    let _ = rustls::crypto::ring::default_provider().install_default();
    let (ctx, token) = access_ctx(true, Duration::from_secs(120));
    let state = RecordingState::new(ctx.lease.clone());
    let router = duthub_mcp::transport::http_router(state.clone(), Some(Arc::new(ctx)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let request = |method, params| {
        client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method)
            .body(json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}).to_string())
    };
    let metadata = || {
        json!({
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientInfo":{"name":"duthub-e2e","version":"1"},
            "io.modelcontextprotocol/clientCapabilities":{}
        })
    };
    let list = || request("tools/list", json!({"_meta":metadata()}));
    assert_eq!(list().send().await.unwrap().status(), 401);

    let discover = request("server/discover", json!({"_meta":metadata()}))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let status = discover.status();
    assert!(!discover.headers().contains_key("Mcp-Session-Id"));
    let body = discover.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let discovered: Value = serde_json::from_str(&body).unwrap();
    assert!(
        discovered["result"]["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28"))
    );

    let listing = list().bearer_auth(&token).send().await.unwrap();
    assert_eq!(listing.status(), 200);
    assert!(!listing.headers().contains_key("Mcp-Session-Id"));
    assert_eq!(listing.headers()["content-type"], "application/json");
    let tools: Value = serde_json::from_str(&listing.text().await.unwrap()).unwrap();
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "serial_read"));
    assert!(!tools.iter().any(|tool| tool["name"] == "serial_write"));

    let denied = request(
        "tools/call",
        json!({
            "_meta": metadata(),
            "name": "serial_write",
            "arguments": {"dutId":"dut-1", "text":"forbidden"}
        }),
    )
    .header("Mcp-Name", "serial_write")
    .bearer_auth(&token)
    .send()
    .await
    .unwrap();
    assert!(denied.text().await.unwrap().contains("PLAN_MODE_ONLY"));
    assert!(state.writes.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn valid_bearer_reads_only_its_dut_and_cannot_dispatch_writes() {
    use std::io::Write;

    use rmcp::model::{CallToolRequestParams, ClientConfig};
    use rmcp::service::serve_client;
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };

    let _ = rustls::crypto::ring::default_provider().install_default();
    let (ctx, token) = access_ctx(true, Duration::from_secs(120));
    let root = tempfile::tempdir().unwrap();
    let mut state = RecordingState::new(ctx.lease.clone());
    Arc::get_mut(&mut state).unwrap().artifact_root = Some(root.path().to_owned());
    duthub_core::dut_artifacts::create_file(root.path(), "dut-2", "OtherDut", "trace.txt")
        .unwrap()
        .0
        .write_all(b"cross-dut evidence")
        .unwrap();
    let router = duthub_mcp::transport::http_router(state.clone(), Some(Arc::new(ctx)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token),
    );
    let client = serve_client(ClientConfig::default(), transport)
        .await
        .unwrap();
    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|tool| tool.name == "serial_read"));
    let tool_args = serde_json::json!({"dutId":"dut-1","text":"should never dispatch"});
    for tool in [
        "serial_write",
        "approval_request",
        "heartbeat_set",
        "control_power",
        "relay_test",
        "adb_shell",
        "flash_test",
        "log_mark",
        "run_shell",
    ] {
        assert!(!tools.iter().any(|entry| entry.name == tool), "{tool}");
        let call = CallToolRequestParams::new(tool.to_owned())
            .with_arguments(tool_args.as_object().unwrap().clone());
        let error = client.call_tool_once(call).await.unwrap_err();
        assert!(error.to_string().contains("PLAN_MODE_ONLY"), "{error}");
    }
    let granted = CallToolRequestParams::new("dut_list");
    assert!(client.call_tool_once(granted).await.is_ok());
    let other_dut_file = |name: &str| {
        CallToolRequestParams::new("dut_file_read").with_arguments(
            serde_json::json!({"dutId":"dut-2","fileId":name})
                .as_object()
                .unwrap()
                .clone(),
        )
    };
    // Generic listener enforces per-DUT `DutAction::Read` grants: reading an
    // ungranted DUT (`dut-2`) is rejected with `UNAUTHORIZED`.
    let cross_err = client
        .call_tool_once(other_dut_file("trace.txt"))
        .await
        .unwrap_err();
    assert!(
        cross_err.to_string().contains("UNAUTHORIZED"),
        "{cross_err}"
    );
    assert!(
        client
            .call_tool_once(other_dut_file("../settings.json"))
            .await
            .is_err()
    );
    let unrelated = CallToolRequestParams::new("serial_read").with_arguments(
        serde_json::json!({"dutId":"unrelated"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let error = client.call_tool_once(unrelated).await.unwrap_err();
    assert!(error.to_string().contains("UNAUTHORIZED"), "{error}");
    assert!(state.writes.lock().unwrap().is_empty());
    client.cancel().await.unwrap();
    task.abort();
}

#[tokio::test]
async fn credentials_are_isolated_across_loopback_listeners() {
    let (ctx, primary_token) = access_ctx(true, Duration::from_secs(120));
    let mut settings = ctx.auth.settings();
    settings.agents[1].listener_id = Some("lab".into());
    let lab_token = settings.agents[1].token.clone();
    let primary = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lab = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_address = primary.local_addr().unwrap();
    let lab_address = lab.local_addr().unwrap();
    settings
        .listeners
        .push(duthub_core::agent_access::AgentListener {
            id: "lab".into(),
            port: lab_address.port(),
            dut_id: None,
        });
    ctx.auth.update(settings);
    let state = RecordingState::new(ctx.lease.clone());
    let ctx = Arc::new(ctx);
    for (listener, id) in [(primary, None), (lab, Some(Arc::from("lab")))] {
        let router = duthub_mcp::transport::http_router_on(state.clone(), Some(ctx.clone()), id);
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    }
    let initialize = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "probe", "version": "0"}
        }
    })
    .to_string();
    assert_eq!(
        post_json(primary_address, Some(&primary_token), &initialize).await,
        200
    );
    assert_eq!(
        post_json(lab_address, Some(&lab_token), &initialize).await,
        200
    );
    assert_eq!(
        post_json(primary_address, Some(&lab_token), &initialize).await,
        401
    );
    assert_eq!(
        post_json(lab_address, Some(&primary_token), &initialize).await,
        401
    );
    let mut settings = ctx.auth.settings();
    settings.agents[1].enabled = false;
    ctx.auth.update(settings);
    assert_eq!(
        post_json(lab_address, Some(&lab_token), &initialize).await,
        401
    );
    assert_eq!(
        post_json(primary_address, Some(&primary_token), &initialize).await,
        200
    );
}

#[tokio::test]
async fn stdio_bridge_uses_the_upstream_bearer() {
    use rmcp::model::ClientConfig;
    use rmcp::service::serve_client;
    use rmcp::transport::child_process::TokioChildProcess;

    let (ctx, token) = access_ctx(true, Duration::from_secs(120));
    let router = duthub_mcp::transport::http_router(
        RecordingState::new(ctx.lease.clone()),
        Some(Arc::new(ctx)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let endpoint = format!("http://{address}/mcp");
    let connect = |token: String| {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_duthub-mcp"));
        command
            .args(["--bridge", &endpoint])
            .env("DUTHUB_MCP_TOKEN", token);
        let child = TokioChildProcess::new(command).unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            serve_client(ClientConfig::default(), child),
        )
    };
    let denied = connect("invalid-token".into())
        .await
        .expect("bridge must stop on HTTP denial");
    assert!(denied.is_err());
    let client = connect(token)
        .await
        .unwrap()
        .expect("valid bearer initializes through stdio");
    let tools = tokio::time::timeout(Duration::from_secs(10), client.list_all_tools())
        .await
        .unwrap()
        .unwrap();
    assert!(tools.iter().any(|tool| tool.name == "serial_read"));
    assert!(!tools.iter().any(|tool| tool.name == "dut_acquire_control"));
    tokio::time::timeout(Duration::from_secs(10), client.cancel())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn loopback_listener_rejects_untrusted_browser_origins() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (ctx, token) = access_ctx(true, Duration::from_secs(120));
    let router = duthub_mcp::transport::http_router(
        RecordingState::new(ctx.lease.clone()),
        Some(Arc::new(ctx)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}/mcp"))
        .bearer_auth(token)
        .header("Origin", "https://example.invalid")
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[test]
fn shared_token_cannot_reacquire_an_active_lease() {
    let (ctx, _) = access_ctx(true, Duration::from_secs(120));
    let holder = LeaseHolder::Agent {
        agent_name: "bot".into(),
        credential_id: "cred-a".into(),
    };
    ctx.lease.set_agent_access(ctx.auth.clone());
    let original = ctx.lease.acquire("dut-1", &holder, None).unwrap();
    assert!(matches!(
        ctx.lease.acquire("dut-1", &holder, None),
        Err(duthub_core::lease::LeaseError::Busy { .. })
    ));
    assert!(ctx.lease.acquire("dut-2", &holder, None).is_ok());
    let in_flight = ctx.lease.begin_agent_action().unwrap();
    let mut settings = ctx.auth.settings();
    settings.agents[0].enabled = false;
    ctx.auth.update(settings);
    ctx.lease.revoke_credential("cred-a");
    assert!(
        ctx.lease
            .validate("dut-1", &original.lease_id, original.generation)
            .is_err()
    );
    assert_eq!(
        ctx.lease.acquire("dut-1", &holder, None),
        Err(duthub_core::lease::LeaseError::Unauthorized)
    );
    let other = LeaseHolder::Agent {
        agent_name: "intruder".into(),
        credential_id: "cred-x".into(),
    };
    assert!(matches!(
        ctx.lease.acquire("dut-1", &other, None),
        Err(duthub_core::lease::LeaseError::Busy { .. })
    ));
    drop(in_flight);
    assert!(ctx.lease.acquire("dut-1", &other, None).is_ok());
}

/// Minimal HTTP/1.1 JSON-RPC client (`std::net::TcpStream` + `tokio`), returning the
/// status code (and optional body).
async fn post_json(addr: std::net::SocketAddr, auth: Option<&str>, body: &str) -> u16 {
    post_json_with_body(addr, auth, body).await.0
}

async fn post_json_with_body(
    addr: std::net::SocketAddr,
    auth: Option<&str>,
    body: &str,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream =
        tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
            .await
            .expect("connect timeout")
            .unwrap();
    let auth_line = match auth {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{auth_line}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    // Timed read: the status line is sufficient (rmcp session endpoints may keep the stream open).
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    let raw = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    (status, body)
}

#[test]
fn json_helper_import_silence() {
    // Reference the Json wrapper to silence unused-import warnings.
    let _ = Json(serde_json::json!({}));
}

#[tokio::test]
async fn dut_bound_listener_http_scope_and_default_dut() {
    use rmcp::model::{CallToolRequestParams, ClientConfig};
    use rmcp::service::serve_client;
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };
    use std::io::Write;

    let _ = rustls::crypto::ring::default_provider().install_default();
    let token = duthub_core::agent_access::generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![duthub_core::AgentCredential {
            listener_id: Some("dut-a-mcp".into()),
            id: "cred-a".into(),
            agent_name: "bot".into(),
            token: token.clone(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-a".into(),
                actions: vec![DutAction::Read],
            }],
        }],
        listeners: vec![duthub_core::agent_access::AgentListener {
            id: "dut-a-mcp".into(),
            port: 0,
            dut_id: Some("dut-a".into()),
        }],
        ..Default::default()
    });
    let ctx = AccessContext::new(
        auth,
        Arc::new(
            ControlLeaseManager::new(Some(AuditSink::memory())).with_ttl(Duration::from_secs(120)),
        ),
        AuditSink::memory(),
    );
    let root = tempfile::tempdir().unwrap();
    let mut state = RecordingState::new(ctx.lease.clone());
    {
        let state = Arc::get_mut(&mut state).unwrap();
        state.artifact_root = Some(root.path().to_owned());
        state.scoped_duts = true;
    }
    duthub_core::dut_artifacts::create_file(root.path(), "dut-a", "Alpha", "a.txt")
        .unwrap()
        .0
        .write_all(b"alpha")
        .unwrap();
    duthub_core::dut_artifacts::create_file(root.path(), "dut-b", "Beta", "b.txt")
        .unwrap()
        .0
        .write_all(b"beta")
        .unwrap();

    let router = duthub_mcp::transport::http_router_on_dut(
        state,
        Some(Arc::new(ctx)),
        Some(Arc::<str>::from("dut-a-mcp")),
        Some(Arc::<str>::from("dut-a")),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token),
    );
    let client = serve_client(ClientConfig::default(), transport)
        .await
        .unwrap();

    let list = client
        .call_tool_once(CallToolRequestParams::new("dut_list"))
        .await
        .unwrap();
    let rmcp::model::CallToolResponse::Complete(list) = list else {
        panic!("expected tool data")
    };
    let data = list.structured_content.expect("typed data");
    assert_eq!(data["duts"].as_array().unwrap().len(), 1);
    assert_eq!(data["duts"][0]["dut_id"], "dut-a");

    let call = |name: &str, args: serde_json::Value| {
        CallToolRequestParams::new(name.to_owned())
            .with_arguments(args.as_object().unwrap().clone())
    };
    for (name, args) in [
        ("dut_status", serde_json::json!({"dutId": "dut-b"})),
        ("log_segments", serde_json::json!({"dutId": "dut-b"})),
        ("serial_read", serde_json::json!({"dutId": "dut-b"})),
        (
            "dut_file_read",
            serde_json::json!({"dutId": "dut-b", "fileId": "b.txt"}),
        ),
    ] {
        let error = client.call_tool_once(call(name, args)).await.unwrap_err();
        assert!(
            error.to_string().contains("UNAUTHORIZED"),
            "{name}: {error}"
        );
    }

    let files = client
        .call_tool_once(CallToolRequestParams::new("dut_files"))
        .await
        .unwrap();
    let rmcp::model::CallToolResponse::Complete(files) = files else {
        panic!("expected tool data")
    };
    let data = files.structured_content.expect("typed data");
    let names: Vec<&str> = data["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|file| file["id"].as_str())
        .collect();
    assert!(names.contains(&"a.txt"), "{names:?}");
    assert!(!names.contains(&"b.txt"), "{names:?}");

    client.cancel().await.unwrap();
    task.abort();
}

/// `skill_export` authorization (W1 / T17): an HTTP credential with zero grants is
/// rejected, a credential with any DUT read grant is allowed, and stdio `LocalAdmin`
/// access is covered by `server` unit tests.
#[tokio::test]
async fn skill_export_requires_a_read_grant_over_http() {
    use rmcp::model::{CallToolRequestParams, ClientConfig};
    use rmcp::service::serve_client;
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };

    let _ = rustls::crypto::ring::default_provider().install_default();
    let denied_token = duthub_core::agent_access::generate_token();
    let allowed_token = duthub_core::agent_access::generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![
            duthub_core::AgentCredential {
                listener_id: None,
                id: "cred-no-grant".into(),
                agent_name: "no-grant".into(),
                token: denied_token.clone(),
                enabled: true,
                grants: Vec::new(),
            },
            duthub_core::AgentCredential {
                listener_id: None,
                id: "cred-read".into(),
                agent_name: "reader".into(),
                token: allowed_token.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-1".into(),
                    actions: vec![DutAction::Read],
                }],
            },
        ],
        ..Default::default()
    });
    let ctx = AccessContext::new(
        auth,
        Arc::new(ControlLeaseManager::new(Some(AuditSink::memory()))),
        AuditSink::memory(),
    );
    let router = duthub_mcp::transport::http_router(
        RecordingState::new(ctx.lease.clone()),
        Some(Arc::new(ctx)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let connect = |token: String| {
        StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            StreamableHttpClientTransportConfig::with_uri(endpoint.clone()).auth_header(token),
        )
    };
    let denied = serve_client(ClientConfig::default(), connect(denied_token))
        .await
        .unwrap();
    let error = denied
        .call_tool_once(CallToolRequestParams::new("skill_export"))
        .await
        .expect_err("zero-grant credential must be rejected for skill_export");
    assert!(error.to_string().contains("UNAUTHORIZED"), "{error}");
    denied.cancel().await.ok();

    let allowed = serve_client(ClientConfig::default(), connect(allowed_token))
        .await
        .unwrap();
    let response = allowed
        .call_tool_once(CallToolRequestParams::new("skill_export"))
        .await
        .expect("read grant can read SKILL.md");
    let rmcp::model::CallToolResponse::Complete(result) = response else {
        panic!("expected complete result");
    };
    let data = result.structured_content.expect("structured content");
    assert_eq!(data["ok"], serde_json::json!(true));
    let markdown = data["markdown"].as_str().expect("markdown");
    assert!(markdown.contains("SKILL"), "{markdown}");
    allowed.cancel().await.ok();

    task.abort();
}

async fn post_json_with_headers(
    addr: std::net::SocketAddr,
    host: &str,
    auth: Option<&str>,
    origin: Option<&str>,
    body: &str,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream =
        tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
            .await
            .expect("connect timeout")
            .unwrap();
    let auth_line = match auth {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    let origin_line = match origin {
        Some(o) => format!("Origin: {o}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\n{auth_line}{origin_line}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    let raw = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    (status, body)
}

#[tokio::test]
async fn lan_wildcard_listener_allows_lan_and_configured_hosts_and_blocks_foreign_host_or_origin() {
    let token = duthub_core::agent_access::generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        lan_access: true,
        allowed_hosts: vec!["192.168.1.175".into(), "lab-box.internal:33612".into()],
        agents: vec![duthub_core::AgentCredential {
            listener_id: None,
            id: "cred-lan".into(),
            agent_name: "lan-bot".into(),
            token: token.clone(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-a".into(),
                actions: vec![DutAction::Read],
            }],
        }],
        ..Default::default()
    });
    let ctx = Arc::new(AccessContext::new(
        auth,
        Arc::new(ControlLeaseManager::new(Some(AuditSink::memory()))),
        AuditSink::memory(),
    ));
    let std_listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = std_listener.local_addr().unwrap().port();
    let handle = duthub_mcp::transport::spawn_http_listener_on(
        std_listener,
        RecordingState::new(ctx.lease.clone()),
        Some(ctx),
        None,
        None,
        None,
    )
    .expect("spawn wildcard LAN HTTP listener");
    let connect_addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"lan-test","version":"1"}}}"#;

    // 1) Advertised LAN IP host header succeeds with valid Bearer.
    let adv_host = handle.endpoint.to_string();
    let (status, body) =
        post_json_with_headers(connect_addr, &adv_host, Some(&token), None, init_body).await;
    assert_eq!(
        status, 200,
        "advertised LAN host must pass: {adv_host} -> {body}"
    );

    // 2) Configured LAN IP & host:port entries succeed.
    let (status, body) = post_json_with_headers(
        connect_addr,
        &format!("192.168.1.175:{port}"),
        Some(&token),
        None,
        init_body,
    )
    .await;
    assert_eq!(status, 200, "configured LAN IP must pass: {body}");
    let (status, body) = post_json_with_headers(
        connect_addr,
        "lab-box.internal:33612",
        Some(&token),
        None,
        init_body,
    )
    .await;
    assert_eq!(status, 200, "configured host:port must pass: {body}");

    // 3) Loopback Host still succeeds.
    let (status, body) = post_json_with_headers(
        connect_addr,
        &format!("127.0.0.1:{port}"),
        Some(&token),
        None,
        init_body,
    )
    .await;
    assert_eq!(status, 200, "loopback Host must pass: {body}");

    // 4) Unlisted Host and browser Origin remain rejected with 403; missing Bearer returns 401.
    let (status, body) = post_json_with_headers(
        connect_addr,
        &format!("evil.example:{port}"),
        Some(&token),
        None,
        init_body,
    )
    .await;
    assert_eq!(status, 403, "foreign Host must be rejected: {body}");

    let (status, body) = post_json_with_headers(
        connect_addr,
        &adv_host,
        Some(&token),
        Some("http://evil.example"),
        init_body,
    )
    .await;
    assert_eq!(status, 403, "browser Origin must be rejected: {body}");

    let (status, _) = post_json_with_headers(connect_addr, &adv_host, None, None, init_body).await;
    assert_eq!(
        status, 401,
        "missing Bearer must return 401 before Host check"
    );

    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(3)));
}
