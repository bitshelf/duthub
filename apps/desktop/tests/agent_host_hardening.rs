//! W2a Agent hosting hardening regressions.
//!
//! - Every new goose session (Fresh and Degraded) validates its runtime tool
//!   surface; resumed sessions re-probe the MCP tools of the new listener.
//! - Resume markers are double-written (`resume.json` + legacy `session-id`)
//!   and a marker from another Agent/program is ignored.
//! - `envKeys` cannot claim sandbox/isolation variables, and model variables
//!   are applied after `envKeys` without silent overrides.
//! - ACP stdout EOF wakes pending requests instead of blocking the full
//!   request timeout.
//! - The transcript writer flushes on finish and writes cap notes once.
//! - External Agent programs must be absolute paths.

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings};
use duthub_core::agent_config::{AgentDefinition, AgentKind};
use duthub_core::lease::ControlLeaseManager;
use duthub_core::{AuditSink, Provider};
use duthub_desktop::agent_host::acp::AcpClient;
use duthub_desktop::agent_host::host::{HostManager, StopJoin, validate_extension_results};
use duthub_desktop::agent_host::listener;
use duthub_desktop::agent_host::model_env::GooseBinding;
use duthub_desktop::agent_host::registry;
use duthub_desktop::agent_host::sandbox;
use duthub_desktop::agent_host::session::{
    FILE_TRANSCRIPT_LIMIT, SessionStatus, SessionView, TranscriptEntry, TranscriptKind,
};
use duthub_desktop::agent_host::transcript::TranscriptWriter;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::snapshot::SnapshotState;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-agent-hardening-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn binding() -> GooseBinding {
    GooseBinding {
        provider: Provider::Openai,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "fake-model".into(),
        api_key: "sk-hardening-secret".into(),
    }
}

fn access_context() -> Arc<AccessContext> {
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let lease = Arc::new(ControlLeaseManager::new(None));
    Arc::new(AccessContext::new(auth, lease, AuditSink::memory()))
}

// ---------------------------------------------------------------------------
// Test goose fixture (2026-11-24: goose is downloaded, no longer embedded)
// ---------------------------------------------------------------------------

/// External-program definition that runs an installed goose through the same
/// fail-closed path as production (absolute path + pinned SHA-256).
///
/// `DUTHUB_TEST_GOOSE` (absolute path to a goose executable) wins, otherwise the
/// product resolver decides (downloaded install > PATH).
fn test_goose_agent() -> Option<AgentDefinition> {
    let program = match std::env::var_os("DUTHUB_TEST_GOOSE") {
        Some(raw) => {
            let path = PathBuf::from(raw);
            assert!(
                path.is_absolute() && path.is_file(),
                "DUTHUB_TEST_GOOSE 必须是存在的 goose 可执行文件绝对路径: {}",
                path.display()
            );
            path
        }
        None => registry::goose_install_state().program()?.to_path_buf(),
    };
    let sha256 = duthub_core::store::sha256_file(&program).ok()?;
    let mut agent = AgentDefinition::goose();
    agent.program = Some(program.display().to_string());
    agent.expected_sha256 = Some(sha256);
    Some(agent)
}

/// Loud skip: goose is no longer embedded, so ACP tests need an installed one.
fn skip_without_goose() {
    eprintln!(
        "SKIP: 未安装 goose（内嵌资产已于 2026-11-24 移除）——先在「工具下载」页下载 goose，\
         或设置 DUTHUB_TEST_GOOSE 指向 goose 可执行文件"
    );
}

