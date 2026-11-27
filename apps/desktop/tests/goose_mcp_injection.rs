//! W2 验收：真实 goose 消费 `session/new.mcpServers`，
//! 且 managed config 关闭 developer / summon / tom / Extension Manager。
//!
//! 2026-11-24 起 goose 不再内嵌：这些用例需要本机已安装的 goose
//! （「工具下载」页下载，或 `DUTHUB_TEST_GOOSE` 指向可执行文件），否则带清晰原因跳过。
#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use duthub_desktop::agent_host::acp::{AcpClient, McpServerRef};
use duthub_desktop::agent_host::model_env::{self, GooseBinding};
use duthub_desktop::agent_host::{registry, sandbox};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
struct Recorded {
    authorization: Option<String>,
    method: String,
    protocol_version: Option<String>,
}

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<Recorded>>,
}

impl Recorder {
    fn wait_for_method(&self, method: &str, token: &str) -> Option<Recorded> {
        let expected = format!("Bearer {token}");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(found) = self
                .requests
                .lock()
                .unwrap()
                .iter()
                .find(|request| {
                    request.method == method
                        && request.authorization.as_deref() == Some(expected.as_str())
                })
                .cloned()
            {
                return Some(found);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct FakeMcp {
    base_url: String,
    recorder: Arc<Recorder>,
    _runtime: tokio::runtime::Runtime,
}

fn spawn_fake_mcp() -> FakeMcp {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let recorder = Arc::new(Recorder::default());
    let app = Router::new()
        .route("/mcp", post(mcp_handler))
        .with_state(recorder.clone());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        axum::serve(listener, app).await.unwrap();
    });
    FakeMcp {
        base_url: format!("http://{address}"),
        recorder,
        _runtime: runtime,
    }
}

async fn mcp_handler(
    State(recorder): State<Arc<Recorder>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad json").into_response(),
    };
    recorder.requests.lock().unwrap().push(Recorded {
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        method: value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        protocol_version: headers
            .get("mcp-protocol-version")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    });
    let Some(id) = value.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match value.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "fake-mcp", "version": "1.0" },
        }),
        // rmcp DiscoverResult 的完整形状（缺 resultType/ttlMs/cacheScope 会退化成
        // CustomResult，goose 会报 "expect initialized result"）。
        "server/discover" => json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": { "tools": {} },
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {
                "io.modelcontextprotocol/serverInfo": { "name": "fake-mcp", "version": "1.0" }
            },
        }),
        "tools/list" => json!({ "tools": [] }),
        _ => json!({}),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
    )
        .into_response()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("duthub-goose-mcp-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn extension_names(extension_results: &Value) -> Vec<&str> {
    extension_results
        .as_array()
        .map_or_default(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                .collect()
        })
}

fn binding() -> GooseBinding {
    GooseBinding {
        provider: duthub_core::Provider::Openai,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "fake-model".into(),
        api_key: "sk-injection-secret".into(),
    }
}

fn close(acp: AcpClient) {
    acp.close_stdin();
    let _ = acp.wait_timeout(Duration::from_secs(10));
    acp.kill();
}

// ---------------------------------------------------------------------------
// Test goose fixture (2026-11-24: goose is downloaded, no longer embedded)
// ---------------------------------------------------------------------------

