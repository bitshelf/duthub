//! W4a regressions: A2A endpoint view / token rotation / port validation /
//! DUT removal ordering / pending-cleanup journal / export secret scan /
//! OS isolation badge / MCP bind-failure recording.
//!
//! Everything here drives the lib target (`duthub_desktop`) only — no GPUI.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use duthub_a2a::auth::InMemoryTokenSource;
use duthub_a2a::listener::{A2aDutConfig, A2aRuntime, A2aStatus};
use duthub_core::A2aPeer;
use duthub_core::model::DutA2aConfig;
use duthub_desktop::agent_ui::{
    CleanupStep, DutCleanupOps, PendingCleanupEntry, SandboxBadgeState, SandboxBoundaryProof,
    a2a_endpoint_view, a2a_export_payload, append_pending_cleanup, delete_agent_dir_idempotent,
    effective_trust_label, load_pending_cleanup, reserved_ports_from, run_remove_dut,
    sandbox_badge_state, sandbox_boundary_proof, validate_a2a_port,
};
use duthub_sandbox::{SandboxBackendKind, SandboxStatus};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duthub-agent-ui-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// 1. A2A status → card URL (loopback only, Running only)
// ---------------------------------------------------------------------------

#[test]
fn a2a_status_running_shows_only_loopback_url() {
    let config = DutA2aConfig {
        enabled: true,
        port: 8790,
        agent_name: "bench".into(),
        bind_addr: None,
    };
    let view = a2a_endpoint_view(
        &config,
        &A2aStatus::Running {
            endpoint: "http://127.0.0.1:8790".into(),
        },
    );
    assert_eq!(
        view.card_url.as_deref(),
        Some("http://127.0.0.1:8790/.well-known/agent-card.json"),
        "A running listener must expose only its loopback Agent Card URL"
    );
    assert!(view.card_url.unwrap().starts_with("http://127.0.0.1:"));
    assert!(!view.failed);

    // Any non-running state yields no URL at all — there is no LAN URL field
    // to fill, so the loopback URL is the only URL the card can advertise.
    for status in [
        A2aStatus::Disabled,
        A2aStatus::PortConflict,
        A2aStatus::Failed {
            reason: "token missing".into(),
        },
    ] {
        let view = a2a_endpoint_view(&config, &status);
        assert!(view.card_url.is_none(), "{status:?} must not expose a URL");
    }
    let failed = a2a_endpoint_view(
        &config,
        &A2aStatus::Failed {
            reason: "A2A token 未配置".into(),
        },
    );
    assert!(failed.failed, "A failed listener must be marked as failed");
    assert!(failed.status_text.contains("A2A token 未配置"));
}

// ---------------------------------------------------------------------------
// 2. Token rotation invalidates the old bearer token on a live listener
// ---------------------------------------------------------------------------

/// Minimal HTTP/1.1 client for the loopback A2A surface.
fn post_jsonrpc(port: u16, bearer: Option<&str>, body: &str) -> Result<String, String> {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).map_err(|error| format!("connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let mut request = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(bearer) = bearer {
        request.push_str("Authorization: Bearer ");
        request.push_str(bearer);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("read: {error}"))?;
    Ok(response)
}

/// Grab an ephemeral port by binding and dropping a listener (small race, so
/// callers retry).
fn ephemeral_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread")]
async fn a2a_token_rotation_invalidates_old_token() {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"message/send","params":{}}"#;
    let state: Arc<dyn duthub_mcp::state::McpState> =
        Arc::new(duthub_mcp::snapshot::SnapshotState::empty());

    let mut last_error = String::new();
    for _ in 0..5 {
        let port = ephemeral_port();
        let runtime = A2aRuntime::with_token_source(Arc::new(InMemoryTokenSource::new([(
            "dut-rot", "tok-A",
        )])));
        let statuses = runtime
            .start(
                vec![A2aDutConfig {
                    dut_id: "dut-rot".into(),
                    port,
                    bind_addr: None,
                    agent_name: "bench".into(),
                }],
                state.clone(),
                Vec::new(),
            )
            .await;
        if !matches!(
            statuses.first().map(|(_, status)| status),
            Some(A2aStatus::Running { .. })
        ) {
            last_error = format!("The listener never reached Running: {statuses:?}");
            continue;
        }
        let expected_ok = |response: Result<String, String>, what: &str| {
            let response = response.expect(what);
            assert!(
                !response.starts_with("HTTP/1.1 401"),
                "{what}: a valid token must not return 401: {response}"
            );
        };
        // Old token accepted before rotation.
        expected_ok(
            post_jsonrpc(port, Some("tok-A"), body),
            "rotation before: tok-A",
        );
        assert!(
            post_jsonrpc(port, Some("tok-B"), body)
                .unwrap()
                .starts_with("HTTP/1.1 401"),
            "Before rotation, tok-B must return 401"
        );

        // Rotate: persist through the token source and hot-swap the listener.
        runtime
            .set_token("dut-rot", "tok-B")
            .await
            .expect("set_token");

        let old = post_jsonrpc(port, Some("tok-A"), body).unwrap();
        assert!(
            old.starts_with("HTTP/1.1 401"),
            "The prior token must return 401 immediately after rotation: {old}"
        );
        expected_ok(
            post_jsonrpc(port, Some("tok-B"), body),
            "rotation after: tok-B",
        );
        runtime.shutdown().await;
        return;
    }
    panic!("The A2A listener could not bind: {last_error}");
}

// ---------------------------------------------------------------------------
// 3. Port validation: 0 and reserved ports are rejected
// ---------------------------------------------------------------------------

#[test]
fn a2a_port_zero_and_reserved_rejected() {
    assert!(validate_a2a_port(0, &[]).is_err(), "Reject port zero");
    assert!(validate_a2a_port(0, &[8797]).is_err());
    let reserved_err = validate_a2a_port(8797, &[8797]).unwrap_err();
    assert!(reserved_err.contains("8797"));
    assert!(validate_a2a_port(9100, &[8797]).is_ok());

    // Reserved set derivation: MCP http port + listener ports + enabled DUT
    // A2A ports, deduplicated, order-stable.
    let settings = duthub_core::store::default_settings();
    let reserved = reserved_ports_from(&settings, [9000, 9000, 9001]);
    assert!(reserved.contains(&settings.agent_access.http_port));
    assert_eq!(
        reserved.iter().filter(|&&port| port == 9000).count(),
        1,
        "Remove duplicate ports"
    );
    assert!(reserved.contains(&9001));
}

// ---------------------------------------------------------------------------
// 4-6. Removal ordering via DutCleanupOps
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockOps {
    trace: Vec<&'static str>,
    fail_strip: bool,
    persist_error: Option<String>,
    token_error: Option<String>,
    dir_error: Option<String>,
    pending: Vec<(String, CleanupStep, String)>,
}

impl DutCleanupOps for MockOps {
    fn strip_mcp(&mut self, _dut_id: &str) -> bool {
        self.trace.push("strip_mcp");
        !self.fail_strip
    }

