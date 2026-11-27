//! Release security, file-fence, board-control (MaskROM/Loader), and MCP HTTP
//! concurrency stress tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use rmcp::model::{CallToolRequestParams, CallToolResponse, ClientConfig};
use rmcp::service::serve_client;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::json;

use duthub_core::agent_access::{
    AgentAccessRuntime, AgentAccessSettings, AgentCredential, AgentListener, DutAction, DutGrant,
    generate_token,
};
use duthub_core::audit::AuditSink;
use duthub_core::lease::{ControlLeaseManager, LeaseHolder, WriteIdentity};
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::notify::{NotifyOutcome, NotifyRequest, UserNotifier};
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, ControlStatusDto, DutInfo, DutSummary,
    HeartbeatViewDto, LeaseGrantDto, LogLineDto, LogReadResult, McpState, ProcessResultDto,
    RelayBrief, RelayChannelsBrief, SerialBindInfo,
};
use duthub_mcp::transport::{MAX_MCP_REQUEST_BYTES, spawn_http_listener_on};

struct NoopNotifier;
impl UserNotifier for NoopNotifier {
    fn notify(&self, _request: &NotifyRequest) -> NotifyOutcome {
        NotifyOutcome::Queued
    }
}

struct InteractiveTestState {
    artifact_root: PathBuf,
    log_dir: PathBuf,
    lease: Arc<ControlLeaseManager>,
    relay_brief: Mutex<RelayBrief>,
    lines: Mutex<Vec<LogLineDto>>,
    marks: Mutex<Vec<String>>,
    control_log: Mutex<Vec<String>>,
    tx_log: Mutex<Vec<String>>,
}

impl InteractiveTestState {
    fn new(artifact_root: PathBuf, log_dir: PathBuf, lease: Arc<ControlLeaseManager>) -> Self {
        let channels = RelayChannelsBrief {
            reset: Some(1),
            ..Default::default()
        };
        let capabilities = channels.capabilities();
        let relay = RelayBrief {
            mode: "usb-board".into(),
            board_id: Some("board-ch340".into()),
            board_name: Some("ch340-relay".into()),
            node: Some("COM25".into()),
            driver: Some("CH340".into()),
            power_outlet: None,
            channels,
            capabilities,
        };
        Self {
            artifact_root,
            log_dir,
            lease,
            relay_brief: Mutex::new(relay),
            lines: Mutex::new(vec![LogLineDto {
                seq: 1,
                text: "Linux rk3576 login: ".into(),
            }]),
            marks: Mutex::new(Vec::new()),
            control_log: Mutex::new(Vec::new()),
            tx_log: Mutex::new(Vec::new()),
        }
    }

    fn push_rx(&self, text: &str) {
        let mut lines = self.lines.lock().unwrap();
        let next_seq = lines.last().map_or(1, |l| l.seq + 1);
        lines.push(LogLineDto {
            seq: next_seq,
            text: text.to_owned(),
        });
    }
}

impl McpState for InteractiveTestState {
    fn list_duts(&self) -> Vec<DutSummary> {
        vec![DutSummary {
            dut_id: "dut-rk3576".into(),
            name: "RK3576-Board".into(),
            source: "device".into(),
            port: "COM8".into(),
            baud: 1_500_000,
        }]
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        if dut_id != "dut-rk3576" {
            return None;
        }
        Some(DutInfo {
            dut_id: "dut-rk3576".into(),
            name: "RK3576-Board".into(),
            source: "device".into(),
            serial: SerialBindInfo {
                host: "local".into(),
                port: "COM8".into(),
                baud: 1_500_000,
                format: "8N1".into(),
            },
            adb_serial: Some("RK3576001".into()),
            relay: Some(self.relay_brief.lock().unwrap().clone()),
            mcp_label: None,
            session_lines: self.lines.lock().unwrap().last().map(|l| l.seq),
        })
    }

    fn dut_acquire_control(
        &self,
        dut_id: &str,
        holder: LeaseHolder,
        wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        let wait = (wait_ms > 0).then(|| Duration::from_millis(wait_ms));
        let grant = self.lease.acquire(dut_id, &holder, wait).map_err(|e| {
            format!(
                "LeaseRequired/{}: {}",
                duthub_mcp::auth_http::lease_error_code(&e),
                e.message()
            )
        })?;
        Ok(LeaseGrantDto {
            lease_id: grant.lease_id,
            dut_id: grant.dut_id,
            generation: grant.generation,
            expires_at_unix_ms: grant.expires_at_unix_ms,
            holder: grant.holder,
            session_name: grant.session_name,
            acquired_at_unix_ms: grant.acquired_at_unix_ms,
        })
    }

    fn dut_acquire_control_with_session(
        &self,
        dut_id: &str,
        holder: LeaseHolder,
        session_name: &str,
        wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        let wait = (wait_ms > 0).then(|| Duration::from_millis(wait_ms));
        let grant = self
            .lease
            .acquire_with_session(dut_id, &holder, session_name, wait)
            .map_err(|e| {
                format!(
                    "LeaseRequired/{}: {}",
                    duthub_mcp::auth_http::lease_error_code(&e),
                    e.message()
                )
            })?;
        Ok(LeaseGrantDto {
            lease_id: grant.lease_id,
            dut_id: grant.dut_id,
            generation: grant.generation,
            expires_at_unix_ms: grant.expires_at_unix_ms,
            holder: grant.holder,
            session_name: grant.session_name,
            acquired_at_unix_ms: grant.acquired_at_unix_ms,
        })
    }

    fn dut_renew_control(&self, dut_id: &str, lease_id: &str) -> Result<LeaseGrantDto, String> {
        let grant = self.lease.renew(dut_id, lease_id).map_err(|e| {
            format!(
                "LeaseRequired/{}: {}",
                duthub_mcp::auth_http::lease_error_code(&e),
                e.message()
            )
        })?;
        Ok(LeaseGrantDto {
            lease_id: grant.lease_id,
            dut_id: grant.dut_id,
            generation: grant.generation,
            expires_at_unix_ms: grant.expires_at_unix_ms,
            holder: grant.holder,
            session_name: grant.session_name,
            acquired_at_unix_ms: grant.acquired_at_unix_ms,
        })
    }

    fn dut_release_control(&self, dut_id: &str, lease_id: &str) -> Result<(), String> {
        self.lease
            .release(dut_id, lease_id)
            .map_err(|e| e.message().clone())
    }

    fn dut_control_status(&self, dut_id: &str) -> Result<ControlStatusDto, String> {
        let status = self.lease.status(dut_id);
        Ok(ControlStatusDto {
            dut_id: status.dut_id,
            state: format!("{:?}", status.phase).to_lowercase(),
            holder: status.holder,
            session_name: status.session_name,
            acquired_at_unix_ms: status.acquired_at_unix_ms,
            occupied_secs: status.occupied_secs,
            remaining_secs: status.remaining_secs,
            generation: status.generation,
            queue_len: status.queue_len,
            protected_op: status.protected_op,
        })
    }

    fn serial_read(&self, dut_id: &str, since_seq: u64, limit: usize) -> Option<LogReadResult> {
        if dut_id != "dut-rk3576" {
            return None;
        }
        let guard = self.lines.lock().unwrap();
        let lines: Vec<LogLineDto> = guard
            .iter()
            .filter(|l| l.seq > since_seq)
            .take(limit)
            .cloned()
            .collect();
        let last_seq = lines.last().map_or(since_seq, |l| l.seq);
        Some(LogReadResult {
            dut_id: dut_id.into(),
            session_id: Some("sess-dut-rk3576".into()),
            last_seq,
            lines,
        })
    }