/// `DUTHUB_TEST_GOOSE` (absolute path to a goose executable) wins, otherwise the
/// product resolver decides (downloaded install > PATH). `None` = not installed.
fn test_goose_program() -> Option<std::path::PathBuf> {
    if let Some(raw) = std::env::var_os("DUTHUB_TEST_GOOSE") {
        let path = std::path::PathBuf::from(raw);
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

#[test]
fn goose_consumes_injected_mcp_server_with_session_token() {
    let mcp = spawn_fake_mcp();
    let work = temp_dir("inject");
    let paths = sandbox::prepare(&work, "dut-inject").unwrap();
    model_env::write_config(&paths.goose_root).unwrap();
    let env = model_env::goose_env(&binding(), &paths.goose_root).unwrap();
    let Some(exe) = test_goose_program() else {
        skip_without_goose();
        return;
    };
    let acp = AcpClient::spawn(&exe, &[], &paths.sandbox, &env, 64 * 1024).unwrap();
    let info = acp.initialize(Duration::from_secs(20)).unwrap();
    assert_eq!(info.agent_name, "goose");
    // 不再内嵌固定构建：断言「goose 报出的版本通过产品版本门槛」，而不是
    // 与某个内嵌版本逐字相等（下载安装的是上游 latest，版本随上游变化）。
    assert!(
        registry::version_at_least(&info.agent_version, registry::MIN_GOOSE_VERSION),
        "goose 版本必须 >= {}，实际 {}",
        registry::MIN_GOOSE_VERSION,
        info.agent_version
    );
    assert!(info.mcp_http, "goose 必须声明 mcpCapabilities.http");

    let token = "dth-injection-token-1234567890";
    let url = format!("{}/mcp", mcp.base_url);
    let session = acp
        .new_session(
            &paths.sandbox,
            &McpServerRef {
                name: "duthub",
                url: &url,
                token,
            },
            Duration::from_secs(30),
        )
        .unwrap();

    // F1：GOOSE_MODE=approve（auto 下权限请求永不发生，工具面无法被审批约束）。
    assert_eq!(session.current_mode.as_deref(), Some("approve"));

    // 真实 goose + managed config：四个内置扩展一个都不启动，只剩注入的 duthub。
    let names = extension_names(&session.extension_results);
    for builtin in ["Extension Manager", "summon", "developer", "tom"] {
        assert!(
            !names.contains(&builtin),
            "managed config 必须关闭 {builtin}，实际 {names:?}"
        );
    }
    assert!(
        names.contains(&"duthub"),
        "注入的 MCP server 必须出现在 extensionResults：{names:?}"
    );
    assert!(
        session
            .extension_results
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "duthub" && entry["success"] == true),
        "duthub MCP 扩展必须成功：{}",
        session.extension_results
    );
    // 运行时强校验与真实 goose 输出一致（恰好 1 个 duthub success=true）。
    duthub_desktop::agent_host::host::validate_extension_results(&session.extension_results)
        .expect("真实 goose 的工具面必须通过启动强校验");

    let discover = mcp
        .recorder
        .wait_for_method("server/discover", token)
        .expect("goose 必须连接 session/new 注入的 MCP server 并携带 Bearer token");
    assert_eq!(
        discover.authorization.as_deref(),
        Some("Bearer dth-injection-token-1234567890")
    );
    assert_eq!(discover.protocol_version.as_deref(), Some("2026-07-28"));

    close(acp);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn goose_load_session_restores_persisted_session() {
    let mcp = spawn_fake_mcp();
    let work = temp_dir("load");
    let paths = sandbox::prepare(&work, "dut-load").unwrap();
    model_env::write_config(&paths.goose_root).unwrap();
    let env = model_env::goose_env(&binding(), &paths.goose_root).unwrap();
    let Some(exe) = test_goose_program() else {
        skip_without_goose();
        return;
    };
    let token = "dth-load-token-1234567890";
    let url = format!("{}/mcp", mcp.base_url);
    let server = McpServerRef {
        name: "duthub",
        url: &url,
        token,
    };

    let first = AcpClient::spawn(&exe, &[], &paths.sandbox, &env, 64 * 1024).unwrap();
    first.initialize(Duration::from_secs(20)).unwrap();
    let session = first
        .new_session(&paths.sandbox, &server, Duration::from_secs(30))
        .unwrap();
    close(first);

    let second = AcpClient::spawn(&exe, &[], &paths.sandbox, &env, 64 * 1024).unwrap();
    second.initialize(Duration::from_secs(20)).unwrap();
    assert!(
        second
            .load_session(
                &session.session_id,
                &paths.sandbox,
                &server,
                Duration::from_secs(30)
            )
            .unwrap(),
        "goose 应支持 session/load"
    );
    close(second);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn managed_config_is_what_disables_the_four_builtins() {
    let mcp = spawn_fake_mcp();
    let work = temp_dir("builtins");
    let binding = binding();

    // 对照：不写 managed config → 四个内置扩展都出现在 extensionResults。
    let default_paths = sandbox::prepare(&work, "dut-default").unwrap();
    let default_env = model_env::goose_env(&binding, &default_paths.goose_root).unwrap();
    let Some(exe) = test_goose_program() else {
        skip_without_goose();
        return;
    };
    let default_acp =
        AcpClient::spawn(&exe, &[], &default_paths.sandbox, &default_env, 64 * 1024).unwrap();
    default_acp.initialize(Duration::from_secs(20)).unwrap();
    let url = format!("{}/mcp", mcp.base_url);
    let default_session = default_acp
        .new_session(
            &default_paths.sandbox,
            &McpServerRef {
                name: "duthub",
                url: &url,
                token: "dth-default-token",
            },
            Duration::from_secs(30),
        )
        .unwrap();
    let names = extension_names(&default_session.extension_results);
    for expected in ["Extension Manager", "summon", "developer", "tom"] {
        assert!(
            names.contains(&expected),
            "未配置 managed config 时应有 {expected}，实际 {names:?}"
        );
    }
    close(default_acp);

    // 实验组：managed config 写入后为空。
    let managed_paths = sandbox::prepare(&work, "dut-managed").unwrap();
    model_env::write_config(&managed_paths.goose_root).unwrap();
    let managed_env = model_env::goose_env(&binding, &managed_paths.goose_root).unwrap();
    let managed_acp =
        AcpClient::spawn(&exe, &[], &managed_paths.sandbox, &managed_env, 64 * 1024).unwrap();
    managed_acp.initialize(Duration::from_secs(20)).unwrap();
    let managed_session = managed_acp
        .new_session(
            &managed_paths.sandbox,
            &McpServerRef {
                name: "duthub",
                url: &url,
                token: "dth-managed-token",
            },
            Duration::from_secs(30),
        )
        .unwrap();
    let managed_names = extension_names(&managed_session.extension_results);
    for builtin in ["Extension Manager", "summon", "developer", "tom"] {
        assert!(
            !managed_names.contains(&builtin),
            "managed config 必须关闭 {builtin}，实际 {managed_names:?}"
        );
    }
    close(managed_acp);

    let _ = std::fs::remove_dir_all(&work);
}
