//! W2c: CLI profile (`claude-code`) integration tests.
//!
//! The deterministic process contract is exercised through `.cmd` fixtures
//! that emit the same `stream-json` event shapes. A real Claude Code 2.1.204
//! is discovered from `DUTHUB_CLAUDE_EXE`, PATH (`claude.exe`), or the native
//! per-user install. The non-ignored
//! `cli_real_claude_expands_mcp_token_from_env` uses it for a health check
//! only (no model call) and skips when absent; the `#[ignore]`d live test
//! performs a billable model call and additionally requires
//! `DUTHUB_CLAUDE_LIVE=1`.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings};
use duthub_core::agent_config::{AgentDefinition, AgentKind};
use duthub_core::lease::ControlLeaseManager;
use duthub_core::{AuditSink, store::sha256_file};
use duthub_desktop::agent_host::cli;
use duthub_desktop::agent_host::host::{BindingSource, HostManager};
use duthub_desktop::agent_host::listener;
use duthub_desktop::agent_host::sandbox;
use duthub_desktop::agent_host::session::{SessionStatus, TranscriptKind};
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::snapshot::SnapshotState;
use serde_json::Value;

fn access_context() -> Arc<AccessContext> {
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let lease = Arc::new(ControlLeaseManager::new(None));
    Arc::new(AccessContext::new(auth, lease, AuditSink::memory()))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duthub-agent-cli-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_fixture(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    // cmd.exe parses LF-only batch files inconsistently; fixtures are CRLF.
    std::fs::write(&path, body.replace('\n', "\r\n")).unwrap();
    path
}

fn cli_agent(program: &Path) -> AgentDefinition {
    AgentDefinition {
        id: "claude".into(),
        name: "Claude Code".into(),
        kind: AgentKind::Cli,
        program: Some(program.display().to_string()),
        args: Vec::new(),
        env_keys: Vec::new(),
        cli_profile: Some(cli::CLAUDE_CODE_PROFILE.into()),
        enabled: true,
        auth_mode: "inherit".into(),
        trust_level: "mcp_only".into(),
        expected_sha256: Some(sha256_file(program).unwrap()),
    }
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_ready_for(host: &mut HostManager, dut: &str, timeout: Duration) -> bool {
    wait_until(timeout, || {
        host.tick();
        host.view(dut)
            .is_some_and(|view| matches!(view.status(), SessionStatus::Ready))
    })
}

fn wait_ready(host: &mut HostManager, dut: &str) -> bool {
    wait_ready_for(host, dut, Duration::from_secs(60))
}

fn wait_terminal(host: &mut HostManager, dut: &str) -> bool {
    wait_until(Duration::from_secs(30), || {
        host.tick();
        host.view(dut)
            .is_some_and(|view| view.status().is_terminal())
    })
}

const OK_FIXTURE: &str = r#"@echo off
setlocal
echo %*>> "%CD%\argv.log"
echo %DUTHUB_AGENT_TOKEN%> "%CD%\token.log"
echo {"type":"system","subtype":"init","session_id":"sess-cli-1","model":"fixture-model","mcp_servers":[{"name":"duthub","status":"connected"}]}
echo {"type":"assistant","session_id":"sess-cli-1","message":{"role":"assistant","content":[{"type":"text","text":"fixture says hello"}]}}
echo {"type":"result","subtype":"success","is_error":false,"session_id":"sess-cli-1","result":"done"}
exit /b 0
"#;

const SLOW_FIXTURE: &str = r#"@echo off
setlocal
echo {"type":"system","subtype":"init","session_id":"sess-cli-slow"}
start "" /b cmd /c "%~dp0heartbeat.cmd"
ping -n 60 127.0.0.1 >nul
exit /b 0
"#;

const HEARTBEAT_FIXTURE: &str = r#"@echo off
:loop
echo beat>> "%CD%\heartbeat.log"
ping -n 1 -w 200 127.0.0.1 >nul
goto loop
"#;

// Mirrors the real Claude Code 2.1.204 stream shapes: init `tools[]` +
// `mcp_server_errors`/failed status, a tool_result `user` event, and a
// `result` with `permission_denials[]`.
const RICH_EVENTS_FIXTURE: &str = r#"@echo off
setlocal
echo {"type":"system","subtype":"init","session_id":"sess-cli-rich","model":"fixture-model","tools":["Bash","mcp__duthub__dut_list","mcp__duthub__serial_read"],"mcp_servers":[{"name":"duthub","status":"connected"},{"name":"other","status":"failed"}],"mcp_server_errors":[{"name":"other","error":"connection refused"}]}
echo {"type":"rate_limit_event","session_id":"sess-cli-rich","rate_limit_info":{}}
echo {"type":"assistant","session_id":"sess-cli-rich","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"mcp__duthub__serial_read","input":{"dutId":"d1"}}]}}
echo {"type":"user","session_id":"sess-cli-rich","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"boot log line"}],"is_error":false}]}}
echo {"type":"result","subtype":"success","is_error":false,"session_id":"sess-cli-rich","result":"done","permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_2","tool_input":{"command":"rm -rf /"}}]}
exit /b 0
"#;

#[test]
fn cli_profile_streams_ndjson_and_persists_resume() {
    let work = temp_dir("stream");
    let fixture = write_fixture(&work, "claude-ok.cmd", OK_FIXTURE);
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_entry("dut-cli", BindingSource::None, cli_agent(&fixture))
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-cli"),
        "CLI session did not become ready: {:?} stderr={}",
        host.view("dut-cli").map(|view| view.status()),
        host.view("dut-cli")
            .map_or_default(|view| view.stderr_tail())
    );
    host.prompt("dut-cli", "hello duthub").unwrap();
    assert!(
        wait_ready(&mut host, "dut-cli"),
        "CLI turn did not finish: {:?} stderr={}",
        host.view("dut-cli").map(|view| view.status()),
        host.view("dut-cli")
            .map_or_default(|view| view.stderr_tail())
    );

    let view = host.view("dut-cli").unwrap();
    let entries = view.transcript_snapshot();
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind == TranscriptKind::Agent
                && entry.text.contains("fixture says hello")),
        "assistant text must enter transcript: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind == TranscriptKind::System
                && entry.text.contains("本轮结束（success）")),
        "result event must record turn-finished system note: {entries:?}"
    );

    let paths = sandbox::prepare(&work, "dut-cli").unwrap();
    let resume: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.resume).unwrap()).unwrap();
    assert_eq!(resume["agentId"], "claude");
    assert_eq!(resume["sessionId"], "sess-cli-1");
    assert_eq!(
        std::fs::read_to_string(&paths.session_id).unwrap(),
        "sess-cli-1"
    );

    host.stop("dut-cli");
    assert!(wait_terminal(&mut host, "dut-cli"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn cli_parses_user_tool_results_mcp_errors_and_plan_denials() {
    let work = temp_dir("rich");
    let fixture = write_fixture(&work, "claude-rich.cmd", RICH_EVENTS_FIXTURE);
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_entry("dut-cli", BindingSource::None, cli_agent(&fixture))
        .unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));
    host.prompt("dut-cli", "exercise event shapes").unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));

    let entries = host.view("dut-cli").unwrap().transcript_snapshot();
    let all = entries
        .iter()
        .map(|entry| entry.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind == TranscriptKind::Tool
                && entry.text.contains("工具结果")
                && entry.text.contains("boot log line")),
        "user/tool_result must enter transcript: {all}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind == TranscriptKind::Error
                && entry.text.contains("MCP 连接错误")
                && entry.text.contains("connection refused")),
        "mcp_server_errors must be visible: {all}"
    );
    assert!(
        all.contains("权限拒绝（plan 模式）") && all.contains("Bash"),
        "permission_denials must be visible: {all}"
    );
    assert!(
        all.contains("duthub 工具 2 个"),
        "init tools[] must report duthub tool surface: {all}"
    );
    assert!(
        all.contains("未处理的 CLI 事件: rate_limit_event"),
        "valid unrendered event type should be noted as unhandled rather than protocol noise: {all}"
    );
    assert!(
        !all.contains("忽略非 NDJSON stdout"),
        "valid user/unknown events must not be logged as protocol noise: {all}"
    );

    host.stop("dut-cli");
    assert!(wait_terminal(&mut host, "dut-cli"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn cli_resume_uses_session_id_on_second_turn() {
    let work = temp_dir("resume");
    let fixture = write_fixture(&work, "claude-ok.cmd", OK_FIXTURE);
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_entry("dut-cli", BindingSource::None, cli_agent(&fixture))
        .unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));

    let paths = sandbox::prepare(&work, "dut-cli").unwrap();
    host.prompt("dut-cli", "first turn").unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));
    let argv_log = paths.sandbox.join("argv.log");
    let first = std::fs::read_to_string(&argv_log).unwrap();
    assert!(first.contains("-p"), "{first}");
    assert!(
        !first.contains("--resume"),
        "first turn must not pass --resume: {first}"
    );

    host.prompt("dut-cli", "second turn").unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));
    let second = std::fs::read_to_string(&argv_log).unwrap();
    let lines: Vec<&str> = second
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 2, "each turn must spawn CLI once: {second}");
    assert!(
        !lines[0].contains("--resume"),
        "first turn must start a fresh session: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("--resume") && lines[1].contains("sess-cli-1"),
        "second turn must resume with session id from result event: {}",
        lines[1]
    );

    host.stop("dut-cli");
    assert!(wait_terminal(&mut host, "dut-cli"));
    drop(host);

    // Resume must survive the session stop: a new HostManager on the same work
    // dir resumes the persisted session id on its very first turn.
    let mut restarted = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    restarted
        .start_entry("dut-cli", BindingSource::None, cli_agent(&fixture))
        .unwrap();
    assert!(wait_ready(&mut restarted, "dut-cli"));
    assert_eq!(
        restarted.view("dut-cli").unwrap().session_id().as_deref(),
        Some("sess-cli-1"),
        "new session must restore session id from resume.json"
    );
    restarted.prompt("dut-cli", "third turn").unwrap();
    assert!(wait_ready(&mut restarted, "dut-cli"));
    let resumed = std::fs::read_to_string(&argv_log).unwrap();
    let lines: Vec<&str> = resumed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 3, "each turn must spawn CLI once: {resumed}");
    assert!(
        lines[2].contains("--resume") && lines[2].contains("sess-cli-1"),
        "first turn after restart must resume with persisted session id: {}",
        lines[2]
    );

    restarted.stop("dut-cli");
    assert!(wait_terminal(&mut restarted, "dut-cli"));
    drop(restarted);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn cli_mcp_config_uses_env_token_not_literal() {
    let work = temp_dir("mcp");
    let fixture = write_fixture(&work, "claude-ok.cmd", OK_FIXTURE);
    let agent = cli_agent(&fixture);
    let paths = sandbox::prepare(&work, "dut-cli").unwrap();

    // Plan-level contract: argv shape and env placeholder.
    let plan = cli::claude_code_plan(&agent, &paths.sandbox, "http://127.0.0.1:1/mcp").unwrap();
    let args = plan.args.join(" ");
    assert!(args.contains("-p"), "{args}");
    assert!(args.contains("--output-format stream-json"), "{args}");
    assert!(args.contains("--verbose"), "{args}");
    assert!(args.contains("--permission-mode plan"), "{args}");
    assert!(args.contains("--strict-mcp-config"), "{args}");
    assert!(args.contains("--mcp-config"), "{args}");
    assert!(
        plan.env.is_empty(),
        "plan must not carry token; worker injects it via env: {:?}",
        plan.env
    );
    let config = std::fs::read_to_string(&plan.mcp_config).unwrap();
    assert!(
        config.contains("${DUTHUB_AGENT_TOKEN}"),
        "MCP config must use env expansion: {config}"
    );
    assert!(
        !config.contains("Bearer dth-"),
        "MCP config must not contain literal token: {config}"
    );

    // Worker-level contract: the child sees DUTHUB_AGENT_TOKEN, and the
    // generated config still does not contain that value.
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_entry("dut-cli", BindingSource::None, agent)
        .unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));
    host.prompt("dut-cli", "token check").unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));

    let token = std::fs::read_to_string(paths.sandbox.join("token.log"))
        .unwrap()
        .trim()
        .to_owned();
    assert!(
        token.starts_with("dth-"),
        "child process must receive MCP token: {token:?}"
    );
    let config = std::fs::read_to_string(paths.sandbox.join(cli::MCP_CONFIG_FILE_NAME)).unwrap();
    assert!(
        !config.contains(&token),
        "MCP config must not persist token on disk: {config}"
    );
    assert!(
        !std::fs::read_to_string(paths.sandbox.join("argv.log"))
            .unwrap()
            .contains(&token),
        "MCP token must not appear in argv"
    );

    host.stop("dut-cli");
    assert!(wait_terminal(&mut host, "dut-cli"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn cli_stop_kills_process_tree() {
    let work = temp_dir("kill");
    let fixture = write_fixture(&work, "claude-slow.cmd", SLOW_FIXTURE);
    write_fixture(&work, "heartbeat.cmd", HEARTBEAT_FIXTURE);
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_entry("dut-cli", BindingSource::None, cli_agent(&fixture))
        .unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));
    host.prompt("dut-cli", "hold the tree").unwrap();

    let paths = sandbox::prepare(&work, "dut-cli").unwrap();
    let heartbeat = paths.sandbox.join("heartbeat.log");
    assert!(
        wait_until(Duration::from_secs(20), || {
            std::fs::metadata(&heartbeat).is_ok_and(|metadata| metadata.len() > 0)
        }),
        "grandchild heartbeat did not start (fixture did not run)"
    );

    host.stop("dut-cli");
    assert!(wait_terminal(&mut host, "dut-cli"));
    // A surviving grandchild would keep appending `beat` lines.
    std::thread::sleep(Duration::from_millis(1200));
    let before = std::fs::metadata(&heartbeat).unwrap().len();
    std::thread::sleep(Duration::from_millis(900));
    let after = std::fs::metadata(&heartbeat).unwrap().len();
    assert_eq!(
        before, after,
        "grandchild still writing heartbeat after host.stop (process tree not terminated by Job)"
    );

    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn cli_profile_cannot_write_outside_dut_sandbox() {
    let work = temp_dir("sandbox-write");
    let sibling = sandbox::prepare(&work, "dut-other").unwrap();
    let escape_sibling = sibling.sandbox.join("tampered.txt");
    let escape_root = work.join("escape.txt");
    let script = format!(
        "@echo off\r\nsetlocal\r\necho ok> \"%CD%\\inside.txt\"\r\necho bad> \"{}\"\r\necho bad> \"{}\"\r\necho {{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"sess-sb\",\"result\":\"done\"}}\r\nexit /b 0\r\n",
        escape_sibling.display(),
        escape_root.display()
    );
    let fixture = write_fixture(&work, "claude-escape.cmd", &script);
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_entry("dut-cli", BindingSource::None, cli_agent(&fixture))
        .unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));
    host.prompt("dut-cli", "attempt escape").unwrap();
    assert!(wait_ready(&mut host, "dut-cli"));

    let paths = sandbox::prepare(&work, "dut-cli").unwrap();
    assert!(
        paths.sandbox.join("inside.txt").is_file(),
        "CLI process must be allowed to write inside its own DUT sandbox"
    );
    assert!(
        !escape_sibling.exists(),
        "CLI process must NOT be able to modify a sibling DUT directory"
    );
    assert!(
        !escape_root.exists(),
        "CLI process must NOT be able to write outside its DUT sandbox"
    );

    host.stop("dut-cli");
    assert!(wait_terminal(&mut host, "dut-cli"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

/// Discover the real Claude Code executable: explicit override first, then
/// `claude.exe` on PATH, then the per-user native install directory.
fn real_claude_program() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DUTHUB_CLAUDE_EXE").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let candidate = directory.join("claude.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let fallback = PathBuf::from(home)
        .join(".local")
        .join("bin")
        .join("claude.exe");
    fallback.is_file().then_some(fallback)
}

fn claude_mcp_list(
    program: &Path,
    cwd: &Path,
    config_dir: &Path,
    token: &str,
) -> std::process::Output {
    Command::new(program)
        .args(["mcp", "list"])
        .current_dir(cwd)
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .env("DUTHUB_AGENT_TOKEN", token)
        .output()
        .expect("spawn claude mcp list")
}

/// Real CLI, no model call: `claude mcp list` health-checks the configured
/// duthub listener. A successful connection proves Claude expands
/// `${DUTHUB_AGENT_TOKEN}` in the HTTP header (the listener rejects a wrong
/// token); the negative control runs the same check with a bad token.
#[test]
fn cli_real_claude_expands_mcp_token_from_env() {
    let Some(program) = real_claude_program() else {
        eprintln!("skipped: claude.exe not found on PATH (set DUTHUB_CLAUDE_EXE)");
        return;
    };
    let work = temp_dir("real-mcp");
    let project = work.join("project");
    let config_dir = work.join("config");
    std::fs::create_dir_all(&project).unwrap();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let access = Arc::new(AccessContext::new(
        auth.clone(),
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    ));
    let listener =
        listener::issue(Arc::new(SnapshotState::empty()), access, auth, "dut-cli").unwrap();
    let url = format!("http://127.0.0.1:{}/mcp", listener.port);

    // The listener the CLI connects to must expose exactly the 13 Plan tools.
    let tools = duthub_mcp::probe::list_tool_names(&url, &listener.token, Duration::from_secs(10))
        .expect("duthub listener must answer tools/list");
    let mut exposed = tools;
    exposed.sort();
    let mut expected: Vec<String> = cli::PLAN_TOOLS
        .iter()
        .map(|tool| (*tool).to_owned())
        .collect();
    expected.sort();
    assert_eq!(
        exposed, expected,
        "duthub listener must expose exactly the 13 Plan tools"
    );

    let add = Command::new(&program)
        .args([
            "mcp",
            "add",
            "--transport",
            "http",
            "duthub",
            &url,
            "--header",
            "Authorization: Bearer ${DUTHUB_AGENT_TOKEN}",
        ])
        .current_dir(&project)
        .env("CLAUDE_CONFIG_DIR", &config_dir)
        .output()
        .expect("spawn claude mcp add");
    assert!(
        add.status.success(),
        "claude mcp add failed: {}{}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );

    let good = claude_mcp_list(&program, &project, &config_dir, &listener.token);
    let good_text = format!(
        "{}{}",
        String::from_utf8_lossy(&good.stdout),
        String::from_utf8_lossy(&good.stderr)
    );
    assert!(
        good_text.contains("Connected"),
        "real CLI failed to connect to duthub MCP using DUTHUB_AGENT_TOKEN: {good_text}"
    );

    // Diagnostic only: as of Claude Code 2.1.204 + rmcp 3.10.0 this reports
    // "connected · tools fetch failed" because every duthub tool's rmcp
    // outputSchema is not `type: object` (see the wave report gap). The
    // connection/header contract asserted above is unaffected; tool
    // registration is gated by the duthub-mcp fix.
    let get = Command::new(&program)
        .args(["mcp", "get", "duthub"])
        .current_dir(&project)
        .env("CLAUDE_CONFIG_DIR", &config_dir)
        .env("DUTHUB_AGENT_TOKEN", &listener.token)
        .output()
        .expect("spawn claude mcp get");
    let get_text = format!(
        "{}{}",
        String::from_utf8_lossy(&get.stdout),
        String::from_utf8_lossy(&get.stderr)
    );
    eprintln!("MCP GET: {get_text}");
    assert!(
        get_text.contains("Bearer ${DUTHUB_AGENT_TOKEN}"),
        "header read by real CLI must be the env placeholder: {get_text}"
    );
    assert!(
        get_text.contains("Connected"),
        "real CLI did not connect to duthub listener: {get_text}"
    );

    let bad = claude_mcp_list(&program, &project, &config_dir, "dth-wrong-token");
    let bad_text = format!(
        "{}{}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr)
    );
    assert!(
        !bad_text.contains("Connected"),
        "wrong token must not connect successfully (auth not enforced): {bad_text}"
    );

    drop(listener);
    let _ = std::fs::remove_dir_all(&work);
}

/// Live T27 check: the real Claude Code must connect to the per-session
/// duthub MCP listener and report the 13 Plan tools in `system/init.tools[]`.
///
/// As of 2.1.204 this is expected to fail at the tool count: the CLI's
/// `mcp get` reports "tools fetch failed" because rmcp 3.10.0 emits a
/// non-object `outputSchema` for the duthub tools (owned by duthub-mcp).
/// The test is the acceptance gate for that upstream fix.
///
/// Ignored by default because it performs a real (billable) model call and
/// needs the user's OAuth login. Run explicitly with:
/// `DUTHUB_CLAUDE_LIVE=1 cargo test -p duthub-desktop --test agent_cli -- --ignored cli_live`
#[test]
#[ignore = "requires real Claude Code auth and a billable model call; set DUTHUB_CLAUDE_LIVE=1"]
fn cli_live_claude_exposes_thirteen_plan_tools() {
    if std::env::var("DUTHUB_CLAUDE_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set DUTHUB_CLAUDE_LIVE=1 to run the live Claude call");
        return;
    }
    let Some(program) = real_claude_program() else {
        eprintln!("skipped: claude.exe not found on PATH (set DUTHUB_CLAUDE_EXE)");
        return;
    };
    let work = temp_dir("live");
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_entry("dut-cli", BindingSource::None, cli_agent(&program))
        .unwrap();
    assert!(
        wait_ready_for(&mut host, "dut-cli", Duration::from_secs(120)),
        "real CLI did not become ready: {:?} stderr={}",
        host.view("dut-cli").map(|view| view.status()),
        host.view("dut-cli")
            .map_or_default(|view| view.stderr_tail())
    );
    host.prompt("dut-cli", "Reply with the single word ok.")
        .unwrap();
    assert!(
        wait_ready_for(&mut host, "dut-cli", Duration::from_secs(300)),
        "real CLI turn did not finish: {:?} stderr={}",
        host.view("dut-cli").map(|view| view.status()),
        host.view("dut-cli")
            .map_or_default(|view| view.stderr_tail())
    );
    let entries = host.view("dut-cli").unwrap().transcript_snapshot();
    let system = entries
        .iter()
        .filter(|entry| entry.kind == TranscriptKind::System)
        .map(|entry| entry.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        system.contains("duthub 工具 13 个"),
        "real init did not report 13 duthub Plan tools: {system}"
    );
    for tool in cli::PLAN_TOOLS {
        assert!(
            system.contains(tool),
            "real init missing Plan tool {tool}: {system}"
        );
    }

    host.stop("dut-cli");
    assert!(wait_terminal(&mut host, "dut-cli"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}