    fn log_archive_dir(&self) -> Option<PathBuf> {
        Some(self.log_dir.clone())
    }

    fn dut_artifact_root(&self) -> Option<PathBuf> {
        Some(self.artifact_root.clone())
    }

    fn serial_write(&self, dut_id: &str, text: &str, _writer: WriteIdentity) -> Result<(), String> {
        if self.lease.manual_control() {
            return Err("LeaseRequired/MANUAL_CONTROL: manual takeover active".into());
        }
        self.tx_log.lock().unwrap().push(text.to_owned());
        match text {
            t if t.contains("rockusb") => {
                self.push_rx("rockusb 0 mmc 0: ready in loader mode\r\n");
            }
            t if t.contains('\r') => self.push_rx("=> "),
            t if t.contains("reboot loader") => {
                self.push_rx("Rebooting into loader mode...\r\n");
            }
            _ => {}
        }
        let _ = dut_id;
        Ok(())
    }

    fn adb_list(&self) -> Vec<AdbDeviceDto> {
        vec![AdbDeviceDto {
            serial: "RK3576001".into(),
            state: "device".into(),
            model: Some("rk3576".into()),
        }]
    }

    fn adb_shell(
        &self,
        _dut_id: &str,
        _argv: &[String],
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("disabled".into())
    }

    fn control_power(
        &self,
        _dut_id: &str,
        on: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_log.lock().unwrap().push(format!("power:{on}"));
        Ok(())
    }

    fn control_reset(
        &self,
        _dut_id: &str,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        if self.lease.manual_control() {
            return Err("LeaseRequired/MANUAL_CONTROL: manual takeover active".into());
        }
        self.control_log.lock().unwrap().push("reset".into());
        // Simulate U-Boot autoboot banner appearing right after relay reset.
        self.push_rx("U-Boot 2017.09 RK3576\r\nHit any key to stop autoboot:  2\r\n");
        Ok(())
    }

    fn control_power_key(
        &self,
        _dut_id: &str,
        long_press: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        self.control_log
            .lock()
            .unwrap()
            .push(format!("power_key:{long_press}"));
        Ok(())
    }

    fn control_enter_maskrom(
        &self,
        _dut_id: &str,
        _hold_ms: u64,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        let relay = self.relay_brief.lock().unwrap();
        if relay.channels.maskrom.is_none() {
            return Err(
                "MaskROM relay channel is not configured on this DUT (configured: reset=CH1)"
                    .into(),
            );
        }
        self.control_log
            .lock()
            .unwrap()
            .push("enter_maskrom:released_off".into());
        Ok(())
    }

    fn relay_status(&self, _dut_id: &str) -> Result<Vec<(u8, bool)>, String> {
        Ok(vec![(1, false)])
    }

    fn heartbeat_get(&self, dut_id: &str) -> Result<HeartbeatViewDto, String> {
        Ok(HeartbeatViewDto::new(
            dut_id,
            duthub_core::heartbeat::HeartbeatConfig::default(),
            duthub_core::heartbeat::HeartbeatState::default(),
        ))
    }

