//! W2 验收：provider 环境变量真实生效（goose 用注入的 key/model/base URL 调用模型）。
//!
//! 默认测试用本地假 OpenAI 兼容服务（离线、确定）；真实模型调用是 `#[ignore]`，
//! 需要 owner 的 key（`DUTHUB_GOOSE_LIVE=1`）。
#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use duthub_core::model::{ModelProvider, ProviderModel};
use duthub_desktop::agent_host::acp::{AcpClient, AcpEvent, McpServerRef};
use duthub_desktop::agent_host::model_env::{self, GooseBinding};
use duthub_desktop::agent_host::registry;
use duthub_desktop::agent_host::sandbox;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    authorization: Option<String>,
    model: Option<String>,
}

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<Recorded>>,
}

impl Recorder {
    fn has_model_call(&self, expected_key: &str) -> bool {
        self.requests.lock().unwrap().iter().any(|request| {
            request.path == "/v1/chat/completions"
                && request.authorization.as_deref() == Some(expected_key)
                && request.model.as_deref() == Some("fake-model")
        })
    }
}

struct FakeOpenAi {
    base_url: String,
    recorder: Arc<Recorder>,
    _runtime: tokio::runtime::Runtime,
}

fn spawn_fake_openai() -> FakeOpenAi {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let recorder = Arc::new(Recorder::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(models))
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
    FakeOpenAi {
        base_url: format!("http://{address}/v1"),
        recorder,
        _runtime: runtime,
    }
}

async fn models() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json!({
            "object": "list",
            "data": [ { "id": "fake-model", "object": "model", "owned_by": "duthub-test" } ],
        })
        .to_string(),
    )
        .into_response()
}

async fn chat_completions(
    State(recorder): State<Arc<Recorder>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    recorder.requests.lock().unwrap().push(Recorded {
        path: "/v1/chat/completions".into(),
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
    });
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("fake-model");
    let first = json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": "Hello from fake model" },
            "finish_reason": null,
        }],
    });
    let finish = json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
    });
    let usage = json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [],
        "usage": { "prompt_tokens": 1, "completion_tokens": 4, "total_tokens": 5 },
    });
    let sse = format!("data: {first}\n\ndata: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

// ── 一个可达的假 MCP server（session/new 必须注入；完整 DiscoverResult 形状）──

#[derive(Default)]
struct McpRecorder {
    _requests: Mutex<Vec<String>>,
}

struct FakeMcp {
    base_url: String,
    _runtime: tokio::runtime::Runtime,
}

fn spawn_fake_mcp() -> FakeMcp {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/mcp", post(mcp_handler))
        .with_state(Arc::new(McpRecorder::default()));
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
        _runtime: runtime,
    }
}