    fn persist_removal(&mut self, _dut_id: &str) -> Result<(), String> {
        self.trace.push("persist_removal");
        match self.persist_error.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn stop_and_join(&mut self, _dut_id: &str) {
        self.trace.push("stop_and_join");
    }

    fn delete_a2a_token(&mut self, _dut_id: &str) -> Result<(), String> {
        self.trace.push("delete_a2a_token");
        match self.token_error.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn delete_agent_dir(&mut self, _dut_id: &str) -> Result<(), String> {
        self.trace.push("delete_agent_dir");
        match self.dir_error.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn record_pending_cleanup(&mut self, dut_id: &str, step: CleanupStep, reason: &str) {
        self.pending
            .push((dut_id.to_owned(), step, reason.to_owned()));
    }
}

#[test]
fn remove_plan_runs_strip_before_persist_and_stops_before_dir_delete() {
    let mut ops = MockOps::default();
    let outcome = run_remove_dut(&mut ops, "dut-1").expect("complete cleanup successfully");
    assert_eq!(
        ops.trace,
        [
            "strip_mcp",
            "persist_removal",
            "stop_and_join",
            "delete_a2a_token",
            "delete_agent_dir"
        ],
        "Run the five cleanup stages in order"
    );
    let position = |step: &str| ops.trace.iter().position(|s| *s == step).unwrap();
    assert!(position("strip_mcp") < position("persist_removal"));
    assert!(position("stop_and_join") < position("delete_agent_dir"));
    assert_eq!(outcome.dut_id, "dut-1");
    assert!(outcome.complete, "Successful cleanup must have no warnings");
    assert_eq!(outcome.warnings, [] as [std::string::String; 0]);
    assert_eq!(
        outcome.steps,
        vec![
            CleanupStep::StripMcp,
            CleanupStep::PersistRemoval,
            CleanupStep::StopAndJoin,
            CleanupStep::DeleteA2aToken,
            CleanupStep::DeleteAgentDir,
        ]
    );
}

#[test]
fn remove_plan_persist_failure_stops_before_stop() {
    let mut ops = MockOps {
        persist_error: Some("duts.json 写盘失败".into()),
        ..Default::default()
    };
    let error = run_remove_dut(&mut ops, "dut-2").expect_err("return the persistence failure");
    assert!(error.contains("duts.json"));
    assert_eq!(
        ops.trace,
        ["strip_mcp", "persist_removal"],
        "A persistence failure must leave the DUT runtimes intact"
    );
    assert!(!ops.trace.contains(&"stop_and_join"));
}

#[test]
fn remove_plan_token_failure_writes_pending_cleanup() {
    let mut ops = MockOps {
        token_error: Some("keyring 拒绝访问".into()),
        dir_error: Some("目录被占用".into()),
        ..Default::default()
    };
    let outcome =
        run_remove_dut(&mut ops, "dut-3").expect("continue after a best-effort cleanup failure");
    assert!(
        !outcome.complete,
        "Token or directory cleanup failure must leave the operation incomplete"
    );
    assert_eq!(outcome.warnings.len(), 2);
    assert!(outcome.warnings[0].contains("A2A Token"));
    assert!(outcome.warnings[1].contains("Agent 目录"));
    // Both best-effort failures were journaled for the next startup retry.
    assert_eq!(
        ops.pending
            .iter()
            .map(|(_, step, _)| *step)
            .collect::<Vec<_>>(),
        vec![CleanupStep::DeleteA2aToken, CleanupStep::DeleteAgentDir]
    );
    assert!(
        ops.pending
            .iter()
            .all(|(dut, _, reason)| dut == "dut-3" && !reason.is_empty())
    );
    // The removal still ran to the end: the directory step was attempted.
    assert_eq!(ops.trace.last(), Some(&"delete_agent_dir"));
}

// ---------------------------------------------------------------------------
// 7. Agent dir deletion is idempotent when the directory is missing
// ---------------------------------------------------------------------------

#[test]
fn remove_dir_idempotent_when_missing() {
    let work = temp_dir("dir-missing");
    // Missing directory: Ok (NotFound is success), and it must NOT create it.
    delete_agent_dir_idempotent(&work, "dut-missing").unwrap();
    assert!(
        !work.join("agent").join("dut").exists(),
        "Deletion must not recreate the directory"
    );
    // Existing directory: deleted on the first call and Ok again afterwards.
    let key = duthub_core::store::agent_dut_key("dut-gone").unwrap();
    let dir = work.join("agent").join("dut").join(&key);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("marker"), b"x").unwrap();
    delete_agent_dir_idempotent(&work, "dut-gone").unwrap();
    assert!(!dir.exists());
    delete_agent_dir_idempotent(&work, "dut-gone").unwrap();
    // Blank DUT id cannot map to a directory path: explicit error, never Ok.
    assert!(delete_agent_dir_idempotent(&work, "").is_err());
}

// ---------------------------------------------------------------------------
// 8. Export payload secret scan (fail closed)
// ---------------------------------------------------------------------------

#[test]
fn export_contains_secret_blocks_write() {
    let clean = vec![A2aPeer {
        id: "peer-1".into(),
        name: "workbench peer".into(),
        card_url: "http://127.0.0.1:8790/.well-known/agent-card.json".into(),
    }];
    let payload = a2a_export_payload(&clean).expect("export peers without credentials");
    assert!(payload.is_array());
    assert_eq!(payload[0]["agentCardUrl"], clean[0].card_url);

    let smuggled = vec![A2aPeer {
        id: "peer-2".into(),
        name: "smuggler".into(),
        card_url: "https://remote.example/agent-card?token=supersecret-token-value".into(),
    }];
    let error = a2a_export_payload(&smuggled).expect_err("reject suspected credentials");
    assert!(error.contains("拒绝") || error.contains("fail closed"));
    // Keyed fields are refused too, not just values.
    let payload_with_key = serde_json::json!({"apiKey": "sk-1234567890"});
    assert!(duthub_core::keys::export_contains_secret(&payload_with_key));
}

// ---------------------------------------------------------------------------
// 9. OS isolation badge never claims os_sandboxed without a backend proof
// ---------------------------------------------------------------------------

#[test]
fn os_isolation_badge_never_says_os_sandboxed_when_probe_false() {
    let unavailable = SandboxStatus {
        backend: SandboxBackendKind::ProcessContainer,
        supported: false,
        reason: Some("PSEC 1.1 unavailable".into()),
        ..SandboxStatus::default()
    };
    assert_eq!(
        sandbox_boundary_proof(&unavailable),
        SandboxBoundaryProof::Unverified
    );
    assert_eq!(
        sandbox_badge_state(&unavailable),
        SandboxBadgeState::Unavailable {
            backend: "processcontainer",
            detail: "PSEC 1.1 unavailable".into(),
        }
    );

    let label = effective_trust_label("os_sandboxed", SandboxBoundaryProof::Unverified);
    assert!(
        label.contains("未验证"),
        "Without backend proof, the label must show an unverified state: {label}"
    );
    assert!(!label.contains("os_sandboxed"));
    assert!(
        !label.contains("OS 沙箱") && !label.contains("已验证"),
        "An unverified label must not claim an active OS sandbox: {label}"
    );

    // Partial backend (supported, but missing read_deny) must stay Unverified.
    let partial = SandboxStatus {
        backend: SandboxBackendKind::ProcessContainer,
        supported: true,
        read_deny: false,
        host_loopback: true,
        ui_isolation: true,
        ..SandboxStatus::default()
    };
    assert_eq!(
        sandbox_boundary_proof(&partial),
        SandboxBoundaryProof::Unverified
    );

    // Only an enforced MXC backend that verifies full Agent isolation upgrades
    // the proof to VerifiedByBackend.
    let enforced = SandboxStatus {
        backend: SandboxBackendKind::ProcessContainer,
        supported: true,
        read_deny: true,
        host_loopback: true,
        ui_isolation: true,
        capture_denials: true,
        ..SandboxStatus::default()
    };
    assert_eq!(
        sandbox_boundary_proof(&enforced),
        SandboxBoundaryProof::VerifiedByBackend
    );
    assert_eq!(
        sandbox_badge_state(&enforced),
        SandboxBadgeState::Ready {
            backend: "processcontainer",
            fs_deny: true,
            network: true,
            ui_isolation: true,
            denial_capture: true,
        }
    );
    let proven = effective_trust_label("os_sandboxed", SandboxBoundaryProof::VerifiedByBackend);
    assert_eq!(proven, "OS 沙箱（已验证）");
    assert!(
        !proven.contains("未验证"),
        "A verified label must not report an unverified state: {proven}"
    );

    // Other levels are unchanged.
    assert_eq!(
        effective_trust_label("verified_readonly", SandboxBoundaryProof::Unverified),
        "已验证只读"
    );
    assert_eq!(
        effective_trust_label("mcp_only", SandboxBoundaryProof::Unverified),
        "MCP 接入"
    );

    // Wiring (source contract): desktop UI derives the boundary proof from
    // `cached_sandbox_status()` -> `sandbox_boundary_proof(&sandbox_status)`.
    let pages = source_of("pages.rs");
    assert!(
        pages.contains("sandbox_boundary_proof(&sandbox_status)"),
        "Agent settings must derive isolation proof from the actual MXC status"
    );
    assert!(
        pages.contains("format_sandbox_status(&sandbox_status)"),
        "Agent settings must show localized MXC isolation status"
    );
    assert!(
        !pages.contains("SandboxBoundaryProof::Unverified"),
        "Agent settings must not hard-code an unverified placeholder"
    );
}

// ---------------------------------------------------------------------------
// 10. MCP bind failure is recorded and shown
// ---------------------------------------------------------------------------

/// W4a contract: MCP endpoint bind failures are no longer only logged via `eprintln!` — they are
/// recorded in `McpRuntimeHandles::failures` (with reasons), surfaced on the Agent access page and
/// connection-management MCP card via `failure_for_port` / `failures`, and shutdown still closes
/// handles.
///
/// `spawn_mcp_runtime` / `McpRuntimeHandles` are pinned in the bin target by `plan_badges.rs`
/// (lib tests cannot instantiate bin modules), so this follows the same source-contract pattern.
#[test]
fn mcp_bind_failure_recorded_and_shown() {
    let runtime = source_of("mcp_runtime.rs");
    assert!(
        runtime.contains("pub failures: Vec<(u16, String)>"),
        "McpRuntimeHandles must report bind failures"
    );
    assert!(
        runtime.contains("failure_for_port"),
        "Expose the failure reason by listener port"
    );
    assert!(
        runtime.contains("绑定 {bind_addr} 失败") && runtime.contains("启动 HTTP 服务失败"),
        "Bind and startup failures must record a readable reason"
    );

    let pages = source_of("pages.rs");
    assert!(
        pages.contains("failure_for_port") && pages.contains("w.mcp_runtime.failures"),
        "Agent access and the MCP card must surface listener failures"
    );

    let workspace = source_of("workspace.rs");
    assert!(
        workspace.contains("impl Drop for Workbench")
            && workspace.contains("mcp_runtime.shutdown()"),
        "Shutdown must close listener handles"
    );

    // Real kernel behavior evidence: binding the same loopback port twice must fail —
    // spawn_mcp_runtime records this exact condition rather than fabricating an error.
    let holder = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let held = holder.local_addr().unwrap().port();
    let second = std::net::TcpListener::bind(("127.0.0.1", held));
    assert!(
        second.is_err(),
        "A second bind must fail while port {held} is in use"
    );
}

// ---------------------------------------------------------------------------
// pending-cleanup journal roundtrip (startup retry source of truth)
// ---------------------------------------------------------------------------

#[test]
fn pending_cleanup_journal_roundtrip_and_retry() {
    let work = temp_dir("pending-cleanup");
    assert!(
        load_pending_cleanup(&work).is_empty(),
        "A missing file must return an empty list"
    );

    append_pending_cleanup(&work, "dut-x", CleanupStep::DeleteA2aToken, "keyring 拒绝").unwrap();
    append_pending_cleanup(&work, "dut-x", CleanupStep::DeleteA2aToken, "重复必须去重").unwrap();
    append_pending_cleanup(&work, "dut-x", CleanupStep::DeleteAgentDir, "目录占用").unwrap();
    let entries = load_pending_cleanup(&work);
    assert_eq!(
        entries.len(),
        2,
        "Deduplicate identical steps for the same DUT"
    );
    let PendingCleanupEntry { dut_id, step, .. } = &entries[0];
    assert_eq!(dut_id, "dut-x");
    assert_eq!(step, "delete_a2a_token");

    // Retry: the DUT never existed on this machine, so both idempotent steps
    // succeed and the journal empties out.
    let (completed, failures) = duthub_desktop::agent_ui::retry_pending_cleanup(&work);
    assert_eq!(
        completed, 2,
        "Successful retries must clear their entries: {failures:?}"
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        load_pending_cleanup(&work),
        [] as [duthub_desktop::agent_ui::PendingCleanupEntry; 0]
    );
}

// ---------------------------------------------------------------------------
// W4b: registry editor draft gate (args JSON / env keys / program path)
// ---------------------------------------------------------------------------

/// Read one bin-source file for source-contract assertions (same pattern as
/// `mcp_bind_failure_recorded_and_shown`).
fn source_of(name: &str) -> String {
    let src_root =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("src");
    let path = src_root.join(name);
    if path.is_file() {
        return std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read src/{name}: {error}"));
    }
    if let Some(stem) = name.strip_suffix(".rs") {
        let dir = src_root.join(stem);
        if dir.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "rs"))
                .collect();
            files.sort_by(|a, b| {
                let a_mod = a.file_name().is_some_and(|n| n == "mod.rs");
                let b_mod = b.file_name().is_some_and(|n| n == "mod.rs");
                b_mod.cmp(&a_mod).then_with(|| a.cmp(b))
            });
            return files
                .into_iter()
                .map(|p| {
                    std::fs::read_to_string(&p)
                        .unwrap_or_else(|error| panic!("read {}: {error}", p.display()))
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    panic!("missing source src/{name}");
}

#[test]
fn args_json_roundtrip_preserves_quoted_argument() {
    use duthub_desktop::agent_ui::{args_to_json, parse_args_json};
    // Arguments with spaces, embedded quotes and drive paths must survive the
    // editor round-trip byte for byte (the old whitespace split destroyed them).
    let args = vec![
        "--profile".to_owned(),
        "my agent".to_owned(),
        "--say".to_owned(),
        "\"hello world\"".to_owned(),
        "--cwd".to_owned(),
        "C:/Program Files/duthub".to_owned(),
    ];
    let raw = args_to_json(&args);
    assert_eq!(
        parse_args_json(&raw).unwrap(),
        args,
        "Preserve spaces and quotes in arguments across a JSON round trip: {raw}"
    );
    // Empty draft = no args; non-array JSON and non-string elements are refused.
    assert_eq!(parse_args_json("").unwrap(), Vec::<String>::new());
    assert_eq!(parse_args_json("   \n").unwrap(), Vec::<String>::new());
    assert!(
        parse_args_json("--acp --stdio").is_err(),
        "Reject an argument draft that is not a JSON array"
    );
    assert!(
        parse_args_json("[1, 2]").is_err(),
        "Reject non-string elements"
    );
    assert!(
        parse_args_json("{\"a\":1}").is_err(),
        "Reject an object in place of an array"
    );
    assert!(
        parse_args_json("[\"--acp\",").is_err(),
        "Reject truncated JSON"
    );
}

#[test]
fn args_invalid_json_does_not_save() {
    use duthub_desktop::agent_ui::validate_agent_editor_draft;
    // The save gate refuses malformed args before anything reaches agents.json.
    let error = validate_agent_editor_draft("C:/tools/acme.exe", "[\"--acp\",", "ACME_API_KEY")
        .expect_err("Reject truncated JSON");
    assert!(error.contains("JSON"), "{error}");
    // A valid draft parses args/keys and keeps the absolute program.
    let draft = validate_agent_editor_draft(
        "C:/tools/acme.exe",
        "[\"--acp\", \"--stdio\"]",
        "ACME_API_KEY",
    )
    .unwrap();
    assert_eq!(draft.program.as_deref(), Some("C:/tools/acme.exe"));
    assert_eq!(draft.args, ["--acp", "--stdio"]);
    assert_eq!(draft.env_keys, ["ACME_API_KEY"]);
    // Embedded program (goose): empty program is accepted without path checks.
    let goose = validate_agent_editor_draft("", "[]", "").unwrap();
    assert_eq!(goose.program, None);
    // Source contract: the bin's save path delegates to this gate and returns
    // early on failure, so an invalid draft cannot be persisted.
    let workspace = source_of("workspace.rs");
    assert!(
        workspace.contains("duthub_desktop::agent_ui::validate_agent_editor_draft("),
        "Registry saves must use the shared draft validator"
    );
    assert!(
        workspace.contains("invalid drafts never reach agents.json"),
        "Reject invalid drafts before writing the registry"
    );
    assert!(
        !workspace.contains("split_whitespace()"),
        "Argument parsing must preserve quoted and space-containing arguments"
    );
}

#[test]
fn reserved_env_key_rejected() {
    use duthub_desktop::agent_ui::{validate_agent_editor_draft, validate_env_keys};
    // Sandbox/isolation variables are duthub-owned; core rejects them
    // case-insensitively and the editor gate reuses that rule.
    for key in [
        "PATH",
        "path",
        "Temp",
        "TEMP",
        "GOOSE_MODE",
        "USERPROFILE",
        "SystemRoot",
    ] {
        let error = validate_env_keys(&[key.to_owned()]).expect_err("reject reserved variables");
        assert!(error.contains("保留"), "{key}: {error}");
        let error =
            validate_agent_editor_draft("C:/tools/acme.exe", "[]", &format!("ACME_API_KEY, {key}"))
                .expect_err("reject reserved variables in the combined draft");
        assert!(error.contains("保留"), "{key}: {error}");
    }
    // Legal keys pass; duplicates are refused.
    assert!(validate_env_keys(&["ACME_API_KEY".to_owned(), "ACME_ENDPOINT".to_owned()]).is_ok());
    let duplicate = validate_env_keys(&["DUP_KEY".to_owned(), "DUP_KEY".to_owned()])
        .expect_err("reject duplicate keys");
    assert!(duplicate.contains("重复"), "{duplicate}");
    // Key naming rules still apply through the gate.
    assert!(validate_env_keys(&["1BAD".to_owned()]).is_err());
    assert!(validate_env_keys(&["WITH-DASH".to_owned()]).is_err());
}

#[test]
fn external_program_relative_rejected() {
    use duthub_desktop::agent_ui::{validate_agent_editor_draft, validate_external_program_path};
    // The editor only accepts local absolute executable paths.
    for path in [
        "acme.exe",
        "tools/acme.exe",
        "./acme.exe",
        "../acme.exe",
        "C:acme.exe",
        r"\\server\share\acme.exe",
        "",
        "   ",
    ] {
        assert!(validate_external_program_path(path).is_err(), "{path}");
    }
    for path in ["C:/tools/acme.exe", r"c:\tools\acme.exe"] {
        assert!(validate_external_program_path(path).is_ok(), "{path}");
    }
    let error = validate_agent_editor_draft("tools/acme.exe", "[]", "")
        .expect_err("reject relative program paths");
    assert!(error.contains("绝对路径"), "{error}");
    // Empty program = embedded (goose) and skips path validation entirely.
    assert!(validate_agent_editor_draft("", "[]", "").is_ok());
}

// ---------------------------------------------------------------------------
// W4b: agent deletion — keyring cleanup with a pending-cleanup journal
// ---------------------------------------------------------------------------

#[test]
fn delete_agent_removes_keyring_entries_and_reports_failures() {
    use duthub_core::agent_config::AgentDefinition;
    use duthub_desktop::agent_ui::{
        append_agent_env_pending_cleanup, delete_agent_env_entries, delete_agent_env_entries_with,
        load_pending_cleanup,
    };
    use std::cell::RefCell;
    // Every key is attempted; failures are collected, not fatal.
    let removed = RefCell::new(Vec::new());
    let failing = ["ACME_BROKEN".to_owned()];
    let delete = |key: &str| -> Result<(), String> {
        removed.borrow_mut().push(key.to_owned());
        if failing.contains(&key.to_owned()) {
            Err("keyring 拒绝访问".into())
        } else {
            Ok(())
        }
    };
    let agent = AgentDefinition {
        id: "acme".into(),
        env_keys: vec!["ACME_OK".into(), "ACME_BROKEN".into()],
        ..AgentDefinition::goose()
    };
    let pending = delete_agent_env_entries_with(&agent.id, &agent.env_keys, delete).unwrap();
    assert_eq!(
        removed.borrow().as_slice(),
        ["ACME_OK", "ACME_BROKEN"],
        "A deletion failure must not stop cleanup of the remaining keys"
    );
    assert_eq!(
        pending,
        ["ACME_BROKEN"],
        "Failed keys must remain in the pending cleanup list"
    );
    // All-success path and the invalid-id guard.
    assert_eq!(
        delete_agent_env_entries_with("acme", &["ACME_OK".to_owned()], |_| Ok(())).unwrap(),
        [] as [std::string::String; 0]
    );
    assert!(delete_agent_env_entries_with("  ", &[], |_| Ok(())).is_err());
    // Real entry naming goes through core (`acp/<id>/env/<KEY>`); the
    // injectable closure receives the bare key.
    let env_entry = duthub_core::agent_config::agent_env_entry("acme", "ACME_OK");
    assert_eq!(env_entry, "acp/acme/env/ACME_OK");
    let empty = AgentDefinition {
        id: "acme".into(),
        env_keys: Vec::new(),
        ..AgentDefinition::goose()
    };
    assert_eq!(
        delete_agent_env_entries(&empty).unwrap(),
        [] as [std::string::String; 0]
    );
    // Failed keys are journalled per agent (merged, never duplicated rows)
    // for the next-startup retry.
    let work = temp_dir("agent-env-cleanup");
    append_agent_env_pending_cleanup(&work, "acme", &pending, "keyring 拒绝访问").unwrap();
    append_agent_env_pending_cleanup(&work, "acme", &["ACME_EXTRA".to_owned()], "补充").unwrap();
    append_agent_env_pending_cleanup(&work, "goose", &["G_KEY".to_owned()], "其他 Agent").unwrap();
    let entries = load_pending_cleanup(&work);
    assert_eq!(
        entries.len(),
        2,
        "Merge equivalent cleanup steps for the same Agent"
    );
    let acme = entries
        .iter()
        .find(|entry| entry.dut_id == "acme")
        .expect("find the acme entry");
    assert_eq!(acme.step, "delete_agent_env_keys");
    assert_eq!(acme.env_keys, ["ACME_BROKEN", "ACME_EXTRA"]);
    assert!(acme.reason.contains("keyring"));
    let _ = std::fs::remove_dir_all(&work);
}

// ---------------------------------------------------------------------------
// W4b: start_entry product path (CLI routing + binding gating)
// ---------------------------------------------------------------------------

#[test]
fn start_agent_session_uses_start_entry_for_cli() {
    let workspace = source_of("workspace.rs");
    // The product start path goes through start_entry: it routes CLI profiles
    // to the per-turn CLI worker and resolves ACP bindings itself.
    assert!(
        workspace.contains("host\n                .start_entry(dut_id, binding, agent)")
            || workspace.contains("host.start_entry(dut_id, binding, agent)"),
        "start_agent_session must call host.start_entry (which routes CLI profiles)"
    );
    assert!(
        !workspace.contains("start_with_agent"),
        "product path must not call start_with_agent directly (bypasses CLI routing and binding gate)"
    );
    assert!(
        workspace.contains("agent.kind == duthub_core::agent_config::AgentKind::Acp"),
        "model binding is only for goose ACP; CLI entries do not enter the binding branch"
    );
}

#[test]
fn start_agent_session_never_passes_model_binding_to_non_goose() {
    let workspace = source_of("workspace.rs");
    // FromSettings is gated on goose; everything else gets None.
    assert!(
        workspace.contains("agent.is_goose()"),
        "binding source must be gated on agent.is_goose()"
    );
    assert!(
        workspace.contains("BindingSource::FromSettings(&settings)"),
        "goose resolves model binding from settings"
    );
    assert!(
        workspace.contains("BindingSource::None"),
        "non-goose product path must pass BindingSource::None (never forward model tokens)"
    );
    // host.rs is owned by another wave; read-only cross-check that
    // start_entry itself drops bindings for non-goose FromSettings sources.
    let host = source_of("agent_host/host.rs");
    assert!(
        host.contains("BindingSource::FromSettings(settings) if agent.is_goose()"),
        "host: FromSettings applies only to goose"
    );
    assert!(
        host.contains("BindingSource::FromSettings(_) | BindingSource::None => None"),
        "host: non-goose binding sources must be dropped"
    );
}

#[test]
fn running_selector_disabled_and_shows_actual_launch() {
    use duthub_desktop::agent_host::{AgentLaunchSpec, SessionStatus};
    use duthub_desktop::agent_ui::{running_agent_selector_disabled, running_launch_line};
    // Live (non-terminal) states lock the per-DUT Agent dropdown.
    for status in [
        SessionStatus::Starting,
        SessionStatus::Ready,
        SessionStatus::Busy,
        SessionStatus::Stopping,
    ] {
        assert!(
            running_agent_selector_disabled(&status),
            "{status:?} must lock the agent selector"
        );
    }
    for status in [SessionStatus::Stopped, SessionStatus::Failed("x".into())] {
        assert!(
            !running_agent_selector_disabled(&status),
            "{status:?} is terminal and must allow changing selection"
        );
    }
    // The running panel shows the actually launched agent id, never the
    // pending dropdown selection.
    let spec = AgentLaunchSpec {
        agent_id: "acme".into(),
        agent_name: "Acme ACP".into(),
        program: PathBuf::from("C:/tools/acme.exe"),
        args: Vec::new(),
        env: Vec::new(),
        embedded: false,
        sha256: "a".repeat(64),
        version: None,
        signature: "NotSigned".into(),
    };
    let line = running_launch_line(Some(&spec));
    assert!(line.contains("acme"), "{line}");
    assert!(line.contains("C:/tools/acme.exe"), "{line}");
    let starting = running_launch_line(None);
    assert!(starting.contains("启动"), "{starting}");
    // UI wiring (source contract): the panel locks the selector, renders the
    // actual launch line and the next-start hint.
    let pages = source_of("pages.rs");
    assert!(
        pages.contains("running_agent_selector_disabled(&status)"),
        "session panel must lock selector using running status"
    );
    assert!(
        pages.contains("running_launch_line("),
        "running panel must show actual launch line"
    );
    assert!(
        pages.contains("agentSessionSelectorNextStart"),
        "must display next-start hint for selector changes"
    );
}

// ---------------------------------------------------------------------------
// W4b: registry toggle guard
// ---------------------------------------------------------------------------

#[test]
fn agent_toggle_forbids_disabling_default_goose_and_last_enabled() {
    use duthub_core::agent_config::{AgentDefinition, AgentKind};
    use duthub_desktop::agent_ui::validate_agent_toggle;
    let mut file = duthub_core::agent_config::default_agents();
    // Default goose can never be disabled.
    let error =
        validate_agent_toggle(&file, "goose").expect_err("default goose must refuse disabling");
    assert!(error.contains("goose"), "{error}");
    // Enabling is always allowed.
    file.goose_mut().unwrap().enabled = false;
    assert!(validate_agent_toggle(&file, "goose").is_ok());
    file.goose_mut().unwrap().enabled = true;
    // With goose enabled, disabling another entry is fine...
    file.agents.push(AgentDefinition {
        id: "acme".into(),
        name: "Acme".into(),
        kind: AgentKind::Acp,
        program: Some("C:/tools/acme.exe".into()),
        ..AgentDefinition::goose()
    });
    assert!(validate_agent_toggle(&file, "acme").is_ok());
    // ...but the last enabled entry cannot be disabled (goose disabled case).
    file.goose_mut().unwrap().enabled = false;
    let error =
        validate_agent_toggle(&file, "acme").expect_err("last enabled entry must refuse disabling");
    assert!(error.contains("最后一个启用"), "{error}");
    // Unknown ids are refused either way.
    assert!(validate_agent_toggle(&file, "missing").is_err());
}

// ---------------------------------------------------------------------------
// W4c: prompt clear is value-matched and keeps the draft on enqueue failure
// ---------------------------------------------------------------------------

#[test]
fn prompt_clear_only_if_value_matches() {
    use duthub_desktop::agent_ui::should_clear_prompt;
    // Exact match clears (the deferred clear wipes the sent text).
    assert!(should_clear_prompt("collect boot log", "collect boot log"));
    // Anything typed between enqueue and the deferred clear must survive:
    // a suffix append, an edit, and a cleared box all refuse the wipe.
    assert!(!should_clear_prompt(
        "collect boot log now",
        "collect boot log"
    ));
    assert!(!should_clear_prompt("different text", "collect boot log"));
    assert!(!should_clear_prompt("", "collect boot log"));
    // Trailing whitespace in the editor is not the sent (trimmed) text either.
    assert!(!should_clear_prompt(
        "collect boot log ",
        "collect boot log"
    ));

    // Wiring (source contract): the deferred clear consults the matcher and
    // the pending requests are keyed per DUT, so switching tabs cannot drop
    // another DUT's request or clear the wrong editor.
    let workspace = source_of("workspace.rs");
    assert!(
        workspace.contains("duthub_desktop::agent_ui::should_clear_prompt(&current, &sent)"),
        "deferred render-frame clear must use value-matched check"
    );
    assert!(
        workspace.contains("agent_prompt_clear: HashMap<String, (String, u64)>"),
        "pending clear requests must be keyed per DUT (not a global Option)"
    );
    assert!(
        workspace.contains("self.agent_prompt_clear.remove(dut_id);"),
        "removing a DUT must also clear its pending prompt-clear request"
    );
}

#[test]
fn prompt_clear_keeps_draft_on_enqueue_failure() {
    use duthub_desktop::agent_ui::{PromptSendOutcome, prompt_send_outcome};
    // Host accepted: the editor may be cleared (deferred + value-matched).
    assert_eq!(
        prompt_send_outcome(Ok::<(), &str>(())),
        PromptSendOutcome::Enqueued
    );
    // Host refused: the draft is kept, the refusal reason is carried back as
    // data and the attempted text lands in the transcript so the failure is
    // visible and retryable.
    let rejected = prompt_send_outcome(Err::<(), &str>("会话忙（Busy）"));
    let PromptSendOutcome::Rejected { reason } = rejected else {
        panic!("A failed enqueue must return Rejected");
    };
    assert!(reason.contains("会话忙（Busy）"), "{reason}");

    // Wiring (source contract): the rejected path pushes a transcript entry
    // (prompt + reason) and never inserts a pending-clear request.
    let workspace = source_of("workspace.rs");
    assert!(
        workspace
            .contains("duthub_desktop::agent_ui::prompt_send_outcome(host.prompt(dut_id, &text))"),
        "send path must route through pure prompt_send_outcome helper"
    );
    assert!(
        workspace.contains("PromptSendOutcome::Rejected { reason }"),
        "rejected branch must write attempted text and reason into transcript"
    );
    assert!(
        workspace.contains("\"agentPromptSendFailed\""),
        "rejected transcript line must use localized agentPromptSendFailed copy"
    );
}

// ---------------------------------------------------------------------------
// W4c: transcript follow survives the 2000-entry memory cap
// ---------------------------------------------------------------------------

#[test]
fn should_follow_after_2000_entries() {
    use duthub_desktop::agent_host::{SessionView, TranscriptEntry};
    use duthub_desktop::agent_ui::should_follow;
    // Basic rule: follow only when new entries arrived since the last frame.
    assert!(should_follow(0, 1));
    assert!(!should_follow(5, 5));
    assert!(
        !should_follow(7, 5),
        "A sequence reset must not trigger automatic scrolling"
    );

    // The point of the monotonic counter: the in-memory transcript is capped
    // at 2000, so after the cap `transcript.len()` stops growing. The old
    // length-based check silently stopped following; total_entries keeps
    // counting dropped entries.
    let view = SessionView::new();
    for index in 0..2001 {
        view.push_entry(TranscriptEntry::system(format!("entry-{index}")));
    }
    assert_eq!(view.transcript_snapshot().len(), 2000);
    assert_eq!(view.total_entries(), 2001);
    assert!(
        should_follow(2000, view.total_entries()),
        "New entries must still trigger scrolling after the 2,000-entry limit"
    );
    assert!(!should_follow(2001, view.total_entries()));

    // Wiring (source contract): the panel feeds the monotonic counter into
    // the follow check and stores it as u64.
    let pages = source_of("pages.rs");
    assert!(
        pages.contains("duthub_desktop::agent_ui::should_follow(seen, total_entries)"),
        "transcript auto-follow must use monotonic total_entries counter"
    );
    let workspace = source_of("workspace.rs");
    assert!(
        workspace.contains("pub(crate) agent_transcript_seen: HashMap<String, u64>"),
        "seen sequence counter must be stored as u64"
    );
}

// ---------------------------------------------------------------------------
// W4c: transcript tag uses the current agent (launch, then registry name)
// ---------------------------------------------------------------------------

#[test]
fn transcript_tag_uses_selected_agent() {
    use duthub_desktop::agent_host::AgentLaunchSpec;
    use duthub_desktop::agent_ui::transcript_agent_tag;
    let launch = AgentLaunchSpec {
        agent_id: "goose".into(),
        agent_name: "Goose ACP".into(),
        program: PathBuf::from("embedded"),
        args: Vec::new(),
        env: Vec::new(),
        embedded: true,
        sha256: "a".repeat(64),
        version: Some("1.60.2".into()),
        signature: "NotSigned".into(),
    };
    // The running process wins: the dropdown selection is never echoed for a
    // live session.
    assert_eq!(
        transcript_agent_tag(Some(&launch), Some("Acme ACP")),
        "Goose ACP"
    );
    // Before launch the registry name of the per-DUT selection is used —
    // never the old hardcoded "GOOSE".
    assert_eq!(transcript_agent_tag(None, Some("Acme ACP")), "Acme ACP");
    // Degenerate inputs never produce an empty tag.
    assert_eq!(transcript_agent_tag(None, None), "AGENT");
    assert_eq!(transcript_agent_tag(None, Some("  ")), "AGENT");
    let nameless = AgentLaunchSpec {
        agent_name: "  ".into(),
        ..launch.clone()
    };
    assert_eq!(transcript_agent_tag(Some(&nameless), None), "goose");

    // Wiring (source contract): rows get the tag through the workbench helper
    // and the hardcoded GOOSE label is gone.
    let pages = source_of("pages.rs");
    assert!(
        pages.contains("agent_transcript_row(entry, &transcript_tag, p)"),
        "transcript row must carry current agent tag"
    );
    assert!(
        !pages.contains("(\"GOOSE\", p.text)"),
        "agent rows must not hardcode GOOSE tag"
    );
    assert!(
        pages.contains("w.transcript_agent_tag(&dut.id)"),
        "tag must come from workbench launch/registry fallback helper"
    );
}

// ---------------------------------------------------------------------------
// W4c: Authenticode badge — external programs are "unverified", not "unsigned"
// ---------------------------------------------------------------------------

#[test]
fn external_badge_is_unverified_not_unsigned() {
    use duthub_desktop::agent_host::AgentLaunchSpec;
    use duthub_desktop::agent_ui::{
        AuthenticodeBadge, ProgramSource, authenticode_badge, program_source,
    };
    // External programs never get a signature-chain check: the badge is the
    // unverified variant, whose localized text says "Unverified" and must not
    // claim the *absence* of a signature was proven.
    assert_eq!(
        authenticode_badge(ProgramSource::External, "NotSigned"),
        AuthenticodeBadge::ExternalUnverified
    );
    let locales = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("locales");
    for name in ["zh-CN.json", "en-US.json"] {
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(locales.join(name)).unwrap()).unwrap();
        let text = catalog["agentProgramUnverified"].as_str().expect("key");
        assert!(
            text.contains("未验证") || text.contains("Unverified"),
            "{name}: {text}"
        );
        assert!(!text.contains("未签名"), "{name}: {text}");
    }
    // The embedded goose keeps the manifest-recorded status verbatim.
    assert_eq!(
        authenticode_badge(ProgramSource::Embedded, "NotSigned"),
        AuthenticodeBadge::EmbeddedNotSigned
    );
    assert_eq!(
        authenticode_badge(ProgramSource::Embedded, "Valid: Acme CA"),
        AuthenticodeBadge::EmbeddedStatus("Valid: Acme CA".into())
    );

    // Source resolution: the launch spec wins; before launch nothing is
    // verified either (goose is downloaded, not embedded), so both pre-launch
    // branches project to ExternalUnverified.
    let spec = AgentLaunchSpec {
        agent_id: "acme".into(),
        agent_name: "Acme ACP".into(),
        program: PathBuf::from("C:/tools/acme.exe"),
        args: Vec::new(),
        env: Vec::new(),
        embedded: false,
        sha256: "a".repeat(64),
        version: None,
        signature: "NotSigned".into(),
    };
    assert_eq!(program_source(Some(&spec), false), ProgramSource::External);
    let embedded_spec = AgentLaunchSpec {
        embedded: true,
        ..spec.clone()
    };
    assert_eq!(
        program_source(Some(&embedded_spec), false),
        ProgramSource::Embedded
    );
    assert_eq!(program_source(None, true), ProgramSource::External);
    assert_eq!(
        program_source(None, false),
        ProgramSource::External,
        "must not claim embedded goose when not running and no external path configured"
    );
    assert_eq!(
        authenticode_badge(program_source(None, false), "NotSigned"),
        AuthenticodeBadge::ExternalUnverified,
        "downloaded/PATH goose has no signature-chain check; badge must be ExternalUnverified"
    );

    // Wiring (source contract): the badge variant lives in agent_ui and both
    // badge call sites route through it.
    let pages = source_of("pages.rs");
    assert!(
        pages.contains("duthub_desktop::agent_ui::authenticode_badge(source, signature)"),
        "badge variant must come from agent_ui::authenticode_badge"
    );
    assert!(
        !pages.contains("let status = bundle::authenticode_status();"),
        "session badge must no longer read legacy embedded manifest status"
    );
}

// ---------------------------------------------------------------------------
// W4c: Inspector audit filter + permission-denial entries
// ---------------------------------------------------------------------------

#[test]
fn inspector_filters_audit_by_credential() {
    use duthub_core::audit::{AuditEvent, AuditOutcome};
    use duthub_desktop::agent_host::{TranscriptEntry, TranscriptKind};
    use duthub_desktop::agent_ui::{inspector_audit_events, permission_denial_entries};
    let event = |action: &str, credential: Option<&str>, result: AuditOutcome| AuditEvent {
        ts_unix_ms: 1,
        action: action.to_owned(),
        agent: Some("goose".into()),
        credential_id: credential.map(str::to_owned),
        dut: Some("dut-1".into()),
        result,
        detail: Some("tool=serial_read duration_ms=5".into()),
    };
    let recent = vec![
        event("tool.call", Some("cred-1"), AuditOutcome::Ok),
        event("tool.call", Some("cred-2"), AuditOutcome::Ok),
        event("tool.call", None, AuditOutcome::Denied),
        event("lease.grant", Some("cred-1"), AuditOutcome::Ok),
        event("tool.call", Some("cred-1"), AuditOutcome::Denied),
    ];
    let filtered = inspector_audit_events(&recent, Some("cred-1"));
    assert_eq!(
        filtered.len(),
        2,
        "keep only tool.call events for this session credential"
    );
    assert!(filtered.iter().all(
        |event| event.credential_id.as_deref() == Some("cred-1") && event.action == "tool.call"
    ));
    // A session without a credential never inherits unattributed events.
    assert_eq!(
        inspector_audit_events(&recent, None),
        [] as [duthub_core::AuditEvent; 0]
    );
    assert_eq!(
        inspector_audit_events(&[], Some("cred-1")),
        [] as [duthub_core::AuditEvent; 0]
    );

    // Permission-denial evidence comes from the transcript System entries the
    // worker writes for AcpEvent::PermissionDenied.
    let entries = vec![
        TranscriptEntry::system("本轮结束（end_turn）"),
        TranscriptEntry::system("已拒绝权限请求：write file"),
        TranscriptEntry::error("已拒绝权限请求：伪装成系统条目的错误行"),
    ];
    let denials = permission_denial_entries(&entries);
    assert_eq!(denials, ["已拒绝权限请求：write file"]);
    let _ = TranscriptKind::System;

    // Wiring (source contract): the Inspector reads the real session view and
    // the demo tool cards are gone; the side panel no longer mounts the demo
    // pane.
    let pages = source_of("pages.rs");
    assert!(
        pages.contains("duthub_desktop::agent_ui::inspector_audit_events("),
        "Inspector must filter audit events through pure helper"
    );
    assert!(
        pages.contains("sink.recent(200)"),
        "audit window must query recent 200 entries"
    );
    assert!(
        !pages.contains("\"AGENT ROLES\"") && !pages.contains("grep · read · git · find · make"),
        "Inspector must not display demo tool cards"
    );
    let side_panel = source_of("side_panel.rs");
    assert!(
        !side_panel.contains("crate::agent::render_agent_pane"),
        "side panel must no longer mount demo Agent pane"
    );
    assert!(
        side_panel.contains("agent_inspector_panel"),
        "Panel::Agent must render real Inspector"
    );
}

// ---------------------------------------------------------------------------
// W4c: --self-test purity (in-memory registry, no listeners, no agent dir)
// ---------------------------------------------------------------------------

#[test]
fn self_test_stays_pure_no_agents_json_no_listeners() {
    let workspace = source_of("workspace.rs");
    // The registry only exists in memory during self-test: load_agents()
    // bootstraps agents.json on a missing file, so it must be bypassed.
    assert!(
        workspace.contains("let (agents, mut agents_error) = if self_test {"),
        "self-test must use in-memory default_agents() branch"
    );
    assert!(
        workspace.contains("wb.mcp_runtime = crate::mcp_runtime::spawn_mcp_runtime(")
            && workspace.contains("if !self_test {"),
        "self-test must not start MCP HTTP listeners"
    );
    assert!(
        workspace.contains("let a2a = if self_test {"),
        "self-test must not construct A2A runtime (no listener on 8790)"
    );
    assert!(
        workspace.contains("pub a2a: Option<crate::a2a_runtime::A2aRuntime>"),
        "A2A runtime must be Option (None in self-test)"
    );
    assert!(
        workspace.contains("if !self_test {\n            let (pending_done, pending_failures) ="),
        "self-test must not touch pending-cleanup retry or agent/ directory"
    );
    // Background probes stay off too: a self-test must never unpack or probe
    // an external exe.
    assert!(
        workspace.contains("goose_probe_pending: false"),
        "external goose probe must stay disabled under self-test"
    );
}

#[test]
fn mcp_client_config_embeds_the_listener_and_token() {
    let name = duthub_desktop::agent_ui::mcp_server_name("windows-pc");
    let text = duthub_desktop::agent_ui::mcp_client_config(
        &name,
        "http://127.0.0.1:8790/mcp",
        "read-token",
    );
    assert!(
        text.ends_with('\n'),
        "The config file must end in a newline"
    );
    assert!(
        text.contains("\"duthub-windows-pc\""),
        "Derive the server key from the DUT ID"
    );
    assert!(text.contains("\"type\": \"http\""));
    assert!(text.contains("\"url\": \"http://127.0.0.1:8790/mcp\""));
    assert!(text.contains("\"Authorization\": \"Bearer read-token\""));
    assert!(text.contains("\"mcpServers\""));
}

#[test]
fn mcp_client_url_prefers_lan_ip_when_available() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    let lan_v4 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 175));
    let lan_url = duthub_desktop::agent_ui::mcp_client_url(33612, Some(lan_v4));
    assert_eq!(lan_url, "http://192.168.1.175:33612/mcp");

    let config =
        duthub_desktop::agent_ui::mcp_client_config("duthub-dut-1", &lan_url, "dth-secret");
    assert!(config.contains("\"url\": \"http://192.168.1.175:33612/mcp\""));

    let lan_v6 = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
    assert_eq!(
        duthub_desktop::agent_ui::mcp_client_url(33612, Some(lan_v6)),
        "http://[fe80::1]:33612/mcp"
    );
    assert_eq!(
        duthub_desktop::agent_ui::mcp_client_url(33612, None),
        "http://127.0.0.1:33612/mcp"
    );
}