    fn log_mark(&self, _dut_id: &str, label: &str) -> Result<(), String> {
        self.marks.lock().unwrap().push(label.to_owned());
        Ok(())
    }

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("disabled".into())
    }

    fn flash_list(&self, _preset_id: Option<&str>) -> Result<ProcessResultDto, String> {
        Ok(ProcessResultDto {
            exit_code: Some(0),
            stdout: "DevNo=1\tVid=0x2207,Pid=0x350e,LocationID=101\tMode=Loader\n".into(),
            stderr: String::new(),
            elapsed_ms: 15,
        })
    }

    fn flash_update(
        &self,
        dut_id: &str,
        file_id: &str,
        _preset_id: Option<&str>,
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        let path = duthub_core::dut_artifacts::resolve_owned_file(
            &self.artifact_root,
            dut_id,
            "RK3576-Board",
            file_id,
        )
        .map_err(|err| format!("file fence rejected firmware image: {err}"))?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if !matches!(ext.as_deref(), Some("img" | "bin")) {
            return Err("firmware image must have .img or .bin extension".into());
        }
        self.marks
            .lock()
            .unwrap()
            .push(format!("flash_update:{file_id}"));
        Ok(ProcessResultDto {
            exit_code: Some(0),
            stdout: format!(
                "Upgrade firmware {} ok.\n",
                path.file_name().unwrap().to_string_lossy()
            ),
            stderr: String::new(),
            elapsed_ms: 42,
        })
    }

    fn adb_exec(
        &self,
        dut_id: &str,
        op: &str,
        command: Option<&str>,
        file_id: Option<&str>,
        remote_path: Option<&str>,
        _timeout_ms: u64,
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        match op {
            "shell" => Ok(ProcessResultDto {
                exit_code: Some(0),
                stdout: format!("mock-adb-shell: {}\n", command.unwrap_or_default()),
                stderr: String::new(),
                elapsed_ms: 8,
            }),
            "pull" => {
                let fid = file_id.ok_or_else(|| "fileId required".to_string())?;
                let dst = duthub_core::dut_artifacts::prepare_owned_destination(
                    &self.artifact_root,
                    dut_id,
                    "RK3576-Board",
                    fid,
                )
                .map_err(|err| format!("file fence rejected pull destination: {err}"))?;
                std::fs::write(
                    &dst,
                    format!("pulled:{}\n", remote_path.unwrap_or_default()),
                )
                .map_err(|e| e.to_string())?;
                Ok(ProcessResultDto {
                    exit_code: Some(0),
                    stdout: "1 file pulled\n".into(),
                    stderr: String::new(),
                    elapsed_ms: 12,
                })
            }
            "push" => {
                let fid = file_id.ok_or_else(|| "fileId required".to_string())?;
                let _src = duthub_core::dut_artifacts::resolve_owned_file(
                    &self.artifact_root,
                    dut_id,
                    "RK3576-Board",
                    fid,
                )
                .map_err(|err| format!("file fence rejected push source: {err}"))?;
                Ok(ProcessResultDto {
                    exit_code: Some(0),
                    stdout: "1 file pushed\n".into(),
                    stderr: String::new(),
                    elapsed_ms: 10,
                })
            }
            other => Ok(ProcessResultDto {
                exit_code: Some(0),
                stdout: format!("mock-adb-{other}\n"),
                stderr: String::new(),
                elapsed_ms: 5,
            }),
        }
    }

    fn approval_issue(&self, dut_id: &str, action: &str, _args_json: &str) -> ApprovalIssueResult {
        ApprovalIssueResult {
            token: format!("tok-{dut_id}-{action}"),
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
        Ok(())
    }
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                walk(base, &path, out);
            } else if meta.is_file() {
                let rel = path.strip_prefix(base).unwrap().to_path_buf();
                out.insert(rel, std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut map = BTreeMap::new();
    walk(root, root, &mut map);
    map
}

async fn call_json(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ClientConfig>,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let params = CallToolRequestParams::new(tool.to_owned())
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    let response = client
        .call_tool_once(params)
        .await
        .map_err(|e| e.to_string())?;
    let CallToolResponse::Complete(result) = response else {
        return Err("unexpected intermediate response".into());
    };
    if result.is_error == Some(true) {
        return Err(format!("{:?}", result.content));
    }
    Ok(result.structured_content.unwrap_or(serde_json::Value::Null))
}

/// Adversarial file-fence test: MCP can read inside duthub's DUT directory and
/// CANNOT read, traverse, or write anywhere outside duthub's directory.
#[tokio::test]
async fn mcp_file_fence_blocks_traversal_junctions_and_outside_writes() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sandbox = tempfile::tempdir().unwrap();
    let duthub_root = sandbox.path().join("duthub-home");
    let outside_dir = sandbox.path().join("windows-outside-secret");
    std::fs::create_dir_all(&duthub_root).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("secret.txt"), b"TOP_SECRET_OUTSIDE_DATA").unwrap();
    let outside_before = snapshot_tree(&outside_dir);

    // Prepare owned DUT artifact inside duthub_root.
    let (mut f, _) = duthub_core::dut_artifacts::create_file(
        &duthub_root,
        "dut-rk3576",
        "RK3576-Board",
        "boot.log",
    )
    .unwrap();
    std::io::Write::write_all(&mut f, b"duthub inside evidence\n").unwrap();
    drop(f);

    // Plant a Windows NTFS junction inside the DUT folder pointing to `outside_dir`.
    #[cfg(windows)]
    {
        let junction = duthub_root.join("RK3576-Board").join("escape_link");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction.display().to_string(),
                &outside_dir.display().to_string(),
            ])
            .status()
            .expect("spawn mklink /J");
        assert!(status.success(), "mklink /J failed");
    }

    let lease = Arc::new(ControlLeaseManager::new(None));
    let log_dir = duthub_root.join("duthub-data");
    std::fs::create_dir_all(&log_dir).unwrap();
    let state = Arc::new(InteractiveTestState::new(
        duthub_root.clone(),
        log_dir,
        lease.clone(),
    ));
    let token = generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![AgentCredential {
            id: "cred-fence".into(),
            listener_id: Some("listener-dut".into()),
            agent_name: "fence-tester".into(),
            token: token.clone(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-rk3576".into(),
                actions: vec![DutAction::Read],
            }],
        }],
        listeners: vec![AgentListener {
            id: "listener-dut".into(),
            port: 0,
            dut_id: Some("dut-rk3576".into()),
        }],
        ..Default::default()
    });
    let access = Arc::new(AccessContext::new(auth, lease, AuditSink::memory()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let handle = spawn_http_listener_on(
        listener,
        state,
        Some(access),
        Some(Arc::from("listener-dut")),
        Some(Arc::from("dut-rk3576")),
        Some(Arc::new(NoopNotifier)),
    )
    .unwrap();

    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(format!("http://{}/mcp", handle.endpoint))
            .auth_header(token),
    );
    let client = serve_client(ClientConfig::default(), transport)
        .await
        .unwrap();

    // Legitimate inside file is listed and readable; the planted junction is hidden.
    let files = call_json(&client, "dut_files", json!({})).await.unwrap();
    let listed: Vec<&str> = files["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["id"].as_str())
        .collect();
    assert!(
        listed.contains(&"boot.log"),
        "inside file must be visible: {listed:?}"
    );
    assert!(
        !listed
            .iter()
            .any(|id| id.contains("escape_link") || id.contains("secret")),
        "junction escape must never be listed: {listed:?}"
    );
    let read_ok = call_json(
        &client,
        "dut_file_read",
        json!({"dutId": "dut-rk3576", "fileId": "boot.log"}),
    )
    .await
    .unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(read_ok["base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, b"duthub inside evidence\n");

    // Path traversal, absolute paths, UNC paths, Win32 device prefix, NTFS ADS,
    // reserved device names, and junction traversal must ALL fail closed.
    let hostile_file_ids = [
        "../windows-outside-secret/secret.txt",
        "..\\..\\windows-outside-secret\\secret.txt",
        "escape_link/secret.txt",
        "escape_link\\secret.txt",
        "C:\\Windows\\win.ini",
        "\\\\127.0.0.1\\C$\\Windows\\win.ini",
        "\\\\?\\C:\\Windows\\win.ini",
        "boot.log:evil.exe",
        "CON",
        "NUL",
        "COM1",
        ".duthub-dut-id",
    ];
    for hostile in hostile_file_ids {
        let err = call_json(
            &client,
            "dut_file_read",
            json!({"dutId": "dut-rk3576", "fileId": hostile}),
        )
        .await
        .expect_err(&format!("hostile fileId must be rejected: {hostile}"));
        assert!(
            !err.contains("TOP_SECRET_OUTSIDE_DATA"),
            "error must never leak outside data: {err}"
        );
    }

    // `skill_export` is strictly read-only and must not create any files on disk.
    let root_before_skill = snapshot_tree(&duthub_root);
    let skill = call_json(&client, "skill_export", json!({})).await.unwrap();
    assert!(skill["markdown"].as_str().unwrap().contains("dut_control"));
    assert!(skill["markdown"].as_str().unwrap().contains("serial_exec"));
    assert_eq!(
        snapshot_tree(&duthub_root),
        root_before_skill,
        "skill_export must not write any files to disk"
    );

    // Hostile/outside directory must be 100% untouched byte-for-byte.
    assert_eq!(
        snapshot_tree(&outside_dir),
        outside_before,
        "MCP operations must never modify any file outside duthub root"
    );

    // MCP Resources & Prompts: read-only serial tail, artifact file, PNG blob,
    // and guided workflow prompts; hostile resource URIs must fail closed.
    let templates = client.list_all_resource_templates().await.unwrap();
    let template_uris: Vec<&str> = templates.iter().map(|t| t.uri_template.as_str()).collect();
    assert!(template_uris.contains(&"duthub://dut/{dutId}/serial/tail"));
    assert!(template_uris.contains(&"duthub://dut/{dutId}/logs/{segmentId}"));
    assert!(template_uris.contains(&"duthub://dut/{dutId}/files/{fileId}"));

    let tail_res = client
        .read_resource(rmcp::model::ReadResourceRequestParams::new(
            "duthub://dut/dut-rk3576/serial/tail",
        ))
        .await
        .unwrap();
    let tail_json = serde_json::to_value(&tail_res).unwrap();
    assert!(tail_json.to_string().contains("Linux rk3576 login"));

    let file_res = client
        .read_resource(rmcp::model::ReadResourceRequestParams::new(
            "duthub://dut/dut-rk3576/files/boot.log",
        ))
        .await
        .unwrap();
    assert!(
        serde_json::to_value(&file_res)
            .unwrap()
            .to_string()
            .contains("duthub inside evidence")
    );

    for hostile_uri in [
        "duthub://dut/dut-rk3576/files/../windows-outside-secret/secret.txt",
        "duthub://dut/dut-rk3576/files/escape_link/secret.txt",
        "duthub://dut/dut-other/serial/tail",
        "file:///C:/Windows/win.ini",
    ] {
        assert!(
            client
                .read_resource(rmcp::model::ReadResourceRequestParams::new(hostile_uri))
                .await
                .is_err(),
            "hostile resource URI must be rejected: {hostile_uri}"
        );
    }

    let prompts = client.list_all_prompts().await.unwrap();
    let prompt_names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    assert!(prompt_names.contains(&"enter_flash_mode"));
    assert!(prompt_names.contains(&"capture_and_diagnose"));
    let prompt = client
        .get_prompt(rmcp::model::GetPromptRequestParams::new("enter_flash_mode"))
        .await
        .unwrap();
    assert!(
        serde_json::to_value(&prompt)
            .unwrap()
            .to_string()
            .contains("serial_exec")
    );

    client.cancel().await.ok();
    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}

/// End-to-end board control test: `dut_status` exposes configured `reset`
/// channel, read-only tokens cannot control, authorized tokens execute atomic
/// `serial_exec` U-Boot Loader entry + `[mark]` split log, and manual takeover
/// blocks agent control immediately.
#[tokio::test]
async fn mcp_board_control_reset_loader_and_manual_takeover() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sandbox = tempfile::tempdir().unwrap();
    let lease = Arc::new(ControlLeaseManager::new(None));
    let state = Arc::new(InteractiveTestState::new(
        sandbox.path().to_path_buf(),
        sandbox.path().join("duthub-data"),
        lease.clone(),
    ));
    let readonly_token = generate_token();
    let control_token = generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![
            AgentCredential {
                id: "cred-ro".into(),
                listener_id: None,
                agent_name: "readonly-agent".into(),
                token: readonly_token.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-rk3576".into(),
                    actions: vec![DutAction::Read],
                }],
            },
            AgentCredential {
                id: "cred-rw".into(),
                listener_id: None,
                agent_name: "control-agent".into(),
                token: control_token.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-rk3576".into(),
                    actions: vec![
                        DutAction::Read,
                        DutAction::SerialWrite,
                        DutAction::Reset,
                        DutAction::Relay,
                    ],
                }],
            },
        ],
        ..Default::default()
    });
    let access = Arc::new(AccessContext::new(auth, lease.clone(), AuditSink::memory()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let handle = spawn_http_listener_on(
        listener,
        state.clone(),
        Some(access),
        None,
        None,
        Some(Arc::new(NoopNotifier)),
    )
    .unwrap();

    let readonly_client = serve_client(
        ClientConfig::default(),
        StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            StreamableHttpClientTransportConfig::with_uri(format!(
                "http://{}/mcp",
                handle.endpoint
            ))
            .auth_header(readonly_token),
        ),
    )
    .await
    .unwrap();

    // 1) `dut_status` exposes the configured `reset` channel and recommends
    //    `serial_exec` with `triggerReset` since `maskrom`/`recovery` are unbound.
    let status = call_json(
        &readonly_client,
        "dut_status",
        json!({"dutId": "dut-rk3576"}),
    )
    .await
    .unwrap();
    assert_eq!(status["relay"]["channels"]["reset"], 1);
    assert_eq!(status["relay"]["capabilities"], json!(["reset"]));
    assert_eq!(status["bootModes"]["canEnterMaskrom"], false);
    assert_eq!(status["bootModes"]["canEnterLoaderByHardware"], false);
    assert_eq!(status["bootModes"]["canEnterLoaderBySerial"], true);
    assert_eq!(
        status["bootModes"]["recommendedLoaderFlow"],
        "adb_exec_reboot_loader_or_serial_exec"
    );

    // 2) Read-only token is denied `dut_control` and `serial_exec`.
    let err = call_json(
        &readonly_client,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "reset"}),
    )
    .await
    .expect_err("read-only token cannot reset board");
    assert!(err.contains("UNAUTHORIZED"), "{err}");

    let err = call_json(
        &readonly_client,
        "serial_exec",
        json!({"dutId": "dut-rk3576", "text": "reboot loader\r"}),
    )
    .await
    .expect_err("read-only token cannot write serial");
    assert!(err.contains("UNAUTHORIZED"), "{err}");

    // 3) Authorized token runs `serial_exec` with `triggerReset` + U-Boot intercept
    //    + `thenSend` + `markLabel` to enter Loader cleanly in one atomic call.
    let control_client = serve_client(
        ClientConfig::default(),
        StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            StreamableHttpClientTransportConfig::with_uri(format!(
                "http://{}/mcp",
                handle.endpoint
            ))
            .auth_header(control_token),
        ),
    )
    .await
    .unwrap();

    let res = call_json(
        &control_client,
        "serial_exec",
        json!({
            "dutId": "dut-rk3576",
            "triggerReset": true,
            "waitFor": "Hit any key to stop autoboot",
            "send": "\r",
            "thenSend": "rockusb 0 mmc 0\r",
            "expect": "ready in loader mode",
            "markLabel": "enter-loader-for-flash",
            "timeoutMs": 5000
        }),
    )
    .await
    .unwrap();
    assert_eq!(res["ok"], true);
    assert_eq!(res["resetTriggered"], true);
    assert_eq!(res["waitMatched"], true);
    assert_eq!(res["expectMatched"], true);
    assert!(
        res["output"]
            .as_str()
            .unwrap()
            .contains("ready in loader mode")
    );
    assert_eq!(
        state.marks.lock().unwrap().as_slice(),
        &["enter-loader-for-flash"]
    );

    // 4) Calling `enter_maskrom` when only `reset` is bound fails clearly.
    let err = call_json(
        &control_client,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "enter_maskrom"}),
    )
    .await
    .expect_err("unbound maskrom channel must fail clearly");
    assert!(err.contains("MaskROM"), "{err}");

    // 5) Manual takeover immediately blocks agent reset & serial writes.
    lease.set_manual_control(true).unwrap();
    let err = call_json(
        &control_client,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "reset"}),
    )
    .await
    .expect_err("manual takeover must block agent reset");
    assert!(
        err.contains("BUSY") || err.contains("MANUAL") || err.contains("人工"),
        "{err}"
    );
    lease.set_manual_control(false).unwrap();

    readonly_client.cancel().await.ok();
    control_client.cancel().await.ok();
    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}

