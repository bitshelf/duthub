//! Integration tests for the Agent bundle, sandbox, listeners, and transcript.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings};
use duthub_core::lease::{ControlLeaseManager, WriteIdentity};
use duthub_core::{ApprovalError, AuditSink};
use duthub_desktop::agent_host::acp::{AcpClient, AcpEvent};
use duthub_desktop::agent_host::bundle;
use duthub_desktop::agent_host::listener;
use duthub_desktop::agent_host::model_env::{self, GooseBinding};
use duthub_desktop::agent_host::sandbox;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, DutInfo, DutSummary, LogReadResult, McpState,
    ProcessResultDto,
};
use serde_json::{Value, json};

struct StubState;

impl McpState for StubState {
    fn list_duts(&self) -> Vec<DutSummary> {
        Vec::new()
    }

    fn dut_info(&self, _dut_id: &str) -> Option<DutInfo> {
        None
    }

    fn serial_read(&self, _dut_id: &str, _since_seq: u64, _limit: usize) -> Option<LogReadResult> {
        None
    }

    fn serial_write(
        &self,
        _dut_id: &str,
        _text: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub".into())
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
        Err("stub".into())
    }

    fn control_power(
        &self,
        _dut_id: &str,
        _on: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub".into())
    }

    fn control_reset(
        &self,
        _dut_id: &str,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub".into())
    }

    fn control_power_key(
        &self,
        _dut_id: &str,
        _long_press: bool,
        _approval_token: &str,
        _writer: WriteIdentity,
    ) -> Result<(), String> {
        Err("stub".into())
    }

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("stub".into())
    }

    fn approval_issue(&self, dut_id: &str, action: &str, _args_json: &str) -> ApprovalIssueResult {
        ApprovalIssueResult {
            token: "stub".into(),
            dut_id: dut_id.into(),
            action: action.into(),
            expires_in_secs: 0,
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
            kind: ApprovalError::Missing,
        })
    }
}

fn access_context() -> (Arc<AccessContext>, Arc<AgentAccessRuntime>) {
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let lease = Arc::new(ControlLeaseManager::new(None));
    let audit = AuditSink::memory();
    (
        Arc::new(AccessContext::new(auth.clone(), lease, audit)),
        auth,
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("duthub-agent-host-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
}

fn discover_body() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": { "name": "duthub-test", "version": "1" },
                "io.modelcontextprotocol/clientCapabilities": {},
            }
        }
    })
    .to_string()
}

fn http_discover(port: u16, token: &str) -> std::io::Result<String> {
    let body = discover_body();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         MCP-Protocol-Version: 2026-07-28\r\nMcp-Method: server/discover\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => response.push_str(&String::from_utf8_lossy(&chunk[..read])),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(response)
}

/// goose is installed from the tool downloads page: no bytes
/// are compiled into duthub. `DUTHUB_TEST_GOOSE` (absolute path to a
/// goose executable) wins, otherwise the product resolver decides.
fn test_goose_program() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("DUTHUB_TEST_GOOSE") {
        let path = PathBuf::from(raw);
        assert!(
            path.is_absolute() && path.is_file(),
            "DUTHUB_TEST_GOOSE must point to an existing goose executable by absolute path: {}",
            path.display()
        );
        return Some(path);
    }
    duthub_desktop::agent_host::registry::goose_install_state()
        .program()
        .map(Path::to_path_buf)
}

/// Loud skip: goose is no longer embedded, so ACP tests need an installed one.
fn skip_without_goose() {
    eprintln!(
        "SKIP: Goose is unavailable. Install it from Tool Downloads or set DUTHUB_TEST_GOOSE to the executable."
    );
}

/// The removed embedded-asset contract: nothing ships under
/// `assets/goose/` and `bundle.rs` compiles no asset in.
#[test]
fn embedded_goose_assets_are_gone() {
    let assets = manifest_dir().join("assets").join("goose");
    assert!(
        !assets.exists(),
        "The embedded goose asset directory must be absent: {}",
        assets.display()
    );
    for name in [
        "goose-acp.exe",
        "goose-acp-x86_64-pc-windows-msvc.zip",
        "LICENSE",
        "NOTICE",
        "manifest.json",
    ] {
        assert!(
            !assets.join(name).exists(),
            "Embedded asset {name} must be absent from the repository"
        );
    }

    // Source contract: the build must not re-embed a binary or a licence file.
    let source = std::fs::read_to_string(manifest_dir().join("src/agent_host/bundle.rs"))
        .expect("read bundle.rs");
    for forbidden in ["include_bytes!", "include_str!", "assets/goose"] {
        assert!(
            !source.contains(forbidden),
            "bundle.rs must not embed assets: found {forbidden}"
        );
    }
}

