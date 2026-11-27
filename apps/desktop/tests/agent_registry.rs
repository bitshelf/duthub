//! 通用 ACP Agent 注册表 + 外部 goose 启动链回归。
//!
//! - 外部 goose 路径真正被 Host 采用（程序/哈希记录进 `SessionView`）；
//! - 两个 ACP 条目可按 DUT 选择（默认 goose 解析 + 外部 ACP 条目）；
//! - 外部程序哈希不匹配 fail closed；CLI profile 明确拒绝；
//! - `agents.json` 不存 token；版本门槛接受 >= 1.60.2。
//!
//! 2026-11-24 起 goose 不再内嵌：需要真实 goose 的用例用本机已安装的 goose
//! （「工具下载」页下载，或 `DUTHUB_TEST_GOOSE` 指向可执行文件），否则带清晰原因跳过。

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings};
use duthub_core::agent_config::{
    AgentDefinition, AgentKind, AgentsFile, default_agents, load_agents,
    save_agent_bindings_atomic, save_agents_atomic,
};
use duthub_core::lease::ControlLeaseManager;
use duthub_core::{AuditSink, Provider};
use duthub_desktop::agent_host::host::{HostError, HostManager};
use duthub_desktop::agent_host::model_env::GooseBinding;
use duthub_desktop::agent_host::registry::{self, MIN_GOOSE_VERSION};
use duthub_desktop::agent_host::session::SessionStatus;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::snapshot::SnapshotState;

fn access_context() -> Arc<AccessContext> {
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let lease = Arc::new(ControlLeaseManager::new(None));
    Arc::new(AccessContext::new(auth, lease, AuditSink::memory()))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duthub-agent-reg-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Test goose fixture (2026-11-24: goose is downloaded, no longer embedded)
// ---------------------------------------------------------------------------

/// `DUTHUB_TEST_GOOSE` (absolute path to a goose executable) wins, otherwise the
/// product resolver decides (downloaded install > PATH). `None` = not installed.
fn test_goose_program() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("DUTHUB_TEST_GOOSE") {
        let path = PathBuf::from(raw);
        assert!(
            path.is_absolute() && path.is_file(),
            "DUTHUB_TEST_GOOSE 必须是存在的 goose 可执行文件绝对路径: {}",
            path.display()
        );
        return Some(path);
    }
    registry::goose_install_state()
        .program()
        .map(std::path::Path::to_path_buf)
}

/// Loud skip: goose is no longer embedded, so ACP tests need an installed one.
fn skip_without_goose() {
    eprintln!(
        "SKIP: 未安装 goose（内嵌资产已于 2026-11-24 移除）——先在「工具下载」页下载 goose，\
         或设置 DUTHUB_TEST_GOOSE 指向 goose 可执行文件"
    );
}

/// Built-in goose definition pointed at the installed test goose through the
/// external-program path (absolute path + the real pinned SHA-256).
fn goose_with_program(program: &std::path::Path) -> AgentDefinition {
    let sha256 = duthub_core::store::sha256_file(program).expect("读取测试 goose 哈希");
    let mut agent = AgentDefinition::goose();
    agent.program = Some(program.display().to_string());
    agent.expected_sha256 = Some(sha256);
    agent
}

fn binding() -> GooseBinding {
    GooseBinding {
        provider: Provider::Openai,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "fake-model".into(),
        api_key: "sk-registry-secret".into(),
    }
}