/// High-concurrency MCP HTTP stress test + >1 MiB payload defense.
#[tokio::test]
async fn mcp_http_concurrent_stress_and_oversized_body_defense() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sandbox = tempfile::tempdir().unwrap();
    let lease = Arc::new(ControlLeaseManager::new(None));
    let state = Arc::new(InteractiveTestState::new(
        sandbox.path().to_path_buf(),
        sandbox.path().join("duthub-data"),
        lease.clone(),
    ));
    let token = generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![AgentCredential {
            id: "cred-stress".into(),
            listener_id: None,
            agent_name: "stress-client".into(),
            token: token.clone(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-rk3576".into(),
                actions: vec![DutAction::Read],
            }],
        }],
        ..Default::default()
    });
    let access = Arc::new(AccessContext::new(auth, lease, AuditSink::memory()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let handle = spawn_http_listener_on(
        listener,
        state,
        Some(access),
        None,
        None,
        Some(Arc::new(NoopNotifier)),
    )
    .unwrap();
    let url = format!("http://{}/mcp", handle.endpoint);

    // 1) Oversized >1 MiB body must be rejected with 413 without harming the server.
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let oversized = http
        .post(&url)
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(vec![b'a'; MAX_MCP_REQUEST_BYTES + 128])
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    // 2) 64 concurrent authenticated MCP sessions calling `dut_list`, `dut_status`,
    //    `serial_read`, and `skill_export`.
    let started = Instant::now();
    let mut tasks = Vec::new();
    for worker in 0..64usize {
        let url = url.clone();
        let token = token.clone();
        tasks.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let transport = StreamableHttpClientTransport::with_client(
                reqwest::Client::builder().no_proxy().build().unwrap(),
                StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
            );
            let client = serve_client(ClientConfig::default(), transport)
                .await
                .expect("stress client init");
            let list = call_json(&client, "dut_list", json!({})).await.unwrap();
            assert_eq!(list["duts"].as_array().unwrap().len(), 1);
            let status = call_json(&client, "dut_status", json!({"dutId": "dut-rk3576"}))
                .await
                .unwrap();
            assert_eq!(status["relay"]["channels"]["reset"], 1);
            if worker % 2 == 0 {
                let _ = call_json(
                    &client,
                    "serial_read",
                    json!({"dutId": "dut-rk3576", "tail": 10}),
                )
                .await
                .unwrap();
            } else {
                let _ = call_json(&client, "skill_export", json!({})).await.unwrap();
            }
            client.cancel().await.ok();
            t0.elapsed()
        }));
    }
    let mut latencies_ms = Vec::with_capacity(tasks.len());
    for task in tasks {
        latencies_ms.push(u64::try_from(task.await.unwrap().as_millis()).unwrap_or(u64::MAX));
    }
    latencies_ms.sort_unstable();
    let p99 = latencies_ms[(latencies_ms.len() * 99 / 100).min(latencies_ms.len() - 1)];
    eprintln!(
        "[stress] 64 concurrent MCP workflows finished in {:?} (p50={}ms, p99={}ms)",
        started.elapsed(),
        latencies_ms[latencies_ms.len() / 2],
        p99
    );
    assert!(p99 < 10_000, "p99 latency too high: {p99}ms");

    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}