/// The compatibility surface `pages.rs` still calls: not embedded, licence and
/// notice ship with the upstream package, no invented signature claim.
#[test]
fn bundle_compat_accessors_report_not_embedded() {
    use duthub_desktop::agent_host::GOOSE_VERSION;

    // GOOSE_VERSION is the recommended/minimum version, single-sourced with the
    // launch gate.
    assert_eq!(GOOSE_VERSION, duthub_desktop::agent_host::MIN_GOOSE_VERSION);

    let status = bundle::authenticode_status();
    assert!(status.contains("未内置"), "{status}");
    assert!(!status.eq_ignore_ascii_case("NotSigned"), "{status}");
    assert!(!status.eq_ignore_ascii_case("Valid"), "{status}");

    let license = bundle::license_text();
    assert!(license.contains("Apache License 2.0"), "{license}");
    assert!(license.contains("上游发行包"), "{license}");
    assert!(
        !license.contains("TERMS AND CONDITIONS"),
        "The Apache license text should not be embedded here"
    );

    let notice = bundle::notice_text();
    assert!(notice.contains("AAIF/goose"), "{notice}");
    assert!(notice.contains("上游发行包"), "{notice}");

    let manifest: Value = serde_json::from_str(bundle::manifest_json()).expect("manifest JSON");
    assert_eq!(manifest["embedded"], false);
    assert_eq!(manifest["distribution"], "downloaded");
    assert_eq!(manifest["license"], "Apache-2.0");
    assert!(bundle::manifest().is_ok());
}

/// No embedded install any more: `resolve(None)` reports it instead of
/// unpacking bytes, while an explicit override is still hash-verified.
#[test]
fn resolve_has_no_embedded_install_and_verifies_overrides() {
    let work = temp_dir("bundle");
    let error = bundle::resolve(&work, None).expect_err("there is no bundled copy to resolve");
    assert!(error.to_string().contains("内嵌 goose 已移除"), "{error}");
    assert!(
        !work.join("agent").exists(),
        "resolve must not install assets into the work directory"
    );

    // External override: full SHA-256 recomputed at launch.
    let exe = work.join("goose-candidate.exe");
    std::fs::write(&exe, b"candidate-bytes").unwrap();
    let sha = duthub_core::store::sha256_hex(b"candidate-bytes");
    assert_eq!(bundle::resolve(&work, Some((&exe, &sha))).unwrap(), exe);
    let wrong = "0".repeat(64);
    assert!(bundle::resolve(&work, Some((&exe, &wrong))).is_err());
    let _ = std::fs::remove_dir_all(&work);
}

#[cfg(windows)]
#[test]
fn probe_version_of_the_installed_goose() {
    let Some(exe) = test_goose_program() else {
        skip_without_goose();
        return;
    };
    let line = bundle::probe_version(&exe).unwrap();
    assert!(
        duthub_desktop::agent_host::registry::version_at_least(
            &line,
            duthub_desktop::agent_host::MIN_GOOSE_VERSION
        ),
        "The installed goose must meet the version requirement: `{line}`"
    );
}

#[test]
fn locale_catalogs_have_identical_keys_and_about_copy() {
    let locales = manifest_dir().join("locales");
    let read = |name: &str| -> serde_json::Map<String, Value> {
        let text = std::fs::read_to_string(locales.join(name)).unwrap();
        serde_json::from_str::<Value>(&text)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    };
    let zh = read("zh-CN.json");
    let en = read("en-US.json");
    assert_eq!(
        zh.len(),
        en.len(),
        "Locales must have the same number of keys"
    );
    for key in zh.keys() {
        assert!(en.contains_key(key), "en-US is missing {key}");
    }
    for key in en.keys() {
        assert!(zh.contains_key(key), "zh-CN is missing {key}");
    }
    for key in ["aboutTitle", "gooseAuthenticodeNotSigned"] {
        for catalog in [&zh, &en] {
            assert!(
                catalog
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty()),
                "Missing nonempty interface text: {key}"
            );
        }
    }
}

