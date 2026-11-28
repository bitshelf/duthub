//! End-to-end tests for streaming firmware upload (`PUT` & tus 1.0.0 including `h2c` for `ztus`),
//! bulk snapshot log export (`log_export` + `GET /dut/{dutId}/logs/export`), `dut_files(category="image")`,
//! `dut_status` dynamic capabilities / `flashPresets`, `skill_export` dynamic permissions, and
//! fail-closed `flash_update` validation with actionable guidance.

use std::io::Read as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::serve_client;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings, DutAction, DutGrant};
use duthub_core::audit::AuditSink;
use duthub_core::lease::{ControlLeaseManager, WriteIdentity};
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, ControlStatusDto, DutInfo, DutSummary,
    FlashPresetSummaryDto, LeaseGrantDto, LogReadResult, McpState, ProcessResultDto,
    SerialBindInfo,
};

struct FlashTransferFixture {
    work_dir: std::path::PathBuf,
    artifact_root: std::path::PathBuf,
    lease: Arc<ControlLeaseManager>,
    approval: duthub_core::ApprovalManager,
    flashed: Mutex<Vec<String>>,
    locale: Mutex<duthub_mcp::i18n::McpLocale>,
}

impl FlashTransferFixture {
    fn new(base: &std::path::Path, lease: Arc<ControlLeaseManager>) -> Arc<Self> {
        let work_dir = base.join("duthub-data");
        std::fs::create_dir_all(&work_dir).unwrap();
        let dut_dir = base.join("DUT-2");
        std::fs::create_dir_all(dut_dir.join("串口日志")).unwrap();
        std::fs::write(dut_dir.join(".duthub-dut-id"), "dut-2").unwrap();
        // Root-level legacy `.img` to verify `dut_files(category="image")` finds it too.
        std::fs::write(dut_dir.join("legacy-root.img"), b"ROOT-IMAGE-DATA").unwrap();
        // Two archived serial segments with ANSI color escape codes.
        std::fs::write(
            dut_dir
                .join("串口日志")
                .join("DUT-2-001_20261127_100000.txt"),
            "\x1b[32mboot step 1\x1b[0m\n",
        )
        .unwrap();
        std::fs::write(
            dut_dir
                .join("串口日志")
                .join("DUT-2-002_20261127_100500.txt"),
            "\x1b[1;33mboot step 2\x1b[0m\n",
        )
        .unwrap();
        Arc::new(Self {
            work_dir,
            artifact_root: base.to_path_buf(),
            lease,
            approval: duthub_core::ApprovalManager::new(),
            flashed: Mutex::new(Vec::new()),
            locale: Mutex::new(duthub_mcp::i18n::McpLocale::ZhCn),
        })
    }

    fn set_locale(&self, locale: duthub_mcp::i18n::McpLocale) {
        *self.locale.lock().unwrap() = locale;
    }
}

impl McpState for FlashTransferFixture {
    fn locale(&self) -> duthub_mcp::i18n::McpLocale {
        *self.locale.lock().unwrap()
    }