fn acme_agent() -> AgentDefinition {
    AgentDefinition {
        id: "acme".into(),
        name: "Acme ACP".into(),
        kind: AgentKind::Acp,
        program: Some("C:/tools/acme.exe".into()),
        args: Vec::new(),
        env_keys: Vec::new(),
        cli_profile: None,
        enabled: true,
        auth_mode: "inherit".into(),
        trust_level: "mcp_only".into(),
        expected_sha256: Some("a".repeat(64)),
    }
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

#[test]
fn host_uses_external_goose_override_not_embedded_copy() {
    let Some(program) = test_goose_program() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("external");
    let expected_sha256 = duthub_core::store::sha256_file(&program).expect("测试 goose 哈希");
    let mut file = default_agents();
    *file.goose_mut().expect("default goose") = goose_with_program(&program);
    save_agents_atomic(&work, &file).unwrap();

    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    // 走默认 start：Host 自己读取 <work_dir>/agents.json 的 goose 条目。
    host.start("dut-ext", binding()).unwrap();
    assert!(
        wait_ready(&mut host, "dut-ext"),
        "外部 goose 未就绪：{:?} stderr={}",
        host.view("dut-ext").map(|view| view.status()),
        host.view("dut-ext")
            .map_or_default(|view| view.stderr_tail())
    );
    let launch = host.launch("dut-ext").expect("必须记录实际启动规格");
    assert_eq!(launch.agent_id, "goose");
    assert!(
        !launch.embedded,
        "不再有内嵌副本：解析必须落到外部/已下载路径"
    );
    assert_eq!(launch.program, program);
    assert_eq!(launch.sha256, expected_sha256);
    assert!(
        launch
            .version
            .as_deref()
            .is_some_and(|version| registry::version_at_least(version, MIN_GOOSE_VERSION)),
        "{launch:?}"
    );

    host.stop("dut-ext");
    assert!(wait_terminal(&mut host, "dut-ext"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn two_acp_entries_are_selected_per_dut() {
    let Some(program) = test_goose_program() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("two");
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    // DUT A：默认 goose 条目（不再内嵌，外部路径指向测试 goose）；DUT B：独立
    // ACP 条目（同一份可执行文件，但不同的程序路径记录与 agent id）。选择来自
    // 持久化的 agent-bindings.json。
    let mut file = default_agents();
    *file.goose_mut().expect("default goose") = goose_with_program(&program);
    let mut acme = acme_agent();
    acme.program = Some(program.display().to_string());
    acme.expected_sha256 =
        Some(duthub_core::store::sha256_file(&program).expect("测试 goose 哈希"));
    file.agents.push(acme);
    save_agents_atomic(&work, &file).unwrap();
    let file = load_agents(&work).unwrap();
    let mut bindings = duthub_core::agent_config::AgentBindingsFile::default();
    bindings.set_agent_id("dut-a", "goose");
    bindings.set_agent_id("dut-b", "acme");
    save_agent_bindings_atomic(&work, &bindings).unwrap();
    let bindings = duthub_core::agent_config::load_agent_bindings(&work).unwrap();
    let agent_for = |dut: &str| {
        file.agent(&bindings.agent_id_for(dut))
            .unwrap_or_else(|| panic!("missing agent for {dut}"))
            .clone()
    };
    host.start_with_agent("dut-a", Some(binding()), agent_for("dut-a"))
        .unwrap();
    host.start_with_agent("dut-b", Some(binding()), agent_for("dut-b"))
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-a"),
        "dut-a 未就绪：{:?}",
        host.view("dut-a").map(|view| view.status())
    );
    assert!(
        wait_ready(&mut host, "dut-b"),
        "dut-b 未就绪：{:?} stderr={}",
        host.view("dut-b").map(|view| view.status()),
        host.view("dut-b")
            .map_or_default(|view| view.stderr_tail())
    );
    let launch_a = host.launch("dut-a").unwrap();
    let launch_b = host.launch("dut-b").unwrap();
    assert_eq!(launch_a.agent_id, "goose");
    assert!(!launch_a.embedded, "不再有内嵌副本");
    assert_eq!(launch_a.program, program);
    assert_eq!(launch_b.agent_id, "acme");
    assert!(!launch_b.embedded);
    assert_eq!(launch_b.program, program);
    // goose 的 session id 按日期/进程内计数生成，两个独立进程可能得到同一字符串；
    // 用每会话独立 loopback listener 端口证明 DUT 之间没有共享会话。
    let port_a = host.view("dut-a").unwrap().port();
    let port_b = host.view("dut-b").unwrap().port();
    assert!(port_a.is_some() && port_b.is_some());
    assert_ne!(port_a, port_b, "每 DUT 必须使用独立的 MCP listener");

    host.stop("dut-a");
    host.stop("dut-b");
    assert!(wait_terminal(&mut host, "dut-a"));
    assert!(wait_terminal(&mut host, "dut-b"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn hash_mismatch_fails_closed_without_launching_program() {
    let work = temp_dir("hash");
    // A real file whose pinned digest does not match: the launch must fail
    // before any process is spawned (never "run anyway").
    let program = work.join("acme.exe");
    std::fs::write(&program, b"not the pinned program").unwrap();
    let mut agent = acme_agent();
    agent.program = Some(program.display().to_string());
    agent.expected_sha256 = Some("0".repeat(64));
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_with_agent("dut-hash", Some(binding()), agent)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        host.tick();
        if host
            .view("dut-hash")
            .is_some_and(|view| matches!(view.status(), SessionStatus::Failed(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    match host.view("dut-hash").unwrap().status() {
        SessionStatus::Failed(message) => assert!(
            message.contains("哈希") || message.contains("Hash"),
            "unexpected failure: {message}"
        ),
        other => panic!("哈希不匹配必须 Failed，实际 {other:?}"),
    }
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn cli_profile_is_rejected_before_spawning() {
    let work = temp_dir("cli");
    let mut agent = AgentDefinition::goose();
    agent.id = "claude".into();
    agent.name = "Claude Code".into();
    agent.kind = AgentKind::Cli;
    agent.cli_profile = Some("claude-code".into());
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    let error = host
        .start_with_agent("dut-cli", Some(binding()), agent)
        .unwrap_err();
    assert!(matches!(error, HostError::Invalid(_)), "{error}");
    assert!(
        error.to_string().contains("CLI profile 本轮未实现"),
        "{error}"
    );
    assert!(host.view("dut-cli").is_none(), "CLI 不得创建会话记录");
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn agents_file_roundtrip_has_no_token_values_and_versions_accept_newer() {
    let work = temp_dir("config");
    let mut file = default_agents();
    file.agents.push(AgentDefinition {
        id: "acme".into(),
        name: "Acme ACP".into(),
        kind: AgentKind::Acp,
        program: Some("C:/tools/acme.exe".into()),
        args: vec!["--acp".into()],
        env_keys: vec!["ACME_API_KEY".into()],
        cli_profile: None,
        enabled: true,
        auth_mode: "inherit".into(),
        trust_level: "mcp_only".into(),
        expected_sha256: Some("a".repeat(64)),
    });
    save_agents_atomic(&work, &file).unwrap();
    let raw = std::fs::read_to_string(duthub_core::agent_config::agents_path(&work)).unwrap();
    assert!(!raw.contains("sk-registry-secret"), "{raw}");
    assert!(!raw.contains("\"token\""), "{raw}");
    assert!(!raw.contains("\"apiKey\""), "{raw}");
    let loaded: AgentsFile = load_agents(&work).unwrap();
    assert_eq!(loaded, file);

    // 版本探测只设下限：1.60.2 与更新版本都接受，旧版本拒绝。
    assert!(registry::version_at_least(
        "goose-acp 1.60.2",
        MIN_GOOSE_VERSION
    ));
    assert!(registry::version_at_least(
        "goose-acp 1.61.0",
        MIN_GOOSE_VERSION
    ));
    assert!(registry::version_at_least(
        "goose-acp 2.0.0",
        MIN_GOOSE_VERSION
    ));
    assert!(!registry::version_at_least(
        "goose-acp 1.60.1",
        MIN_GOOSE_VERSION
    ));
    let _ = std::fs::remove_dir_all(&work);
}

/// Real-binary probe (no embedded copy any more): needs an installed goose.
#[test]
fn probe_program_accepts_an_installed_goose() {
    let work = temp_dir("probe");
    let Some(program) = test_goose_program() else {
        skip_without_goose();
        return;
    };
    let probe = registry::probe_program(&program, true).unwrap();
    assert_eq!(
        probe.sha256,
        duthub_core::store::sha256_file(&program).unwrap()
    );
    assert!(
        probe
            .version
            .as_deref()
            .is_some_and(|line| registry::version_at_least(line, MIN_GOOSE_VERSION)),
        "版本门槛必须接受已安装 goose：{probe:?}"
    );
    assert_eq!(probe.signature, "NotSigned");
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn registry_resolve_rejects_external_program_without_pinned_hash() {
    let work = temp_dir("unpinned");
    let mut agent = acme_agent();
    agent.expected_sha256 = None;
    let error = registry::resolve(&work, &agent).unwrap_err();
    assert!(error.to_string().contains("expectedSha256"), "{error}");
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn external_goose_older_than_minimum_is_rejected_by_version_gate() {
    // 纯版本门槛（不依赖真实旧二进制）：1.59.x 必须拒绝，1.60.2+ 接受。
    for older in ["goose-acp 1.59.9", "goose-acp 1.60.0", "goose-acp 1.60.1"] {
        assert!(registry::ensure_goose_version(older).is_err(), "{older}");
    }
    for newer in ["goose-acp 1.60.2", "goose-acp 1.60.3", "goose-acp 1.61.0"] {
        assert!(registry::ensure_goose_version(newer).is_ok(), "{newer}");
    }
    assert_eq!(
        registry::parse_version("goose-acp 1.70.0"),
        Some((1, 70, 0))
    );
}
