//! Cross-DUT regression for DUT-dedicated listeners (W1 / T19):
//! the 10 Plan tools under `listener_dut = Some("dut-a")` only allow `dut-a`;
//! `dut_list` and `adb_list_devices` only return the bound DUT; `dut_files` defaults
//! to the bound DUT when `dutId` is omitted; `skill_export` never leaks other DUTs.
//!
//! Exercises the real HTTP route (`AuthSvc` + `rmcp` client) rather than calling
//! private tool methods directly.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResponse, ClientConfig};
use rmcp::service::RunningService;
use rmcp::service::serve_client;
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
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, ControlStatusDto, DutInfo, DutSummary,
    HeartbeatViewDto, LogLineDto, LogReadResult, McpState, ProcessResultDto, SerialBindInfo,
};
use duthub_mcp::transport::http_router_on_dut;

/// 10 streamlined Plan tools (aligned with `server::PLAN_TOOL_NAMES`).
const PLAN_TOOLS: [&str; 10] = [
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

struct FixtureState {
    artifact_root: PathBuf,
    log_dir: PathBuf,
}

fn summary(dut_id: &str, name: &str, port: &str) -> DutSummary {
    DutSummary {
        dut_id: dut_id.into(),
        name: name.into(),
        source: "idle".into(),
        port: port.into(),
        baud: 115_200,
    }
}

impl McpState for FixtureState {
    fn list_duts(&self) -> Vec<DutSummary> {
        vec![
            summary("dut-a", "Alpha", "COM30"),
            summary("dut-b", "Beta", "COM31"),
        ]
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        let (name, port, adb) = match dut_id {
            "dut-a" => ("Alpha", "COM30", "serial-a"),
            "dut-b" => ("Beta", "COM31", "serial-b"),
            _ => return None,
        };
        Some(DutInfo {
            dut_id: dut_id.into(),
            name: name.into(),
            source: "idle".into(),
            serial: SerialBindInfo {
                host: "local".into(),
                port: port.into(),
                baud: 115_200,
                format: "8N1".into(),
            },
            adb_serial: Some(adb.into()),
            relay: None,
            mcp_label: None,
            session_lines: Some(2),
        })
    }

    fn serial_read(&self, dut_id: &str, since_seq: u64, limit: usize) -> Option<LogReadResult> {
        let lines = match dut_id {
            "dut-a" => vec![
                LogLineDto {
                    seq: 1,
                    text: "alpha ready".into(),
                },
                LogLineDto {
                    seq: 2,
                    text: "alpha boot".into(),
                },
            ],
            "dut-b" => vec![LogLineDto {
                seq: 1,
                text: "beta secret".into(),
            }],
            _ => return None,
        };
        let lines: Vec<_> = lines
            .into_iter()
            .filter(|line| line.seq > since_seq)
            .take(limit)
            .collect();
        Some(LogReadResult {
            dut_id: dut_id.into(),
            session_id: Some(format!("sess-{dut_id}")),
            last_seq: lines.last().map_or(since_seq, |line| line.seq),
            lines,
        })
    }

    fn log_archive_dir(&self) -> Option<PathBuf> {
        Some(self.log_dir.clone())
    }

    fn dut_artifact_root(&self) -> Option<PathBuf> {
        Some(self.artifact_root.clone())
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
        vec![
            AdbDeviceDto {
                serial: "serial-a".into(),
                state: "device".into(),
                model: Some("Alpha".into()),
            },
            AdbDeviceDto {
                serial: "serial-b".into(),
                state: "device".into(),
                model: Some("Beta".into()),
            },
        ]
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

    fn heartbeat_get(&self, dut_id: &str) -> Result<HeartbeatViewDto, String> {
        Ok(HeartbeatViewDto::new(
            dut_id,
            duthub_core::heartbeat::HeartbeatConfig::default(),
            duthub_core::heartbeat::HeartbeatState::default(),
        ))
    }

    fn dut_control_status(&self, dut_id: &str) -> Result<ControlStatusDto, String> {
        Ok(ControlStatusDto {
            dut_id: dut_id.into(),
            state: "free".into(),
            holder: None,
            session_name: None,
            acquired_at_unix_ms: None,
            occupied_secs: None,
            remaining_secs: None,
            generation: 0,
            queue_len: 0,
            protected_op: None,
        })
    }

    fn relay_status(&self, _dut_id: &str) -> Result<Vec<(u8, bool)>, String> {
        Ok(vec![(1, false)])
    }
}

fn fixture() -> FixtureState {
    let root = tempfile::tempdir().unwrap().keep();
    for (dut_id, name, file, contents) in [
        ("dut-a", "Alpha", "a.txt", &b"alpha"[..]),
        ("dut-b", "Beta", "b.txt", &b"beta"[..]),
    ] {
        duthub_core::dut_artifacts::create_file(&root, dut_id, name, file)
            .unwrap()
            .0
            .write_all(contents)
            .unwrap();
    }
    let log_dir = root.join("log-root");
    let folder = log_dir.join("logs").join("Alpha");
    std::fs::create_dir_all(&folder).unwrap();
    let filename = format!(
        "{}0001_20260101_000000.log",
        duthub_core::log_archive::archive_prefix("dut-a")
    );
    std::fs::write(folder.join(filename), "[mark] boot\nalpha log\n").unwrap();
    FixtureState {
        artifact_root: root,
        log_dir,
    }
}

async fn connect(
    listener_id: &str,
    dut_id: &str,
    token: &str,
) -> (
    RunningService<rmcp::RoleClient, ClientConfig>,
    tokio::task::JoinHandle<()>,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let access = AccessContext::new(
        duthub_core::AgentAccessRuntime::from_settings(AgentAccessSettings {
            enabled: true,
            agents: vec![AgentCredential {
                id: "cred-a".into(),
                listener_id: Some(listener_id.into()),
                agent_name: "bound-agent".into(),
                token: token.into(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: dut_id.into(),
                    actions: vec![DutAction::Read],
                }],
            }],
            listeners: vec![AgentListener {
                id: listener_id.into(),
                port: 0,
                dut_id: Some(dut_id.into()),
            }],
            ..Default::default()
        }),
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    );
    let router = http_router_on_dut(
        Arc::new(fixture()),
        Some(Arc::new(access)),
        Some(Arc::from(listener_id)),
        Some(Arc::from(dut_id)),
    );
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

#[tokio::test]
async fn dut_bound_listener_scopes_all_plan_tools_to_its_dut() {
    let token = duthub_core::agent_access::generate_token();
    let (client, task) = connect("dut-a-mcp", "dut-a", &token).await;

    // All 10 Plan tools are visible.
    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    for expected in PLAN_TOOLS {
        assert!(
            names.contains(&expected),
            "missing Plan tool {expected}: {names:?}"
        );
    }
    assert!(
        !names.contains(&"relay_status"),
        "relay_status must not be on the Plan route (read-only but hidden in plan mode)"
    );

    // Cross-DUT reads must all return LeaseRequired/UNAUTHORIZED.
    let cross_dut_calls = [
        ("dut_status", json!({"dutId": "dut-b"})),
        ("serial_read", json!({"dutId": "dut-b"})),
        ("log_segments", json!({"dutId": "dut-b"})),
        ("log_export", json!({"dutId": "dut-b"})),
        (
            "log_segment_read",
            json!({"dutId": "dut-b", "segmentId": "anything"}),
        ),
        ("dut_files", json!({"dutId": "dut-b"})),
        (
            "dut_file_read",
            json!({"dutId": "dut-b", "fileId": "b.txt"}),
        ),
        ("media_streams", json!({"dutId": "dut-b"})),
    ];
    for (tool, arguments) in cross_dut_calls {
        let error = call(&client, tool, arguments)
            .await
            .expect_err(&format!("{tool} must reject cross-DUT access"));
        assert!(
            error.contains("UNAUTHORIZED"),
            "{tool} cross-DUT rejection message: {error}"
        );
    }

    // Bound DUT tools succeed.
    let data = call(&client, "dut_status", json!({"dutId": "dut-a"}))
        .await
        .expect("dut_status dut-a");
    assert_eq!(data["dut_id"], "dut-a");
    assert_eq!(data["control"]["state"], "free");
    assert_eq!(data["heartbeat"]["dut_id"], "dut-a");
    let data = call(&client, "serial_read", json!({"dutId": "dut-a"}))
        .await
        .expect("serial_read dut-a");
    assert_eq!(data["lines"].as_array().unwrap().len(), 2);
    assert_eq!(data["lines"][0]["text"], "alpha ready");
    let data = call(&client, "serial_read", json!({"dutId": "dut-a", "tail": 1}))
        .await
        .expect("serial_read tail dut-a");
    assert_eq!(data["lines"].as_array().unwrap().len(), 1);
    let data = call(&client, "media_streams", json!({"dutId": "dut-a"}))
        .await
        .expect("media_streams dut-a");
    assert_eq!(data["streams"], json!({}));

    // log_segments / log_segment_read: readable for the bound DUT.
    let data = call(&client, "log_segments", json!({"dutId": "dut-a"}))
        .await
        .expect("log_segments dut-a");
    let segments = data["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 1, "{data}");
    let segment_id = segments[0]["id"].as_str().unwrap().to_string();
    let data = call(
        &client,
        "log_segment_read",
        json!({"dutId": "dut-a", "segmentId": segment_id}),
    )
    .await
    .expect("log_segment_read dut-a");
    assert_eq!(data["text"], "[mark] boot\nalpha log\n");

    // dut_files: omitting dutId defaults to the bound DUT and lists only dut-a files.
    let data = call(&client, "dut_files", json!({}))
        .await
        .expect("dut_files default bound dut");
    let files: Vec<&str> = data["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|file| file["id"].as_str())
        .collect();
    assert_eq!(files, ["a.txt"], "{data}");
    let data = call(
        &client,
        "dut_file_read",
        json!({"dutId": "dut-a", "fileId": "a.txt"}),
    )
    .await
    .expect("dut_file_read dut-a");
    assert_eq!(data["base64"], "YWxwaGE=");

    // dut_list / adb_list_devices / skill_export expose only the bound DUT.
    let data = call(&client, "dut_list", json!({}))
        .await
        .expect("dut_list");
    let duts = data["duts"].as_array().unwrap();
    assert_eq!(duts.len(), 1, "{data}");
    assert_eq!(duts[0]["dut_id"], "dut-a");

    let data = call(&client, "adb_list_devices", json!({}))
        .await
        .expect("adb_list_devices");
    let devices = data["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1, "{data}");
    assert_eq!(devices[0]["serial"], "serial-a");

    let data = call(&client, "skill_export", json!({}))
        .await
        .expect("skill_export");
    let markdown = data["markdown"].as_str().unwrap();
    assert!(markdown.contains("仅可读 DUT `dut-a`"), "{markdown}");
    assert!(
        !markdown.contains("dut-b"),
        "SKILL must not mention other DUTs"
    );

    // relay_status is not on the Plan route: plan mode rejects it immediately.
    let error = call(&client, "relay_status", json!({"dutId": "dut-b"}))
        .await
        .expect_err("relay_status must not be on the Plan route");
    assert!(error.contains("PLAN_MODE_ONLY"), "{error}");

    client.cancel().await.ok();
    task.abort();
}