/// Verifies `adb_exec` and `dut_control(flash_list|flash_update)` respect
/// per-action grants and enforce the DUT workdir file fence.
#[tokio::test]
async fn mcp_adb_and_flash_respect_grants_and_file_fence() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sandbox = tempfile::tempdir().unwrap();
    let duthub_root = sandbox.path().join("duthub-home");
    let outside_dir = sandbox.path().join("outside");
    std::fs::create_dir_all(&duthub_root).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("evil.img"), b"NOT_ALLOWED").unwrap();

    // Stage one legitimate firmware image inside the owned DUT folder.
    let (mut f, _) = duthub_core::dut_artifacts::create_file(
        &duthub_root,
        "dut-rk3576",
        "RK3576-Board",
        "rk3576-test.img",
    )
    .unwrap();
    std::io::Write::write_all(&mut f, b"RKFW_MOCK_IMAGE").unwrap();
    drop(f);

    let lease = Arc::new(ControlLeaseManager::new(None));
    let state = Arc::new(InteractiveTestState::new(
        duthub_root.clone(),
        duthub_root.join("duthub-data"),
        lease.clone(),
    ));
    let ro_token = generate_token();
    let ops_token = generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![
            AgentCredential {
                id: "cred-ro".into(),
                listener_id: None,
                agent_name: "readonly".into(),
                token: ro_token.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-rk3576".into(),
                    actions: vec![DutAction::Read],
                }],
            },
            AgentCredential {
                id: "cred-ops".into(),
                listener_id: None,
                agent_name: "flash-and-adb".into(),
                token: ops_token.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-rk3576".into(),
                    actions: vec![DutAction::Read, DutAction::Adb, DutAction::Flash],
                }],
            },
        ],
        ..Default::default()
    });
    let access = Arc::new(AccessContext::new(auth, lease, AuditSink::memory()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let handle = spawn_http_listener_on(
        listener,
        state.clone(),
        Some(access),
        None,
        None,
        Some(Arc::new(NoopNotifier)),
    )
    .unwrap();
    let url = format!("http://{}/mcp", handle.endpoint);

    let ro_client = serve_client(
        ClientConfig::default(),
        StreamableHttpClientTransport::with_client(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            StreamableHttpClientTransportConfig::with_uri(url.clone()).auth_header(ro_token),
        ),
    )
    .await
    .unwrap();

    // Read-only client may query `flash_list`, but is denied `flash_update` and `adb_exec`.
    let listed = call_json(
        &ro_client,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "flash_list"}),
    )
    .await
    .unwrap();
    assert_eq!(listed["inLoader"], true);

    assert!(
        call_json(
            &ro_client,
            "dut_control",
            json!({"dutId": "dut-rk3576", "action": "flash_update", "fileId": "rk3576-test.img"})
        )
        .await
        .is_err(),
        "read-only credential must not flash"
    );
    assert!(
        call_json(
            &ro_client,
            "adb_exec",
            json!({"dutId": "dut-rk3576", "op": "shell", "command": "uname -a"})
        )
        .await
        .is_err(),
        "read-only credential must not run adb_exec"
    );

    let ops_client = serve_client(
        ClientConfig::default(),
        StreamableHttpClientTransport::with_client(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            StreamableHttpClientTransportConfig::with_uri(url).auth_header(ops_token),
        ),
    )
    .await
    .unwrap();

    // Legitimate owned image flashes cleanly.
    let flashed = call_json(
        &ops_client,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "flash_update", "fileId": "rk3576-test.img"}),
    )
    .await
    .unwrap();
    assert_eq!(flashed["ok"], true);

    // Out-of-root traversal image must be blocked.
    for bad in [
        "../outside/evil.img",
        "..\\outside\\evil.img",
        "C:\\Windows\\win.ini",
    ] {
        assert!(
            call_json(
                &ops_client,
                "dut_control",
                json!({"dutId": "dut-rk3576", "action": "flash_update", "fileId": bad})
            )
            .await
            .is_err(),
            "out-of-root flash_update path must be rejected: {bad}"
        );
    }

    // Legitimate ADB shell + pull into owned DUT dir + Resource readback.
    let sh = call_json(
        &ops_client,
        "adb_exec",
        json!({"dutId": "dut-rk3576", "op": "shell", "command": "uname -a"}),
    )
    .await
    .unwrap();
    assert_eq!(sh["ok"], true);
    assert!(sh["stdout"].as_str().unwrap().contains("uname -a"));

    let pulled = call_json(
        &ops_client,
        "adb_exec",
        json!({"dutId": "dut-rk3576", "op": "pull", "remotePath": "/proc/cmdline", "fileId": "cmdline.txt"}),
    )
    .await
    .unwrap();
    assert_eq!(pulled["ok"], true);
    let uri = pulled["resourceUri"].as_str().unwrap().to_owned();
    let res = ops_client
        .read_resource(rmcp::model::ReadResourceRequestParams::new(uri))
        .await
        .unwrap();
    let rmcp::model::ResourceContents::TextResourceContents { text, .. } = &res.contents[0] else {
        panic!("expected pulled text resource");
    };
    assert!(text.contains("pulled:/proc/cmdline"));

    // Out-of-root ADB pull / push must be rejected.
    for bad in [
        "../outside/pwn.txt",
        "C:\\Windows\\Temp\\pwn.txt",
        ".duthub-dut-id",
    ] {
        assert!(
            call_json(
                &ops_client,
                "adb_exec",
                json!({"dutId": "dut-rk3576", "op": "pull", "remotePath": "/proc/cmdline", "fileId": bad})
            )
            .await
            .is_err(),
            "out-of-root adb pull must be rejected: {bad}"
        );
        assert!(
            call_json(
                &ops_client,
                "adb_exec",
                json!({"dutId": "dut-rk3576", "op": "push", "remotePath": "/data/local/tmp/x", "fileId": bad})
            )
            .await
            .is_err(),
            "out-of-root adb push must be rejected: {bad}"
        );
    }

    ro_client.cancel().await.ok();
    ops_client.cancel().await.ok();
    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}