#[test]
fn a2a_client_config_embeds_the_card_url_and_token() {
    let text = duthub_desktop::agent_ui::a2a_client_config(
        "http://127.0.0.1:8791/.well-known/agent-card.json",
        "a2a-token",
    );
    assert!(text.ends_with('\n'));
    assert!(
        text.contains("\"agentCardUrl\": \"http://127.0.0.1:8791/.well-known/agent-card.json\"")
    );
    assert!(text.contains("\"Authorization\": \"Bearer a2a-token\""));
}

#[test]
fn mcp_server_name_folds_ids_into_a_stable_key() {
    use duthub_desktop::agent_ui::mcp_server_name;
    assert_eq!(mcp_server_name("windows-pc"), "duthub-windows-pc");
    assert_eq!(mcp_server_name("RK 3576 / A"), "duthub-rk-3576-a");
    assert_eq!(mcp_server_name("中文"), "duthub");
    assert_eq!(mcp_server_name("a__b"), "duthub-a-b");
}

// ---------------------------------------------------------------------------
// Agent page: goose-not-installed guidance + honest install states
// ---------------------------------------------------------------------------

#[test]
fn agent_page_guides_the_user_to_tool_downloads_when_goose_is_missing() {
    use duthub_desktop::agent_host::{GooseInstallState, goose_install_state_from};

    // The page renders one of three facts. Precedence and the "nothing usable"
    // state are its inputs: only the last one asks for an install.
    let downloaded = goose_install_state_from(
        Some((
            PathBuf::from("C:/cfg/tools/goose/goose.exe"),
            "a".repeat(64),
        )),
        Some(PathBuf::from("C:/bin/goose.exe")),
    );
    assert!(matches!(downloaded, GooseInstallState::Downloaded { .. }));
    assert!(
        !downloaded.needs_install(),
        "An installed download must not prompt for installation"
    );
    assert_eq!(
        downloaded.program(),
        Some(std::path::Path::new("C:/cfg/tools/goose/goose.exe")),
        "Installed downloads take precedence over PATH"
    );

    let on_path = goose_install_state_from(None, Some(PathBuf::from("C:/bin/goose.exe")));
    assert!(matches!(on_path, GooseInstallState::Path { .. }));
    assert!(
        !on_path.needs_install(),
        "An executable on PATH counts as installed"
    );

    let missing = goose_install_state_from(None, None);
    assert_eq!(missing, GooseInstallState::NotInstalled);
    assert!(
        missing.needs_install(),
        "An absent install must direct the user to Tool Downloads"
    );
    assert_eq!(
        missing.program(),
        None,
        "An absent install has no program path"
    );
    let text = missing.to_string();
    assert!(
        text.contains("未安装") && text.contains("工具下载"),
        "Show the missing installation and the next action: {text}"
    );

    // Wiring (source contract): the panel renders the classified state, offers
    // the one-click jump to the tool-downloads page, and no longer slices an
    // embedded digest (there is no embedded build any more).
    let pages = source_of("pages.rs");
    assert!(
        pages.contains("w.agent.goose_install_state()"),
        "runtime panel must read built-in goose install state"
    );
    assert!(
        pages.contains("builtin_goose_status_line(&goose_state)"),
        "built-in status line must render all three states (downloaded / PATH / not installed)"
    );
    assert!(
        pages.contains("goose_state.needs_install()")
            && pages.contains("crate::workspace::ConfigPage::ToolDownloads"),
        "must provide one-click Tool Downloads button when not installed"
    );
    assert!(
        pages.contains("\"goose-install-open-downloads\""),
        "guidance button needs a stable id"
    );
    assert!(
        !pages.contains("GOOSE_SHA256"),
        "The embedded digest display has been removed"
    );
    assert!(
        !pages.contains("ProgramSource::Embedded"),
        "page must no longer reference removed ProgramSource::Embedded"
    );
    assert!(
        !pages.contains("agentRuntimeVersion\""),
        "The embedded version row has been removed"
    );

    // Classification reads the install manifest and scans PATH, so it is cached
    // with a TTL instead of running on every render frame.
    let agent = source_of("agent.rs");
    assert!(
        agent.contains("duthub_desktop::agent_host::goose_install_state()")
            && agent.contains("GOOSE_STATE_TTL"),
        "built-in install state must be TTL-cached rather than scanning disk/PATH per frame"
    );
    assert!(
        agent.contains("pub goose_install: Option<("),
        "Allow the cache to be supplied"
    );
}