#[test]
fn sandbox_prepare_uses_per_dut_layout() {
    let work = temp_dir("sandbox");
    let paths = sandbox::prepare(&work, "dut-A").unwrap();
    assert!(paths.sandbox.is_dir());
    assert!(paths.goose_root.is_dir());
    assert!(
        paths.temp.is_dir(),
        "The TEMP directory must exist inside the sandbox"
    );
    assert!(paths.temp.starts_with(&paths.sandbox));
    assert!(
        paths.dut_dir.ends_with(
            Path::new("agent")
                .join("dut")
                .join(duthub_core::store::agent_dut_key("dut-A").unwrap())
        )
    );
    assert!(sandbox::prepare(&work, "").is_err());
    let _ = std::fs::remove_dir_all(&work);
}

/// Verify the child's scoped environment.
/// User directory variables such as USERPROFILE and APPDATA are excluded.
/// TEMP and TMP point into the session sandbox.
/// System variables and permitted caller overrides remain available.
#[cfg(windows)]
#[test]
fn acp_child_env_excludes_user_dirs_and_uses_sandbox_temp() {
    use std::collections::HashMap;

    let work = temp_dir("env-scope");
    let paths = sandbox::prepare(&work, "dut-env-scope").unwrap();
    let cmd = std::env::var_os("COMSPEC").map_or_else(|| PathBuf::from("cmd.exe"), PathBuf::from);
    let acp = AcpClient::spawn(
        &cmd,
        &["/c".into(), "set 1>&2".into()],
        &paths.sandbox,
        &[
            ("DUTHUB_TEST_MARKER".into(), "1".into()),
            // Sandbox paths take precedence over explicit overrides of TEMP and TMP.
            (
                "TEMP".into(),
                std::env::temp_dir().to_string_lossy().into_owned(),
            ),
            (
                "TMP".into(),
                std::env::temp_dir().to_string_lossy().into_owned(),
            ),
        ],
        256 * 1024,
    )
    .unwrap();
    assert!(
        acp.wait_timeout(Duration::from_secs(10)).unwrap().is_some(),
        "cmd /c set did not exit"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    while std::time::Instant::now() < deadline {
        output = acp.stderr_tail();
        if output.contains("TEMP=") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        output.contains("TEMP="),
        "The child environment was not captured: {output:?}"
    );
    let env: HashMap<String, String> = output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_ascii_uppercase(), value.trim().to_owned()))
        })
        .collect();

    for key in ["USERPROFILE", "APPDATA", "HOME", "HOMEDRIVE", "HOMEPATH"] {
        assert!(
            !env.contains_key(key),
            "The child must not inherit {key}: {:?}",
            env.get(key)
        );
    }
    // Windows PSEC requires `LOCALAPPDATA` to be present when default
    // environment inheritance is disabled and appends `Packages\sandbox.<guid>\AC`
    // beneath the supplied path; verify it stays inside `<cell>/home/AppData/Local`
    // and never inherits host `%LOCALAPPDATA%`.
    let local_app_data = env
        .get("LOCALAPPDATA")
        .expect("LOCALAPPDATA must be set to the sandbox cell");
    let expected_local_app_data = paths.sandbox.join("home").join("AppData").join("Local");
    assert!(
        PathBuf::from(local_app_data).starts_with(&expected_local_app_data),
        "LOCALAPPDATA must be inside sandbox/home/AppData/Local: {local_app_data}"
    );
    if let Some(host_local) = std::env::var_os("LOCALAPPDATA") {
        assert_ne!(
            PathBuf::from(local_app_data),
            PathBuf::from(host_local),
            "LOCALAPPDATA must not inherit the host user directory"
        );
    }
    let temp = env.get("TEMP").expect("TEMP must be set");
    let short_temp = env.get("TMP").expect("TMP must be set");
    // Windows PSEC may redirect TEMP/TMP to `<LOCALAPPDATA>\Packages\sandbox.<guid>\AC\Temp`
    // beneath the cell root; both remain strictly confined inside `paths.sandbox`.
    assert!(
        PathBuf::from(temp).starts_with(&paths.sandbox),
        "TEMP must be inside the sandbox cell: {temp}"
    );
    assert!(
        PathBuf::from(short_temp).starts_with(&paths.sandbox),
        "TMP must be inside the sandbox cell: {short_temp}"
    );
    assert_ne!(
        PathBuf::from(temp),
        std::env::temp_dir(),
        "TEMP must not inherit the user temporary directory"
    );
    assert!(paths.temp.is_dir(), "The TEMP directory must exist");

    for key in [
        "PATH",
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "PROCESSOR_ARCHITECTURE",
        "NUMBER_OF_PROCESSORS",
    ] {
        assert!(
            env.contains_key(key),
            "Missing system environment variable {key}: {env:?}"
        );
    }
    assert_eq!(
        env.get("DUTHUB_TEST_MARKER").map(String::as_str),
        Some("1"),
        "Caller-supplied environment variables must be preserved"
    );

    acp.close_stdin();
    acp.kill();
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn session_listener_authenticates_scoped_token_and_revokes() {
    let (access, auth) = access_context();
    let listener = listener::issue(Arc::new(StubState), access, auth.clone(), "dut-1").unwrap();
    assert!(listener.id.starts_with("session-"));
    assert!(listener.port > 0);
    let debug = format!("{listener:?}");
    assert!(!debug.contains(&listener.token));
    assert!(debug.contains("[REDACTED]"));

    let response = http_discover(listener.port, &listener.token).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected response: {response}"
    );
    assert!(response.contains("supportedVersions"), "{response}");

    let denied = http_discover(listener.port, "dth-wrong-token").unwrap();
    assert!(
        denied.starts_with("HTTP/1.1 401"),
        "unexpected response: {denied}"
    );

    let id = listener.id.clone();
    let port = listener.port;
    let token = listener.token.clone();
    assert!(auth.is_runtime_listener(&id));
    listener::revoke(listener, &auth);
    assert!(!auth.is_runtime_listener(&id));
    // After revocation, a new connection must fail authentication or fail to connect.
    if let Ok(response) = http_discover(port, &token) {
        assert!(
            response.starts_with("HTTP/1.1 401"),
            "revoked token must be rejected: {response}"
        );
    }
}