#[derive(Default)]
struct RecordingNotifier {
    notices: Mutex<Vec<duthub_mcp::notify::NotifyRequest>>,
}

impl UserNotifier for RecordingNotifier {
    fn notify(&self, request: &duthub_mcp::notify::NotifyRequest) -> NotifyOutcome {
        self.notices.lock().unwrap().push(request.clone());
        NotifyOutcome::Queued
    }
}

/// Multi-Agent concurrency arbitration + automatic human-intervention alerts:
/// 1) Concurrent Agents serialize cleanly in FIFO order when `waitMs` covers the holder.
/// 2) Multi-step `acquire_lease`/`renew_lease`/`release_lease` keeps Agent A atomic.
/// 3) When Agent B cannot obtain usage rights (contention timeout, GUI holding control,
///    manual takeover, or missing grant), duthub automatically raises a desktop alert
///    explaining WHO requested WHAT and WHY, unless `notifyOnBlocked: false`.
#[tokio::test]
async fn mcp_multi_agent_arbitration_and_human_intervention_alerts() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sandbox = tempfile::tempdir().unwrap();
    let lease = Arc::new(ControlLeaseManager::new(None).with_wait_poll(Duration::from_millis(20)));
    let state = Arc::new(InteractiveTestState::new(
        sandbox.path().to_path_buf(),
        sandbox.path().join("duthub-data"),
        lease.clone(),
    ));
    let notifier = Arc::new(RecordingNotifier::default());
    let token_a = generate_token();
    let token_b = generate_token();
    let token_ro = generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![
            AgentCredential {
                id: "cred-alpha".into(),
                listener_id: None,
                agent_name: "agent-alpha".into(),
                token: token_a.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-rk3576".into(),
                    actions: vec![
                        DutAction::Read,
                        DutAction::SerialWrite,
                        DutAction::Reset,
                        DutAction::Adb,
                        DutAction::Flash,
                    ],
                }],
            },
            AgentCredential {
                id: "cred-beta".into(),
                listener_id: None,
                agent_name: "agent-beta".into(),
                token: token_b.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-rk3576".into(),
                    actions: vec![
                        DutAction::Read,
                        DutAction::SerialWrite,
                        DutAction::Reset,
                        DutAction::Adb,
                    ],
                }],
            },
            AgentCredential {
                id: "cred-gamma-ro".into(),
                listener_id: None,
                agent_name: "agent-gamma".into(),
                token: token_ro.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-rk3576".into(),
                    actions: vec![DutAction::Read],
                }],
            },
        ],
        ..Default::default()
    });
    let access = Arc::new(AccessContext::new(auth, lease.clone(), AuditSink::memory()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let handle = spawn_http_listener_on(
        listener,
        state,
        Some(access),
        None,
        None,
        Some(notifier.clone()),
    )
    .unwrap();
    let url = format!("http://{}/mcp", handle.endpoint);

    let connect = |tok: String| {
        let url = url.clone();
        async move {
            serve_client(
                ClientConfig::default(),
                StreamableHttpClientTransport::with_client(
                    reqwest::Client::builder().no_proxy().build().unwrap(),
                    StreamableHttpClientTransportConfig::with_uri(url).auth_header(tok),
                ),
            )
            .await
            .unwrap()
        }
    };
    let client_a = connect(token_a).await;
    let client_b = connect(token_b).await;
    let client_ro = connect(token_ro).await;

    // 1) Agent A acquires a multi-step exclusive lease and renews it.
    let acquired = call_json(
        &client_a,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "acquire_lease", "sessionName": "alpha-flash-session", "waitMs": 2000}),
    )
    .await
    .unwrap();
    let lease_id = acquired["leaseId"].as_str().unwrap().to_owned();
    let renewed = call_json(
        &client_a,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "renew_lease", "leaseId": lease_id}),
    )
    .await
    .unwrap();
    assert_eq!(renewed["ok"], true);

    // Agent A executes a command using its explicit leaseId.
    let a_cmd = call_json(
        &client_a,
        "adb_exec",
        json!({"dutId": "dut-rk3576", "op": "shell", "command": "id", "leaseId": lease_id}),
    )
    .await
    .unwrap();
    assert_eq!(a_cmd["ok"], true);

    // 2) Agent B concurrently requests `adb_exec` with `waitMs: 3000` while Agent A
    //    releases its lease 200ms later -> Agent B queues cleanly and succeeds with ZERO alerts!
    let b_task = tokio::spawn({
        let url = url.clone();
        async move {
            let _ = url;
        }
    });
    let _ = b_task.await;
    let (b_res, rel_res) = tokio::join!(
        call_json(
            &client_b,
            "adb_exec",
            json!({"dutId": "dut-rk3576", "op": "shell", "command": "uptime", "waitMs": 3000}),
        ),
        async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            call_json(
                &client_a,
                "dut_control",
                json!({"dutId": "dut-rk3576", "action": "release_lease", "leaseId": lease_id}),
            )
            .await
        }
    );
    assert_eq!(rel_res.unwrap()["released"], true);
    let b_ok = b_res.expect("Agent B should inherit the lease via FIFO queue");
    assert_eq!(b_ok["ok"], true);
    assert!(
        b_ok["waitedMs"].as_u64().unwrap() >= 100,
        "Agent B should report real wait time: {b_ok:?}"
    );
    assert!(
        notifier.notices.lock().unwrap().is_empty(),
        "Normal queued handoff must not trigger a human alert"
    );

    // 3) Agent A holds a long lease; Agent B times out after 150ms -> automatic desktop alert
    //    explaining WHO requested WHAT and WHO currently holds the DUT!
    let long_lease = call_json(
        &client_a,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "acquire_lease", "sessionName": "alpha-timeout-session"}),
    )
    .await
    .unwrap();
    let long_id = long_lease["leaseId"].as_str().unwrap().to_owned();
    let err_timeout = call_json(
        &client_b,
        "serial_exec",
        json!({"dutId": "dut-rk3576", "text": "ls\r", "waitMs": 150}),
    )
    .await
    .expect_err("Agent B must fail when Agent A does not release in time");
    assert!(err_timeout.contains("DUT_BUSY"), "{err_timeout}");
    {
        let notices = notifier.notices.lock().unwrap();
        assert_eq!(notices.len(), 1, "expected 1 human-intervention alert");
        let n = &notices[0];
        assert_eq!(n.dut_id.as_deref(), Some("dut-rk3576"));
        assert!(
            n.message.contains("agent-beta") && n.message.contains("agent-alpha"),
            "alert must identify both requester and holder: {}",
            n.message
        );
    }

    // 4) Silent probe (`notifyOnBlocked: false`) must NOT trigger a new alert.
    let _ = call_json(
        &client_b,
        "serial_exec",
        json!({"dutId": "dut-rk3576", "text": "ls\r", "waitMs": 60, "notifyOnBlocked": false}),
    )
    .await
    .expect_err("silent probe still fails when busy");
    assert_eq!(
        notifier.notices.lock().unwrap().len(),
        1,
        "notifyOnBlocked=false must suppress desktop alert"
    );

    let _ = call_json(
        &client_a,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "release_lease", "leaseId": long_id}),
    )
    .await
    .unwrap();

    // 5) GUI holding control -> fast-fail (must NOT sleep 5s!) + desktop alert mentioning GUI.
    lease
        .acquire("dut-rk3576", &duthub_core::LeaseHolder::Gui, None)
        .unwrap();
    let t_gui = Instant::now();
    let _ = call_json(
        &client_b,
        "adb_exec",
        json!({"dutId": "dut-rk3576", "op": "shell", "command": "uname -a", "waitMs": 5000}),
    )
    .await
    .expect_err("GUI lease must block Agent B");
    assert!(
        t_gui.elapsed() < Duration::from_secs(2),
        "GUI-held lease should fast-fail instead of wasting waitMs: {:?}",
        t_gui.elapsed()
    );
    {
        let notices = notifier.notices.lock().unwrap();
        let last = notices.last().unwrap();
        assert!(
            last.message.contains("GUI") || last.message.contains("Release control"),
            "alert must explain GUI holds control: {}",
            last.message
        );
    }
    lease.release_for_holder("dut-rk3576", &duthub_core::LeaseHolder::Gui);

    // 6) Manual takeover active -> fast-fail + desktop alert explaining manual takeover.
    lease.set_manual_control(true).unwrap();
    let _ = call_json(
        &client_b,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "reset"}),
    )
    .await
    .expect_err("manual takeover must block reset");
    {
        let notices = notifier.notices.lock().unwrap();
        let last = notices.last().unwrap();
        assert!(
            last.message
                .to_ascii_lowercase()
                .contains("manual takeover"),
            "alert must explain manual takeover: {}",
            last.message
        );
    }
    lease.set_manual_control(false).unwrap();

    // 7) Missing grant (Agent Gamma lacks Flash permission) -> alert explaining missing permission.
    let _ = call_json(
        &client_ro,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "flash_update", "fileId": "test.img"}),
    )
    .await
    .expect_err("read-only agent must be denied flash");
    {
        let notices = notifier.notices.lock().unwrap();
        let last = notices.last().unwrap();
        assert!(
            last.message.contains("agent-gamma") && last.message.contains("grant"),
            "alert must explain missing grant for agent-gamma: {}",
            last.message
        );
    }

    // 8) Mandatory sessionName on acquire_lease + occupancy visibility via dut_list/dut_status.
    let missing_session = call_json(
        &client_a,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "acquire_lease"}),
    )
    .await
    .expect_err("acquire_lease without sessionName must be rejected");
    assert!(
        missing_session.contains("sessionName"),
        "error should mention sessionName: {missing_session}"
    );

    let lease_resp = call_json(
        &client_a,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "acquire_lease", "sessionName": "aging-stress-01"}),
    )
    .await
    .expect("acquire_lease with sessionName");
    assert_eq!(lease_resp["sessionName"], "aging-stress-01");
    assert!(lease_resp["acquiredAtUnixMs"].as_u64().unwrap_or(0) > 0);
    let lease_id = lease_resp["leaseId"].as_str().unwrap().to_owned();

    let status_resp = call_json(&client_ro, "dut_status", json!({"dutId": "dut-rk3576"}))
        .await
        .expect("dut_status");
    assert_eq!(status_resp["control"]["sessionName"], "aging-stress-01");
    assert!(
        status_resp["control"]["acquiredAtUnixMs"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    assert!(status_resp["control"]["occupiedSecs"].is_number());

    let list_resp = call_json(&client_ro, "dut_list", json!({}))
        .await
        .expect("dut_list");
    assert_eq!(
        list_resp["duts"][0]["controlStatus"]["sessionName"],
        "aging-stress-01"
    );
    assert!(
        list_resp["duts"][0]["controlStatus"]["acquiredAtUnixMs"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );

    let _ = call_json(
        &client_a,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "release_lease", "leaseId": lease_id}),
    )
    .await
    .expect("release_lease");

    // 9) agent_feedback writes into `<dut>/feedback/` and dut_files filters by category.
    let fb = call_json(
        &client_ro,
        "agent_feedback",
        json!({
            "dutId": "dut-rk3576",
            "category": "usability",
            "severity": "medium",
            "title": "Need longer boot wait preset",
            "detail": "Flashing took 9 minutes; 10m threshold helps.",
            "toolName": "dut_control",
            "sessionName": "aging-stress-01"
        }),
    )
    .await
    .expect("agent_feedback should succeed for read-authorized agent");
    assert_eq!(fb["ok"], true);
    let fb_file_id = fb["fileId"].as_str().unwrap().to_owned();
    assert!(fb_file_id.starts_with("feedback/"), "{fb}");

    let fb_files = call_json(
        &client_ro,
        "dut_files",
        json!({"dutId": "dut-rk3576", "category": "feedback"}),
    )
    .await
    .expect("dut_files filtered by feedback");
    assert_eq!(
        fb_files["categories"],
        json!(["image", "TEST", "压力&老化", "串口日志", "misc", "feedback"])
    );
    assert!(
        fb_files["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == fb_file_id && f["category"] == "feedback"),
        "{fb_files}"
    );

    client_a.cancel().await.ok();
    client_b.cancel().await.ok();
    client_ro.cancel().await.ok();
    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dut_state_screen_wait_and_logical_keys_work_end_to_end() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let lease = Arc::new(ControlLeaseManager::new(None).with_wait_poll(Duration::from_millis(20)));
    let state = Arc::new(InteractiveTestState::new(
        dir.path().to_path_buf(),
        dir.path().join("duthub-data"),
        lease.clone(),
    ));
    let notifier = Arc::new(RecordingNotifier::default());

    let token_rw = generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![AgentCredential {
            id: "cred-rw".into(),
            listener_id: None,
            agent_name: "agent-rw".into(),
            token: token_rw.clone(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-rk3576".into(),
                actions: vec![DutAction::Read, DutAction::SerialWrite, DutAction::Reset],
            }],
        }],
        ..Default::default()
    });
    let access = Arc::new(AccessContext::new(auth, lease.clone(), AuditSink::memory()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let handle = spawn_http_listener_on(
        listener,
        state.clone(),
        Some(access),
        None,
        None,
        Some(notifier),
    )
    .unwrap();
    let url = format!("http://{}/mcp", handle.endpoint);

    let client = serve_client(
        ClientConfig::default(),
        StreamableHttpClientTransport::with_client(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            StreamableHttpClientTransportConfig::with_uri(url).auth_header(token_rw),
        ),
    )
    .await
    .unwrap();

    // 1) dutState transitions across ready -> busy_agent -> busy_gui -> manual_takeover.
    let list_ready = call_json(&client, "dut_list", json!({})).await.unwrap();
    assert_eq!(list_ready["duts"][0]["dutState"], "ready");
    assert_ne!(
        list_ready["duts"][0]["nextActionHint"].as_str().unwrap(),
        ""
    );

    let l_grant = call_json(
        &client,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "acquire_lease", "sessionName": "smoke-test"}),
    )
    .await
    .unwrap();
    let lease_id = l_grant["leaseId"].as_str().unwrap().to_owned();
    let st_busy = call_json(&client, "dut_status", json!({"dutId": "dut-rk3576"}))
        .await
        .unwrap();
    assert_eq!(st_busy["dutState"], "busy_agent");
    assert!(
        st_busy["nextActionHint"]
            .as_str()
            .unwrap()
            .contains("smoke-test")
    );

    let _ = call_json(
        &client,
        "dut_control",
        json!({"dutId": "dut-rk3576", "action": "release_lease", "leaseId": lease_id}),
    )
    .await
    .unwrap();

    lease
        .acquire("dut-rk3576", &duthub_core::LeaseHolder::Gui, None)
        .unwrap();
    let st_gui = call_json(&client, "dut_status", json!({"dutId": "dut-rk3576"}))
        .await
        .unwrap();
    assert_eq!(st_gui["dutState"], "busy_gui");
    lease.release_for_holder("dut-rk3576", &duthub_core::LeaseHolder::Gui);

    lease.set_manual_control(true).unwrap();
    let st_manual = call_json(&client, "dut_status", json!({"dutId": "dut-rk3576"}))
        .await
        .unwrap();
    assert_eq!(st_manual["dutState"], "manual_takeover");
    lease.set_manual_control(false).unwrap();

    // 2) serial_read(mode="screen") & duthub://dut/{dutId}/serial/screen return clean ANSI-free text.
    state.push_rx("\x1b[32mU-Boot 2025\x1b[0m\r\nLoading 10%\rLoading 100%\r\n");
    let screen_resp = call_json(
        &client,
        "serial_read",
        json!({"dutId": "dut-rk3576", "mode": "screen"}),
    )
    .await
    .unwrap();
    assert_eq!(screen_resp["mode"], "screen");
    let screen_text = screen_resp["screen"].as_str().unwrap();
    assert!(screen_text.contains("U-Boot 2025"), "{screen_text}");
    assert!(screen_text.contains("Loading 100%"), "{screen_text}");
    assert!(
        !screen_text.contains("\x1b["),
        "ANSI must be stripped: {screen_text}"
    );

    let res = client
        .read_resource(rmcp::model::ReadResourceRequestParams::new(
            "duthub://dut/dut-rk3576/serial/screen",
        ))
        .await
        .expect("serial/screen resource");
    let res_text = match &res.contents[0] {
        rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.as_str(),
        other => panic!("expected text resource, got {other:?}"),
    };
    assert!(res_text.contains("Loading 100%"), "{res_text}");

    // 2b) Default stream serial_read and serial/tail resource also strip ANSI highlight codes.
    let stream_resp = call_json(
        &client,
        "serial_read",
        json!({"dutId": "dut-rk3576", "tail": 20}),
    )
    .await
    .unwrap();
    let stream_joined: String = stream_resp["lines"]
        .as_array()
        .expect("lines array")
        .iter()
        .filter_map(|item| item["text"].as_str())
        .collect();
    assert!(stream_joined.contains("U-Boot 2025"), "{stream_joined}");
    assert!(
        !stream_joined.contains("\x1b["),
        "serial_read stream output must strip ANSI highlights: {stream_joined}"
    );

    let tail_res = client
        .read_resource(rmcp::model::ReadResourceRequestParams::new(
            "duthub://dut/dut-rk3576/serial/tail",
        ))
        .await
        .expect("serial/tail resource");
    let tail_text = match &tail_res.contents[0] {
        rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.as_str(),
        other => panic!("expected text resource, got {other:?}"),
    };
    assert!(tail_text.contains("U-Boot 2025"), "{tail_text}");
    assert!(
        !tail_text.contains("\x1b["),
        "serial/tail resource must strip ANSI highlights: {tail_text}"
    );

    // 3) serial_read(sinceSeq, waitRegex, timeoutMs) waits without missing newly pushed output.
    let cursor = screen_resp["last_seq"].as_u64().unwrap();
    let state_bg = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        state_bg.push_rx("root@rk3576-buildroot:/# \r\n");
    });
    let waited = call_json(
        &client,
        "serial_read",
        json!({
            "dutId": "dut-rk3576",
            "sinceSeq": cursor,
            "waitRegex": "root@.*#",
            "timeoutMs": 3000
        }),
    )
    .await
    .unwrap();
    assert_eq!(waited["matched"], true);
    assert_eq!(waited["matchedBy"], "waitRegex");

    // 4) serial_exec logical keys: valid key sends raw byte; invalid key fails whole batch with zero writes.
    let before_writes = state.tx_log.lock().unwrap().len();
    let bad_key = call_json(
        &client,
        "serial_exec",
        json!({"dutId": "dut-rk3576", "keys": ["ctrl+c", "win+r"]}),
    )
    .await
    .expect_err("invalid key must fail batch");
    assert!(
        bad_key.contains("win+r") || bad_key.contains("INVALID_KEY"),
        "{bad_key}"
    );
    assert_eq!(
        state.tx_log.lock().unwrap().len(),
        before_writes,
        "invalid key batch must transmit zero bytes"
    );

    let key_ok = call_json(
        &client,
        "serial_exec",
        json!({"dutId": "dut-rk3576", "keys": ["ctrl+c"]}),
    )
    .await
    .unwrap();
    assert_eq!(key_ok["keysSent"], 1);
    assert_eq!(
        state.tx_log.lock().unwrap().last().map(String::as_str),
        Some("\x03")
    );

    // 5) serial_exec expectRegex, settleMs, and stalled/timeout diagnostics with screenTail.
    let state_re = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        state_re.push_rx("Linux rk3576 6.1.118 aarch64\r\n");
    });
    let re_ok = call_json(
        &client,
        "serial_exec",
        json!({
            "dutId": "dut-rk3576",
            "text": "uname -a\r",
            "expectRegex": "Linux rk3576 [0-9]+\\.[0-9]+",
            "timeoutMs": 3000
        }),
    )
    .await
    .unwrap();
    assert_eq!(re_ok["expectMatched"], true);
    assert_eq!(re_ok["matchedBy"], "expectRegex");
    assert_eq!(re_ok["stalled"], false);

    let state_settle = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        state_settle.push_rx("line-1\r\nline-2\r\n");
    });
    let settled = call_json(
        &client,
        "serial_exec",
        json!({
            "dutId": "dut-rk3576",
            "text": "dmesg | tail\r",
            "settleMs": 220,
            "timeoutMs": 3000
        }),
    )
    .await
    .unwrap();
    assert_eq!(settled["matchedBy"], "settled");
    assert!(settled["bytesObserved"].as_u64().unwrap() > 0);
    assert_eq!(settled["stalled"], false);

    let stalled = call_json(
        &client,
        "serial_exec",
        json!({
            "dutId": "dut-rk3576",
            "text": "silent_cmd",
            "expect": "NEVER_ARRIVES",
            "timeoutMs": 320,
            "stallTimeoutMs": 200
        }),
    )
    .await
    .unwrap();
    assert_eq!(stalled["matchedBy"], "timeout");
    assert_eq!(stalled["bytesObserved"], 0);
    assert_eq!(stalled["stalled"], true);
    assert!(stalled["screenTail"].is_string());

    client.cancel().await.ok();
    assert!(handle.shutdown_and_join_with_timeout(Duration::from_secs(5)));
}