async fn mcp_handler(State(recorder): State<Arc<McpRecorder>>, body: String) -> Response {
    recorder._requests.lock().unwrap().push(body.clone());
    let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let Some(id) = value.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match value.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "fake-mcp", "version": "1.0" },
        }),
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
    let dir = std::env::temp_dir().join(format!("duthub-goose-env-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings_with_catalog(base_url: &str) -> duthub_core::SettingsFile {
    let mut settings = duthub_core::store::default_settings();
    settings.agent.providers = vec![ModelProvider {
        id: "fake-provider".into(),
        name: "Fake Provider".into(),
        base_url: base_url.into(),
        protocol: duthub_core::Provider::Openai,
        enabled: true,
        models: vec![ProviderModel {
            id: "fake-model".into(),
            enabled: true,
        }],
    }];
    settings
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
fn provider_env_vars_drive_a_real_goose_turn() {
    const KEY: &str = "sk-model-env-secret";
    let openai = spawn_fake_openai();
    let mcp = spawn_fake_mcp();
    let settings = settings_with_catalog(&openai.base_url);
    let credential = duthub_core::model_provider_credential_id(
        "fake-provider",
        duthub_core::Provider::Openai,
        &openai.base_url,
    );
    let binding = model_env::resolve_binding_with(&settings, |id| {
        if id == credential {
            Ok(Some(KEY.into()))
        } else {
            Ok(None)
        }
    })
    .unwrap();
    assert_eq!(binding.model, "fake-model");
    assert_eq!(binding.api_key, KEY);

    let work = temp_dir("env");
    let paths = sandbox::prepare(&work, "dut-env").unwrap();
    model_env::write_config(&paths.goose_root).unwrap();
    let env = model_env::goose_env(&binding, &paths.goose_root).unwrap();
    assert!(env.contains(&("OPENAI_API_KEY".into(), KEY.into())));
    assert!(env.contains(&("OPENAI_BASE_URL".into(), openai.base_url.clone())));
    assert!(env.contains(&("GOOSE_MODEL".into(), "fake-model".into())));

    let exe = match test_goose_program() {
        Some(exe) => exe,
        None => {
            skip_without_goose();
            return;
        }
    };
    let acp = AcpClient::spawn(&exe, &[], &paths.sandbox, &env, 64 * 1024).unwrap();
    acp.initialize(Duration::from_secs(20)).unwrap();
    let mcp_url = format!("{}/mcp", mcp.base_url);
    let session = acp
        .new_session(
            &paths.sandbox,
            &McpServerRef {
                name: "duthub",
                url: &mcp_url,
                token: "dth-model-env-token",
            },
            Duration::from_secs(30),
        )
        .unwrap();
    acp.prompt(&session.session_id, "hello fake model").unwrap();

    let mut chunks = String::new();
    let mut done = false;
    let mut protocol_errors = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        for event in acp.drain() {
            match event {
                AcpEvent::AgentChunk(text) => chunks.push_str(&text),
                AcpEvent::PromptDone { .. } => done = true,
                AcpEvent::ProtocolError(error) => protocol_errors.push(error),
                _ => {}
            }
        }
        if done {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        done,
        "真实 goose 未完成 prompt；stderr: {}\nprotocol: {protocol_errors:?}",
        acp.stderr_tail()
    );
    assert!(
        chunks.contains("Hello from fake model"),
        "未收到模型回复 chunks: {chunks:?}"
    );
    assert!(
        openai.recorder.has_model_call(&format!("Bearer {KEY}")),
        "假 OpenAI 服务未收到带 Bearer key 的 /v1/chat/completions：{:?}",
        openai.recorder.requests.lock().unwrap()
    );
    let stderr = acp.stderr_tail();
    assert!(!stderr.contains(KEY), "stderr 泄漏 API key: {stderr}");

    close(acp);
    let _ = std::fs::remove_dir_all(&work);
}

/// 真实模型调用（不计入门禁）：需要 owner 的 key。
/// `DUTHUB_GOOSE_LIVE=1 DUTHUB_GOOSE_LIVE_PROVIDER=openai
///  DUTHUB_GOOSE_LIVE_BASE_URL=... DUTHUB_GOOSE_LIVE_MODEL=...
///  DUTHUB_GOOSE_LIVE_API_KEY=... cargo test -p duthub-desktop --test goose_model_env -- --ignored`
#[test]
#[ignore]
fn live_model_call() {
    assert_eq!(
        std::env::var("DUTHUB_GOOSE_LIVE").as_deref(),
        Ok("1"),
        "需要 DUTHUB_GOOSE_LIVE=1 与 owner 的 key，未配置不得当作通过"
    );
    let provider = std::env::var("DUTHUB_GOOSE_LIVE_PROVIDER")
        .ok()
        .and_then(|value| duthub_core::Provider::from_setting(&value))
        .unwrap_or(duthub_core::Provider::Openai);
    let base_url = std::env::var("DUTHUB_GOOSE_LIVE_BASE_URL").expect("DUTHUB_GOOSE_LIVE_BASE_URL");
    let model = std::env::var("DUTHUB_GOOSE_LIVE_MODEL").expect("DUTHUB_GOOSE_LIVE_MODEL");
    let api_key = std::env::var("DUTHUB_GOOSE_LIVE_API_KEY").expect("DUTHUB_GOOSE_LIVE_API_KEY");
    let binding = GooseBinding {
        provider,
        base_url,
        model,
        api_key,
    };
    let work = temp_dir("live");
    let paths = sandbox::prepare(&work, "dut-live").unwrap();
    model_env::write_config(&paths.goose_root).unwrap();
    let env = model_env::goose_env(&binding, &paths.goose_root).unwrap();
    let exe = match test_goose_program() {
        Some(exe) => exe,
        None => {
            skip_without_goose();
            return;
        }
    };
    let acp = AcpClient::spawn(&exe, &[], &paths.sandbox, &env, 64 * 1024).unwrap();
    acp.initialize(Duration::from_secs(20)).unwrap();
    let mcp_url = "http://127.0.0.1:1/mcp";
    let session = acp
        .new_session(
            &paths.sandbox,
            &McpServerRef {
                name: "duthub",
                url: mcp_url,
                token: "dth-live-token",
            },
            Duration::from_secs(30),
        )
        .unwrap();
    acp.prompt(&session.session_id, "Reply with OK.").unwrap();
    let mut chunks = String::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut done = false;
    while Instant::now() < deadline {
        for event in acp.drain() {
            match event {
                AcpEvent::AgentChunk(text) => chunks.push_str(&text),
                AcpEvent::PromptDone { .. } => done = true,
                _ => {}
            }
        }
        if done {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    close(acp);
    assert!(done, "真实模型调用未完成：{}", chunks);
    assert!(!chunks.trim().is_empty(), "真实模型调用没有回复");
    let _ = std::fs::remove_dir_all(&work);
}