    fn log_archive_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.work_dir.clone())
    }

    fn dut_artifact_root(&self) -> Option<std::path::PathBuf> {
        Some(self.artifact_root.clone())
    }

    fn list_duts(&self) -> Vec<DutSummary> {
        vec![DutSummary {
            dut_id: "dut-2".into(),
            name: "DUT-2".into(),
            source: "demo".into(),
            port: "COM8".into(),
            baud: 115_200,
        }]
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        (dut_id == "dut-2").then(|| DutInfo {
            dut_id: "dut-2".into(),
            name: "DUT-2".into(),
            source: "demo".into(),
            serial: SerialBindInfo {
                host: "local".into(),
                port: "COM8".into(),
                baud: 115_200,
                format: "8N1".into(),
            },
            adb_serial: Some("7208c83dd51a8ec8".into()),
            relay: None,
            mcp_label: None,
            session_lines: Some(12),
        })
    }

    fn serial_read(&self, _dut_id: &str, _since_seq: u64, _limit: usize) -> Option<LogReadResult> {
        None
    }

    fn session_epoch(&self, dut_id: &str) -> Option<u64> {
        (dut_id == "dut-2").then_some(1_795_800_000_000)
    }

    fn flash_presets(&self) -> Vec<FlashPresetSummaryDto> {
        vec![FlashPresetSummaryDto {
            id: "preset-upgrade_tool".into(),
            name: "Rockchip upgrade_tool".into(),
            is_default: true,
        }]
    }

    fn serial_write(
        &self,
        _dut_id: &str,
        _text: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
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
        Ok(ProcessResultDto {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            elapsed_ms: 0,
        })
    }

    fn dut_acquire_control(
        &self,
        dut_id: &str,
        holder: duthub_core::LeaseHolder,
        _wait_ms: u64,
    ) -> Result<LeaseGrantDto, String> {
        let g = self
            .lease
            .acquire(dut_id, &holder, None)
            .map_err(|e| e.message())?;
        Ok(LeaseGrantDto {
            lease_id: g.lease_id,
            dut_id: g.dut_id,
            generation: g.generation,
            expires_at_unix_ms: g.expires_at_unix_ms,
            holder: g.holder,
            session_name: g.session_name,
            acquired_at_unix_ms: g.acquired_at_unix_ms,
        })
    }

    fn dut_renew_control(&self, dut_id: &str, lease_id: &str) -> Result<LeaseGrantDto, String> {
        let g = self
            .lease
            .renew(dut_id, lease_id)
            .map_err(|e| e.message())?;
        Ok(LeaseGrantDto {
            lease_id: g.lease_id,
            dut_id: g.dut_id,
            generation: g.generation,
            expires_at_unix_ms: g.expires_at_unix_ms,
            holder: g.holder,
            session_name: g.session_name,
            acquired_at_unix_ms: g.acquired_at_unix_ms,
        })
    }

    fn dut_release_control(&self, dut_id: &str, lease_id: &str) -> Result<(), String> {
        self.lease
            .release(dut_id, lease_id)
            .map_err(|e| e.message())
    }

    fn dut_control_status(&self, dut_id: &str) -> Result<ControlStatusDto, String> {
        let s = self.lease.status(dut_id);
        Ok(ControlStatusDto {
            dut_id: s.dut_id,
            state: match s.phase {
                duthub_core::lease::LeasePhase::Free => "free",
                duthub_core::lease::LeasePhase::Held => "held",
                duthub_core::lease::LeasePhase::Protected => "protected",
                duthub_core::lease::LeasePhase::NeedsInspection => "needs_inspection",
            }
            .into(),
            holder: s.holder,
            session_name: s.session_name,
            acquired_at_unix_ms: s.acquired_at_unix_ms,
            occupied_secs: s.occupied_secs,
            generation: s.generation,
            remaining_secs: s.remaining_secs,
            queue_len: s.queue_len,
            protected_op: s.protected_op,
            protected_file: s.protected_file,
        })
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

    fn control_power(
        &self,
        _dut_id: &str,
        _on: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Ok(())
    }

    fn control_power_cycle(
        &self,
        _dut_id: &str,
        _off_ms: u64,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Ok(())
    }

    fn control_reset(
        &self,
        _dut_id: &str,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Ok(())
    }

    fn control_power_key(
        &self,
        _dut_id: &str,
        _long_press: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Ok(())
    }

    fn relay_test(
        &self,
        _dut_id: &str,
        _channel: u8,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<duthub_core::backend::ChannelTestReport, String> {
        Err("unsupported".into())
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

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("deprecated".into())
    }

    fn flash_update(
        &self,
        dut_id: &str,
        file_id: &str,
        _preset_id: Option<&str>,
        _writer: WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        let verified = duthub_core::dut_artifacts::resolve_owned_file(
            &self.artifact_root,
            dut_id,
            "DUT-2",
            file_id,
        )
        .map_err(|err| format!("file fence rejected firmware image: {err}"))?;
        self.flashed
            .lock()
            .unwrap()
            .push(verified.file_name().unwrap().to_string_lossy().into_owned());
        Ok(ProcessResultDto {
            exit_code: Some(0),
            stdout: "Upgrade firmware ok.".into(),
            stderr: String::new(),
            elapsed_ms: 42,
        })
    }
}

#[derive(Default)]
struct RecordingNotifier {
    notices: Mutex<Vec<duthub_mcp::notify::NotifyRequest>>,
}

impl RecordingNotifier {
    fn take(&self) -> Vec<duthub_mcp::notify::NotifyRequest> {
        std::mem::take(&mut *self.notices.lock().unwrap())
    }
}

impl duthub_mcp::notify::UserNotifier for RecordingNotifier {
    fn notify(&self, req: &duthub_mcp::notify::NotifyRequest) -> duthub_mcp::notify::NotifyOutcome {
        self.notices.lock().unwrap().push(req.clone());
        duthub_mcp::notify::NotifyOutcome::Queued
    }
}

async fn call_tool_json(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ClientConfig>,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    let mut params = CallToolRequestParams::new(name.to_owned());
    if let Value::Object(map) = args {
        params = params.with_arguments(map);
    }
    let response = client
        .call_tool_once(params)
        .await
        .map_err(|err| err.to_string())?;
    let rmcp::model::CallToolResponse::Complete(result) = response else {
        return Err("expected complete tool response".into());
    };
    if result.is_error == Some(true) {
        let text = result
            .content
            .first()
            .and_then(|c| match c {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        return Err(text);
    }
    result
        .structured_content
        .ok_or_else(|| "missing structured_content".into())
}

#[tokio::test]
async fn streaming_put_tus_h2c_flash_and_log_export_enforce_least_privilege() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tmp = tempfile::tempdir().unwrap();
    let read_token = duthub_core::agent_access::generate_token();
    let flash_token = duthub_core::agent_access::generate_token();

    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![
            duthub_core::AgentCredential {
                listener_id: Some("dut-2-ep".into()),
                id: "cred-readonly".into(),
                agent_name: "ro-bot".into(),
                token: read_token.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-2".into(),
                    actions: vec![DutAction::Read],
                }],
            },
            duthub_core::AgentCredential {
                listener_id: Some("dut-2-ep".into()),
                id: "cred-flasher".into(),
                agent_name: "flash-bot".into(),
                token: flash_token.clone(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-2".into(),
                    actions: vec![DutAction::Read, DutAction::Flash],
                }],
            },
        ],
        listeners: vec![duthub_core::agent_access::AgentListener {
            id: "dut-2-ep".into(),
            port: 0,
            dut_id: Some("dut-2".into()),
        }],
        ..Default::default()
    });
    let lease = Arc::new(ControlLeaseManager::new(Some(AuditSink::memory())));
    let ctx = Arc::new(AccessContext::new(auth, lease.clone(), AuditSink::memory()));
    let state = FlashTransferFixture::new(tmp.path(), lease.clone());
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = std_listener.local_addr().unwrap().port();
    let notifier = Arc::new(RecordingNotifier::default());
    let handle = duthub_mcp::transport::spawn_http_listener_on(
        std_listener,
        state.clone(),
        Some(ctx),
        Some(Arc::from("dut-2-ep")),
        Some(Arc::from("dut-2")),
        Some(notifier.clone()),
    )
    .expect("spawn listener");

    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::builder().no_proxy().build().unwrap();

    // 1) Read-only token is rejected with 403 UNAUTHORIZED when trying to upload an image.
    let resp = http
        .put(format!("{base}/dut/dut-2/image/update.img"))
        .header("Authorization", format!("Bearer {read_token}"))
        .body("IMG-BYTES")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["requiredAction"], "flash");

    // 2) Missing token returns 401; browser Origin returns 403; bad extension returns 400.
    let resp = http
        .put(format!("{base}/dut/dut-2/image/update.img"))
        .body("IMG-BYTES")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let resp = http
        .put(format!("{base}/dut/dut-2/image/update.img"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Origin", "http://evil.example")
        .body("IMG-BYTES")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let resp = http
        .put(format!("{base}/dut/dut-2/image/payload.exe"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .body("BAD-EXT")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // 3) Checksum mismatch returns 422 and cleans up the temporary file.
    let img_bytes = b"RK3576-DEBIAN-IMAGE-PAYLOAD".to_vec();
    let img_sha = duthub_core::util::hex_lower(&Sha256::digest(&img_bytes));
    let resp = http
        .put(format!("{base}/dut/dut-2/image/update.img"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("X-Content-SHA256", "0".repeat(64))
        .body(img_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);

    // 4) Valid streaming PUT succeeds (201) and duplicate without overwrite returns 409.
    let resp = http
        .put(format!("{base}/dut/dut-2/image/update.img"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("X-Content-SHA256", &img_sha)
        .body(img_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["fileId"], "image/update.img");
    assert_eq!(created["sha256"], img_sha);

    let dup = http
        .put(format!("{base}/dut/dut-2/image/update.img"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .body(img_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status().as_u16(), 409);

    // 5) Resumable tus 1.0.0 upload over prior-knowledge HTTP/2 (`h2c`, matching `ztus`).
    let h2c = reqwest::Client::builder()
        .no_proxy()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let opt = h2c
        .request(
            reqwest::Method::OPTIONS,
            format!("{base}/dut/dut-2/transfer"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(opt.status().as_u16(), 204);
    assert_eq!(
        opt.headers()
            .get("tus-resumable")
            .unwrap()
            .to_str()
            .unwrap(),
        "1.0.0"
    );

    let tus_payload = b"CHUNK-ONE-AND-CHUNK-TWO-BOOT-IMAGE".to_vec();
    let tus_sha = duthub_core::util::hex_lower(&Sha256::digest(&tus_payload));
    let fname_b64 = base64::engine::general_purpose::STANDARD.encode("boot.img");
    let create = h2c
        .post(format!("{base}/dut/dut-2/transfer"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Tus-Resumable", "1.0.0")
        .header("Upload-Length", tus_payload.len().to_string())
        .header("Upload-Metadata", format!("filename {fname_b64}"))
        .header("Upload-Checksum", format!("sha256 {tus_sha}"))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status().as_u16(), 201);
    let loc = create
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(loc.starts_with("/dut/dut-2/transfer/"), "{loc}");
    let upload_url = format!("{base}{loc}");

    // HEAD returns 200 (required by ztus).
    let head = h2c
        .head(&upload_url)
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Tus-Resumable", "1.0.0")
        .send()
        .await
        .unwrap();
    assert_eq!(head.status().as_u16(), 200);
    assert_eq!(head.headers().get("upload-offset").unwrap(), "0");

    // Chunk 1 + mismatched offset (409) + Chunk 2 completion.
    let split = 12;
    let p1 = h2c
        .patch(&upload_url)
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Tus-Resumable", "1.0.0")
        .header("Upload-Offset", "0")
        .header("Content-Type", "application/offset+octet-stream")
        .body(tus_payload[..split].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(p1.status().as_u16(), 204);
    assert_eq!(
        p1.headers().get("upload-offset").unwrap(),
        &split.to_string()
    );

    let bad_off = h2c
        .patch(&upload_url)
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Tus-Resumable", "1.0.0")
        .header("Upload-Offset", "0")
        .header("Content-Type", "application/offset+octet-stream")
        .body(tus_payload[split..].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(bad_off.status().as_u16(), 409);

    let p2 = h2c
        .patch(&upload_url)
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Tus-Resumable", "1.0.0")
        .header("Upload-Offset", split.to_string())
        .header("Content-Type", "application/offset+octet-stream")
        .body(tus_payload[split..].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(p2.status().as_u16(), 204);
    assert_eq!(
        p2.headers().get("x-duthub-checksum").unwrap(),
        &format!("sha256:{tus_sha}")
    );
    assert_eq!(
        p2.headers().get("x-duthub-file-id").unwrap(),
        "image/boot.img"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("DUT-2").join("image").join("boot.img")).unwrap(),
        tus_payload
    );

    // 6) MCP client checks: dut_status, dut_files(category="image"), skill_export, flash_update, log_export.
    let mcp_client = serve_client(
        ClientConfig::default(),
        StreamableHttpClientTransport::with_client(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp"))
                .auth_header(flash_token.clone()),
        ),
    )
    .await
    .unwrap();

    let status = call_tool_json(&mcp_client, "dut_status", json!({"dutId": "dut-2"}))
        .await
        .unwrap();
    assert_eq!(status["capabilities"]["flash"], true);
    assert_eq!(status["capabilities"]["upload"], true);
    assert_eq!(status["capabilities"]["serialWrite"], false);
    assert_eq!(status["flashPresets"][0]["id"], "preset-upgrade_tool");
    assert_eq!(status["transfer"]["canUpload"], true);
    assert_eq!(status["sessionEpoch"], 1_795_800_000_000u64);

    // The exported skill reflects the capabilities granted to the session.
    let skill_resp = call_tool_json(&mcp_client, "skill_export", json!({}))
        .await
        .unwrap();
    let skill_md = skill_resp["markdown"].as_str().unwrap();
    assert!(skill_md.contains("`flash`"), "{skill_md}");
    assert!(!skill_md.contains("且不开放写工具"), "{skill_md}");

    // dut_files(category="image") includes root `.img` AND uploaded `image/*.img`.
    let files = call_tool_json(
        &mcp_client,
        "dut_files",
        json!({"dutId": "dut-2", "category": "image"}),
    )
    .await
    .unwrap();
    let ids: Vec<&str> = files["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(ids.contains(&"legacy-root.img"), "{ids:?}");
    assert!(ids.contains(&"image/update.img"), "{ids:?}");
    assert!(ids.contains(&"image/boot.img"), "{ids:?}");

    // flash_update rejects missing image with IMAGE_NOT_FOUND + available list without locking DUT.
    let lease_resp = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({"dutId": "dut-2", "action": "acquire_lease", "sessionName": "flash-e2e"}),
    )
    .await
    .unwrap();
    let lease_id = lease_resp["leaseId"].as_str().unwrap().to_owned();

    let missing_err = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({
            "dutId": "dut-2",
            "action": "flash_update",
            "fileId": "image/missing.img",
            "leaseId": lease_id
        }),
    )
    .await
    .unwrap_err();
    assert!(missing_err.contains("IMAGE_NOT_FOUND"), "{missing_err}");
    assert!(missing_err.contains("image/update.img"), "{missing_err}");

    // flash_update succeeds with canonical `image/update.img`.
    let flashed = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({
            "dutId": "dut-2",
            "action": "flash_update",
            "fileId": "image/update.img",
            "leaseId": lease_id
        }),
    )
    .await
    .unwrap();
    assert_eq!(flashed["ok"], true);
    assert_eq!(flashed["presetId"], "preset-upgrade_tool");

    // 7) log_export MCP tool + HTTP GET /dut/dut-2/logs/export (plain + gzip, ANSI stripped).
    let exported = call_tool_json(&mcp_client, "log_export", json!({"dutId": "dut-2"}))
        .await
        .unwrap();
    assert_eq!(exported["segmentCount"], 2);
    let inline_text = exported["inlineText"].as_str().unwrap();
    assert!(inline_text.contains("boot step 1"), "{inline_text}");
    assert!(inline_text.contains("boot step 2"), "{inline_text}");
    assert!(!inline_text.contains("\x1b["), "ANSI must be stripped");

    let gz_resp = http
        .get(format!("{base}/dut/dut-2/logs/export?format=gzip"))
        .header("Authorization", format!("Bearer {read_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(gz_resp.status().as_u16(), 200);
    assert_eq!(gz_resp.headers().get("x-duthub-segments").unwrap(), "2");
    let gz_bytes = gz_resp.bytes().await.unwrap();
    let mut decoder = flate2::read::GzDecoder::new(&gz_bytes[..]);
    let mut unpacked = String::new();
    decoder.read_to_string(&mut unpacked).unwrap();
    assert_eq!(unpacked, inline_text);

    // Cross-DUT log export is rejected with 403.
    let cross = http
        .get(format!("{base}/dut/dut-other/logs/export"))
        .header("Authorization", format!("Bearer {read_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status().as_u16(), 403);

    // 8) skill_export follows duthub's active locale dynamically (zh-CN <-> en-US).
    let zh_skill = call_tool_json(&mcp_client, "skill_export", json!({}))
        .await
        .unwrap();
    assert_eq!(zh_skill["locale"], "zh-CN");
    assert!(
        zh_skill["markdown"]
            .as_str()
            .unwrap()
            .contains("本端点限定于 DUT `dut-2`"),
        "{zh_skill}"
    );
    state.set_locale(duthub_mcp::i18n::McpLocale::EnUs);
    let en_skill = call_tool_json(&mcp_client, "skill_export", json!({}))
        .await
        .unwrap();
    assert_eq!(en_skill["locale"], "en-US");
    let en_md = en_skill["markdown"].as_str().unwrap();
    assert!(en_md.contains("scoped to DUT `dut-2`"), "{en_md}");
    assert!(en_md.contains("`flash`"), "{en_md}");

    // 9) Concurrent upload + flash: different file allowed, same file rejected (409 FILE_IN_USE).
    // Acquire a lease and enter Protected flashing `update.img`.
    let flash_lease = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({
            "dutId": "dut-2",
            "action": "acquire_lease",
            "sessionName": "concurrent-flash-test",
            "waitMs": 2000
        }),
    )
    .await
    .unwrap();
    let _flash_lease_id = flash_lease["leaseId"].as_str().unwrap();
    lease
        .enter_protected_with_file("dut-2", "flash_update image/update.img", Some("update.img"))
        .unwrap();

    // Same filename (including case-insensitive variant) is rejected with 409 FILE_IN_USE.
    for blocked_name in ["update.img", "UPDATE.IMG"] {
        let same_put = http
            .put(format!(
                "{base}/dut/dut-2/image/{blocked_name}?overwrite=true"
            ))
            .header("Authorization", format!("Bearer {flash_token}"))
            .body("OVERWRITE-BLOCKED")
            .send()
            .await
            .unwrap();
        assert_eq!(same_put.status().as_u16(), 409);
        let body: Value = same_put.json().await.unwrap();
        assert_eq!(body["code"], "FILE_IN_USE", "{body}");
    }
    let same_fname_b64 = base64::engine::general_purpose::STANDARD.encode("update.img");
    let same_tus = h2c
        .post(format!("{base}/dut/dut-2/transfer"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Tus-Resumable", "1.0.0")
        .header("Upload-Length", "16")
        .header("Upload-Metadata", format!("filename {same_fname_b64}"))
        .send()
        .await
        .unwrap();
    assert_eq!(same_tus.status().as_u16(), 409);

    // Different filename (`ubuntu.img`) succeeds concurrently while `update.img` is flashing!
    let diff_bytes = b"CONCURRENT-UBUNTU-IMAGE-BYTES".to_vec();
    let diff_sha = duthub_core::util::hex_lower(&Sha256::digest(&diff_bytes));
    let diff_put = http
        .put(format!("{base}/dut/dut-2/image/ubuntu.img"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("X-Content-SHA256", &diff_sha)
        .body(diff_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(diff_put.status().as_u16(), 201);
    lease.exit_protected("dut-2", true).unwrap();

    // Reverse race: while `staging.img` has an in-flight tus upload, flashing `staging.img`
    // fails closed with UPLOAD_IN_PROGRESS.
    let staging_b64 = base64::engine::general_purpose::STANDARD.encode("staging.img");
    let staging_create = h2c
        .post(format!("{base}/dut/dut-2/transfer"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Tus-Resumable", "1.0.0")
        .header("Upload-Length", "32")
        .header("Upload-Metadata", format!("filename {staging_b64}"))
        .send()
        .await
        .unwrap();
    assert_eq!(staging_create.status().as_u16(), 201);
    let staging_loc = staging_create
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let fresh_lease = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({
            "dutId": "dut-2",
            "action": "acquire_lease",
            "sessionName": "reverse-race-test",
            "waitMs": 2000
        }),
    )
    .await
    .unwrap();
    let fresh_lease_id = fresh_lease["leaseId"].as_str().unwrap().to_owned();
    let race_err = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({
            "dutId": "dut-2",
            "action": "flash_update",
            "fileId": "image/staging.img",
            "leaseId": fresh_lease_id
        }),
    )
    .await
    .unwrap_err();
    assert!(race_err.contains("UPLOAD_IN_PROGRESS"), "{race_err}");
    // Delete the in-flight upload to clean up its slot cleanly.
    let del = h2c
        .delete(format!("{base}{staging_loc}"))
        .header("Authorization", format!("Bearer {flash_token}"))
        .header("Tus-Resumable", "1.0.0")
        .send()
        .await
        .unwrap();
    assert_eq!(del.status().as_u16(), 204);

    // 10) Blocked-access alert noise reduction & bilingual localization following duthub locale.
    let _ = notifier.take();
    // Stale/wrong leaseId while another lease is held (`NOT_HOLDER`) must NOT pop a desktop alert.
    let wrong_lease_err = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({
            "dutId": "dut-2",
            "action": "flash_reboot",
            "leaseId": "lease-stale-does-not-exist"
        }),
    )
    .await
    .unwrap_err();
    assert!(wrong_lease_err.contains("NOT_HOLDER"), "{wrong_lease_err}");
    assert!(
        wrong_lease_err.contains("\"humanNotified\":false"),
        "{wrong_lease_err}"
    );
    // Release lease so `lease-stale-does-not-exist` hits `LEASE_UNKNOWN` — also must NOT pop an alert.
    let _ = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({
            "dutId": "dut-2",
            "action": "release_lease",
            "leaseId": fresh_lease_id
        }),
    )
    .await
    .unwrap();
    let stale_err = call_tool_json(
        &mcp_client,
        "dut_control",
        json!({
            "dutId": "dut-2",
            "action": "flash_reboot",
            "leaseId": "lease-stale-does-not-exist"
        }),
    )
    .await
    .unwrap_err();
    assert!(stale_err.contains("LEASE_UNKNOWN"), "{stale_err}");
    assert!(stale_err.contains("\"humanNotified\":false"), "{stale_err}");
    assert!(
        notifier.take().is_empty(),
        "self-recoverable NOT_HOLDER/LEASE_UNKNOWN must not trigger a human-intervention popup"
    );

    // Unauthorized call with a read-only token DOES notify desktop in the active locale.
    let ro_client = serve_client(
        ClientConfig::default(),
        StreamableHttpClientTransport::with_client(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp"))
                .auth_header(read_token.clone()),
        ),
    )
    .await
    .unwrap();

    state.set_locale(duthub_mcp::i18n::McpLocale::ZhCn);
    let _ = call_tool_json(
        &ro_client,
        "dut_control",
        json!({"dutId": "dut-2", "action": "flash_reboot"}),
    )
    .await
    .unwrap_err();
    let zh_notices = notifier.take();
    assert_eq!(zh_notices.len(), 1, "{zh_notices:?}");
    assert_eq!(
        zh_notices[0].title.as_deref(),
        Some("需要人工介入：Agent 权限不足")
    );
    assert!(zh_notices[0].message.contains("DUT-2"), "{zh_notices:?}");

    state.set_locale(duthub_mcp::i18n::McpLocale::EnUs);
    let _ = call_tool_json(
        &ro_client,
        "dut_control",
        json!({"dutId": "dut-2", "action": "flash_reboot"}),
    )
    .await
    .unwrap_err();
    let en_notices = notifier.take();
    assert_eq!(en_notices.len(), 1, "{en_notices:?}");
    assert_eq!(
        en_notices[0].title.as_deref(),
        Some("Human intervention: Agent permission denied")
    );

    ro_client.cancel().await.ok();
    mcp_client.cancel().await.ok();
    drop(http);
    drop(h2c);
    let stopped = tokio::task::spawn_blocking(move || {
        handle.shutdown_and_join_with_timeout(Duration::from_secs(3))
    })
    .await
    .unwrap();
    assert!(stopped);
}