#[test]
fn agent_page_states_are_localized_in_both_catalogs() {
    let locales = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("locales");
    let read = |name: &str| -> serde_json::Map<String, serde_json::Value> {
        let text = std::fs::read_to_string(locales.join(name)).unwrap();
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    };
    let catalogs = [
        ("zh-CN.json", read("zh-CN.json")),
        ("en-US.json", read("en-US.json")),
    ];
    assert_eq!(
        catalogs[0].1.len(),
        catalogs[1].1.len(),
        "The locales must have the same number of keys"
    );
    // Every state the panel can show has copy in both catalogs: the three goose
    // install facts, the guidance button, the model-not-configured states, and
    // the two keyring failure states.
    for key in [
        "gooseRuntimeDownloaded",
        "gooseRuntimePath",
        "gooseRuntimeNotInstalled",
        "gooseInstallOpenDownloads",
        "agentRuntimeNoModel",
        "agentRuntimeNoSelection",
        "agentKeyringReadFailed",
        "agentKeyringWriteFailed",
    ] {
        for (name, catalog) in &catalogs {
            assert!(
                catalog
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty()),
                "{name} is missing a nonempty label for {key}"
            );
        }
    }
    // No catalog may keep claiming an embedded goose build.
    for (name, catalog) in &catalogs {
        for (key, value) in catalog {
            let text = value.as_str().unwrap_or_default();
            assert!(
                !text.contains("内嵌 goose") && !text.contains("Embedded goose"),
                "{name}:{key} still claims goose is bundled: {text}"
            );
        }
    }
    // The "no model configured" state is rendered by the runtime panel, so the
    // copy must be wired there, not merely present in the catalogs.
    assert!(
        source_of("pages.rs").contains("\"agentRuntimeNoModel\""),
        "Show the missing model configuration and the next action"
    );
}