#[test]
fn goose_binding_and_env_do_not_expose_key_in_debug() {
    let binding = GooseBinding {
        provider: duthub_core::Provider::Openai,
        base_url: "https://api.example.com/v1".into(),
        model: "fake-model".into(),
        api_key: "sk-agent-host-secret".into(),
    };
    let root = temp_dir("env").join("goose");
    std::fs::create_dir_all(&root).unwrap();
    let env = model_env::goose_env(&binding, &root).unwrap();
    assert!(env.contains(&("OPENAI_API_KEY".into(), "sk-agent-host-secret".into())));
    assert!(env.contains(&("GOOSE_MODEL".into(), "fake-model".into())));
    assert!(
        env.contains(&("GOOSE_MODE".into(), "approve".into())),
        "GOOSE_MODE must be approve"
    );
    assert!(env.contains(&(
        "GOOSE_PATH_ROOT".into(),
        root.to_string_lossy().into_owned()
    )));
    assert!(!format!("{binding:?}").contains("sk-agent-host-secret"));
    let _ = std::fs::remove_dir_all(root.parent().unwrap());
}

#[cfg(windows)]
#[test]
fn acp_client_emits_exited_event_after_process_end() {
    // No goose needed: the contract under test is the ACP client's exit event,
    // so any process that terminates with 0 will do.
    let exe = std::env::var_os("COMSPEC").map_or_else(|| PathBuf::from("cmd.exe"), PathBuf::from);
    let cwd = temp_dir("exit-event");
    let acp = AcpClient::spawn(
        &exe,
        &["/C".into(), "exit".into(), "0".into()],
        &cwd,
        &[],
        4096,
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if acp.try_wait().unwrap().is_some() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "Process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    let events = acp.drain();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AcpEvent::Exited(Some(0)))),
        "Missing Exited event: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn acp_client_rejects_missing_working_directory() {
    // The cwd check must fire before the program is looked at, so the program
    // path only has to exist (the embedded goose binary is gone).
    let exe = std::env::current_exe().expect("locate the current test executable");
    let Err(error) = AcpClient::spawn(&exe, &[], Path::new("Z:\\duthub-missing-cwd"), &[], 4096)
    else {
        panic!("The client must reject a missing working directory");
    };
    assert!(error.to_string().contains("工作目录不存在"));
}