fn wait_ready(host: &mut HostManager, dut: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        host.tick();
        if host
            .view(dut)
            .is_some_and(|view| matches!(view.status(), SessionStatus::Ready))
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_terminal(host: &mut HostManager, dut: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        host.tick();
        if host
            .view(dut)
            .is_some_and(|view| view.status().is_terminal())
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn transcript_contains(view: &SessionView, needle: &str) -> bool {
    view.transcript_snapshot()
        .iter()
        .any(|entry| entry.text.contains(needle))
}

fn resume_json(agent_id: &str, sha256: &str, session_id: &str) -> String {
    serde_json::json!({
        "version": 1,
        "agentId": agent_id,
        "programSha256": sha256,
        "sessionId": session_id,
    })
    .to_string()
}

#[test]
fn degraded_session_persists_legacy_session_id_and_resume_json() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("degraded");
    let paths = sandbox::prepare(&work, "dut-degraded").unwrap();
    // Legacy marker only: goose accepts the file but the id itself is stale.
    std::fs::write(&paths.session_id, "stale-legacy-id").unwrap();

    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_with_agent("dut-degraded", Some(binding()), agent)
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-degraded"),
        "降级会话未就绪：{:?} stderr={}",
        host.view("dut-degraded").map(|view| view.status()),
        host.view("dut-degraded")
            .map(|view| view.stderr_tail())
            .unwrap_or_default()
    );

    let view = host.view("dut-degraded").unwrap();
    let session_id = view.session_id().expect("session id");
    assert_ne!(session_id, "stale-legacy-id", "stale id 不得被沿用");
    assert!(
        transcript_contains(&view, "已降级为新会话"),
        "必须显示降级注记：{:?}",
        view.transcript_snapshot()
    );
    assert_eq!(
        std::fs::read_to_string(&paths.session_id).unwrap().trim(),
        session_id,
        "新会话 id 必须写回 legacy session-id"
    );
    let resume: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.resume).unwrap()).unwrap();
    let launch = host.launch("dut-degraded").expect("launch spec");
    assert_eq!(resume["version"], 1);
    assert_eq!(resume["agentId"], "goose");
    assert_eq!(resume["programSha256"], launch.sha256);
    assert_eq!(resume["sessionId"], session_id);

    host.stop("dut-degraded");
    assert!(wait_terminal(&mut host, "dut-degraded"));
    assert!(
        !paths.resume.exists() && !paths.session_id.exists(),
        "显式停止必须删除两个 resume 标记"
    );
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn resume_state_rejects_agent_or_program_hash_mismatch() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("resume-mismatch");
    let paths = sandbox::prepare(&work, "dut-resume-mismatch").unwrap();

    // 1) Marker from another Agent: it must never be resumed.
    std::fs::write(
        &paths.resume,
        resume_json("acme", &"a".repeat(64), "cross-agent-session"),
    )
    .unwrap();
    std::fs::write(&paths.session_id, "cross-agent-session").unwrap();
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_with_agent("dut-resume-mismatch", Some(binding()), agent.clone())
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-resume-mismatch"),
        "跨 Agent 标记被拒后应新建会话：{:?}",
        host.view("dut-resume-mismatch").map(|view| view.status())
    );
    let view = host.view("dut-resume-mismatch").unwrap();
    assert_ne!(
        view.session_id().as_deref(),
        Some("cross-agent-session"),
        "不得恢复其他 Agent 的会话"
    );
    assert!(
        transcript_contains(&view, "不匹配"),
        "必须显示不匹配注记：{:?}",
        view.transcript_snapshot()
    );
    let rewritten: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.resume).unwrap()).unwrap();
    assert_eq!(rewritten["agentId"], "goose", "新会话必须重写标记");
    host.stop("dut-resume-mismatch");
    assert!(wait_terminal(&mut host, "dut-resume-mismatch"));
    drop(host);

    // 2) Same agent but a different program binary hash.
    std::fs::write(
        &paths.resume,
        resume_json("goose", &"0".repeat(64), "old-binary-session"),
    )
    .unwrap();
    std::fs::write(&paths.session_id, "old-binary-session").unwrap();
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_with_agent("dut-resume-mismatch", Some(binding()), agent.clone())
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-resume-mismatch"),
        "程序哈希不匹配后应新建会话：{:?}",
        host.view("dut-resume-mismatch").map(|view| view.status())
    );
    let view = host.view("dut-resume-mismatch").unwrap();
    assert_ne!(view.session_id().as_deref(), Some("old-binary-session"));
    assert!(
        transcript_contains(&view, "不匹配"),
        "必须显示程序哈希不匹配注记：{:?}",
        view.transcript_snapshot()
    );
    host.stop("dut-resume-mismatch");
    assert!(wait_terminal(&mut host, "dut-resume-mismatch"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn resumed_session_reexposes_mcp_tools() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    // Probe success + failure routes against a real session listener.
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let access = Arc::new(AccessContext::new(
        auth.clone(),
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    ));
    let listener = listener::issue(
        Arc::new(SnapshotState::empty()),
        access,
        auth.clone(),
        "dut-probe",
    )
    .expect("session listener");
    let url = format!("http://127.0.0.1:{}/mcp", listener.port);
    let names = duthub_mcp::probe::list_tool_names(&url, &listener.token, Duration::from_secs(15))
        .expect("只读会话 tool 面必须可探测");
    assert!(
        names.iter().any(|name| name == "dut_list"),
        "探测必须看到 duthub 工具面: {names:?}"
    );
    let denied =
        duthub_mcp::probe::list_tool_names(&url, "dth-wrong-token", Duration::from_secs(10))
            .expect_err("错误 Token 必须让探测失败");
    assert!(denied.to_string().contains("失败"), "{denied}");
    listener::revoke(listener, &auth);

    // Host route: a resumed session must pass the probe before becoming Ready.
    let work = temp_dir("resume-probe");
    let paths = sandbox::prepare(&work, "dut-resume-probe").unwrap();
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_with_agent("dut-resume-probe", Some(binding()), agent.clone())
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-resume-probe"),
        "首次会话未就绪：{:?}",
        host.view("dut-resume-probe").map(|view| view.status())
    );
    let original = host.view("dut-resume-probe").unwrap().session_id().unwrap();
    let sha = host.launch("dut-resume-probe").unwrap().sha256;
    host.stop("dut-resume-probe");
    assert!(wait_terminal(&mut host, "dut-resume-probe"));
    drop(host);

    // Keep the session id as if the previous process had crashed (explicit
    // stop deleted both markers above).
    std::fs::write(&paths.resume, resume_json("goose", &sha, &original)).unwrap();
    std::fs::write(&paths.session_id, &original).unwrap();
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_with_agent("dut-resume-probe", Some(binding()), agent.clone())
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-resume-probe"),
        "恢复后未通过 MCP 工具面复验：{:?} stderr={}",
        host.view("dut-resume-probe").map(|view| view.status()),
        host.view("dut-resume-probe")
            .map(|view| view.stderr_tail())
            .unwrap_or_default()
    );
    let view = host.view("dut-resume-probe").unwrap();
    assert_eq!(view.session_id().as_deref(), Some(original.as_str()));
    assert!(
        transcript_contains(&view, "MCP 工具面复验通过"),
        "恢复路径必须重新探测工具面：{:?}",
        view.transcript_snapshot()
    );
    host.stop("dut-resume-probe");
    assert!(wait_terminal(&mut host, "dut-resume-probe"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn unknown_extension_is_rejected() {
    assert!(
        validate_extension_results(&serde_json::json!([{ "name": "duthub", "success": true }]))
            .is_ok()
    );
    for bad in [
        serde_json::json!([{ "name": "unknown", "success": true }]),
        serde_json::json!([{ "name": "developer", "success": true }]),
        serde_json::json!([{ "name": "tom", "success": false }]),
        serde_json::json!([
            { "name": "duthub", "success": true },
            { "name": "unknown", "success": true }
        ]),
        serde_json::json!([
            { "name": "duthub", "success": true },
            { "name": "duthub", "success": true }
        ]),
    ] {
        assert!(
            validate_extension_results(&bad).is_err(),
            "未知/重复扩展必须拒绝: {bad}"
        );
    }
}

#[test]
fn acp_eof_wakes_pending_initialize() {
    let work = temp_dir("acp-eof");
    let paths = sandbox::prepare(&work, "dut-eof").unwrap();
    let program =
        std::env::var_os("COMSPEC").map_or_else(|| PathBuf::from("cmd.exe"), PathBuf::from);
    let acp = AcpClient::spawn_with_cancel(
        &program,
        &["/c".into(), "exit 0".into()],
        &paths.sandbox,
        &[],
        4096,
        Arc::new(AtomicBool::new(false)),
    )
    .expect("spawn cmd");
    let started = Instant::now();
    let error = acp
        .initialize(Duration::from_secs(10))
        .expect_err("stdout EOF 必须唤醒 initialize");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "EOF 后 initialize 仍阻塞了 {elapsed:?}（error={error}）"
    );
    acp.close_stdin();
    acp.kill();
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn transcript_writer_flushes_on_finish_and_notes_cap_once() {
    let dir = temp_dir("transcript");
    let path = dir.join("transcript.jsonl");
    let view = Arc::new(SessionView::new());
    let writer = TranscriptWriter::open(&path, view.clone());
    writer.push(TranscriptEntry::new(TranscriptKind::User, "hello"));
    writer.push(TranscriptEntry::new(TranscriptKind::Agent, "world"));
    writer.finish();

    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "finish 必须刷出全部条目: {text}");
    assert_eq!(lines[1]["text"], "world");

    let cap_path = dir.join("cap.jsonl");
    let cap_view = Arc::new(SessionView::new());
    let writer = TranscriptWriter::open(&cap_path, cap_view.clone());
    let payload = "x".repeat(64 * 1024);
    for _ in 0..130 {
        writer.push(TranscriptEntry::new(TranscriptKind::Agent, &payload));
    }
    writer.finish();
    assert!(
        std::fs::metadata(&cap_path).unwrap().len() <= FILE_TRANSCRIPT_LIMIT,
        "文件不得超过 8 MiB"
    );
    let notes = cap_view
        .transcript_snapshot()
        .iter()
        .filter(|entry| entry.text.contains("8 MiB"))
        .count();
    assert_eq!(notes, 1, "上限注记只能出现一次");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preassigned_temp_env_keys_rejected() {
    for key in [
        "TEMP",
        "TMP",
        "PATH",
        "SystemRoot",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "HOME",
        "HOMEDRIVE",
        "HOMEPATH",
        "GOOSE_PATH_ROOT",
        "GOOSE_MODE",
        "GOOSE_DISABLE_KEYRING",
    ] {
        assert!(
            duthub_core::agent_config::validate_env_key(key).is_err(),
            "envKeys 不得包含 {key}"
        );
    }
    assert!(duthub_core::agent_config::validate_env_key("ACME_API_KEY").is_ok());

    // Host-level defence in depth for a hand-built definition that never went
    // through `save_agents_atomic`.
    let work = temp_dir("reserved-env");
    let mut agent = AgentDefinition::goose();
    agent.env_keys = vec!["TEMP".into()];
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_with_agent("dut-env", Some(binding()), agent)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        host.tick();
        if host
            .view("dut-env")
            .is_some_and(|view| view.status().is_terminal())
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    match host.view("dut-env").unwrap().status() {
        SessionStatus::Failed(message) => assert!(
            message.contains("保留变量") || message.contains("envKeys"),
            "unexpected failure: {message}"
        ),
        other => panic!("保留变量必须拒绝启动，实际 {other:?}"),
    }
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn absolute_external_program_required() {
    let work = temp_dir("absolute");
    for relative in ["acme.exe", "tools/acme.exe", "./acme.exe", "../acme.exe"] {
        let agent = AgentDefinition {
            id: "acme".into(),
            name: "Acme ACP".into(),
            kind: AgentKind::Acp,
            program: Some(relative.into()),
            args: Vec::new(),
            env_keys: Vec::new(),
            cli_profile: None,
            enabled: true,
            auth_mode: "inherit".into(),
            trust_level: "mcp_only".into(),
            expected_sha256: Some("a".repeat(64)),
        };
        let error = registry::resolve(&work, &agent).expect_err("相对路径必须拒绝");
        assert!(error.to_string().contains("绝对路径"), "{error}");
    }
    for absolute in ["C:/tools/acme.exe", r"c:\tools\acme.exe"] {
        assert!(registry::is_absolute_program(absolute), "{absolute}");
    }
    assert!(!registry::is_absolute_program(r"\\server\share\acme.exe"));
    assert!(!registry::is_absolute_program("tools/acme.exe"));
    assert!(registry::probe_program(std::path::Path::new("acme.exe"), false).is_err());
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn stop_and_join_reports_not_running_and_cleans_entry() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("stop-join");
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    assert_eq!(
        host.stop_and_join("dut-missing", Duration::from_secs(1)),
        StopJoin::NotRunning
    );
    host.start_with_agent("dut-stop", Some(binding()), agent)
        .unwrap();
    assert!(wait_ready(&mut host, "dut-stop"));
    assert_eq!(
        host.stop_and_join("dut-stop", Duration::from_secs(30)),
        StopJoin::Joined
    );
    assert!(host.view("dut-stop").is_none(), "Joined 后 entry 必须移除");
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}
