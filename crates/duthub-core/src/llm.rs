//! LLM 连接测试（`docs/MODEL_CREDENTIALS.md`）：六种 provider 的最小化实现。
//!
//! - 仅测试端点（chat completions 的 1-token 探针）；Agent 任务执行器仍为 UI 演示。
//! - API Key 只经内存传递；调用方负责写入 OS 密钥库（`keys.rs`），永不落 settings.json。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Openai,
    Anthropic,
    Gemini,
    Deepseek,
    Qwen,
    Compatible,
}

impl Provider {
    pub fn from_setting(value: &str) -> Option<Provider> {
        match value {
            "openai" => Some(Provider::Openai),
            "anthropic" => Some(Provider::Anthropic),
            "google" => Some(Provider::Gemini),
            "deepseek" => Some(Provider::Deepseek),
            "qwen" => Some(Provider::Qwen),
            "compatible" => Some(Provider::Compatible),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Openai => "OpenAI",
            Provider::Anthropic => "Anthropic",
            Provider::Gemini => "Google Gemini",
            Provider::Deepseek => "DeepSeek",
            Provider::Qwen => "Qwen / DashScope",
            Provider::Compatible => "OpenAI Compatible",
        }
    }

    /// 默认 base URL（Compatible 必须显式提供）
    pub fn default_base(self) -> &'static str {
        match self {
            Provider::Openai => "https://api.openai.com/v1",
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Provider::Deepseek => "https://api.deepseek.com",
            Provider::Qwen => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Provider::Compatible => "",
        }
    }
}

#[derive(Clone)]
pub struct TestRequest {
    pub provider: Provider,
    /// base URL（含 /v1 等）；空串用 provider 默认
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// 探针提示词（默认"请只回复 OK"）
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct TestResponse {
    pub latency_ms: u64,
    pub reply: String,
}

const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REPLY_CHARS: usize = 400;

pub fn validate_model_base(value: &str) -> Result<String, String> {
    let input = value.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(input).map_err(|_| "Base URL 无效".to_owned())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Base URL 必须是 HTTP/HTTPS 地址，且不能包含凭据、查询参数或片段".into());
    }
    Ok(input.to_owned())
}

/// Scope a stored provider credential to its API format and destination.
pub fn model_provider_credential_id(id: &str, provider: Provider, base_url: &str) -> String {
    use sha2::{Digest, Sha256};

    let destination = format!("{}\n{base_url}", provider.label());
    let fingerprint = Sha256::digest(destination.as_bytes());
    format!("provider/{id}/{fingerprint:x}")
}

fn endpoint(req: &TestRequest, suffix: &str) -> Result<String, String> {
    let base = if req.base_url.trim().is_empty() {
        validate_model_base(req.provider.default_base())?
    } else {
        validate_model_base(&req.base_url)?
    };
    let version = if req.provider == Provider::Anthropic && !base.ends_with("/v1") {
        "/v1"
    } else {
        ""
    };
    Ok(format!("{base}{version}/{suffix}"))
}

fn probe_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("HTTP 客户端构建失败: {error}"))
}

/// Test the provider's authenticated model-list endpoint without generating a reply.
pub async fn test_model_api(req: &TestRequest) -> Result<TestResponse, String> {
    if req.api_key.trim().is_empty() {
        return Err("API Key 为空".into());
    }
    let started = Instant::now();
    let request = probe_client()?.get(endpoint(req, "models")?);
    let request = match req.provider {
        Provider::Anthropic => request
            .header("x-api-key", req.api_key.trim())
            .header("anthropic-version", "2023-06-01"),
        Provider::Gemini => request.header("x-goog-api-key", req.api_key.trim()),
        _ => request.bearer_auth(req.api_key.trim()),
    };
    let response = request
        .send()
        .await
        .map_err(|error| format!("请求失败: {}", error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "模型列表响应无效".to_owned())?;
    if !body["data"].is_array() && !body["models"].is_array() {
        return Err("响应未包含模型列表".into());
    }
    Ok(TestResponse {
        latency_ms: started.elapsed().as_millis() as u64,
        reply: String::new(),
    })
}

/// 测试连接：返回 (延迟, 首条回复)。HTTP 非 200 → Err(含状态码与截断响应)。
pub async fn test_model_connection(req: &TestRequest) -> Result<TestResponse, String> {
    if req.api_key.trim().is_empty() {
        return Err("API Key 为空".into());
    }
    let prompt = if req.prompt.trim().is_empty() {
        "请只回复 OK".to_string()
    } else {
        req.prompt.trim().to_string()
    };

    let client = probe_client()?;

    let started = Instant::now();
    let reply = match req.provider {
        Provider::Openai | Provider::Deepseek | Provider::Qwen | Provider::Compatible => {
            let url = endpoint(req, "chat/completions")?;
            let body = serde_json::json!({
                "model": req.model,
                "messages": [
                    { "role": "user", "content": prompt }
                ],
                "max_tokens": 16,
            });
            let resp = client
                .post(&url)
                .bearer_auth(req.api_key.trim())
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("请求失败: {}", e.without_url()))?;
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            if status != 200 {
                return Err(format!(
                    "HTTP {status}: {}",
                    truncate_redacted(&text, &req.api_key, 200)
                ));
            }
            let v: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))?;
            probe_reply(v["choices"][0]["message"]["content"].as_str())?
        }
        Provider::Anthropic => {
            let url = endpoint(req, "messages")?;
            let body = serde_json::json!({
                "model": req.model,
                "max_tokens": 16,
                "messages": [
                    { "role": "user", "content": prompt }
                ]
            });
            let resp = client
                .post(&url)
                .header("x-api-key", req.api_key.trim())
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("请求失败: {}", e.without_url()))?;
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            if status != 200 {
                return Err(format!(
                    "HTTP {status}: {}",
                    truncate_redacted(&text, &req.api_key, 200)
                ));
            }
            let v: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))?;
            probe_reply(v["content"].as_array().and_then(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .find(|text| !text.trim().is_empty())
            }))?
        }
        Provider::Gemini => {
            let url = endpoint(req, &format!("models/{}:generateContent", req.model))?;
            let body = serde_json::json!({
                "contents": [ { "parts": [ { "text": prompt } ] } ],
                "generationConfig": { "maxOutputTokens": 16 }
            });
            let resp = client
                .post(&url)
                .header("x-goog-api-key", req.api_key.trim())
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("请求失败: {}", e.without_url()))?;
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            if status != 200 {
                return Err(format!(
                    "HTTP {status}: {}",
                    truncate_redacted(&text, &req.api_key, 200)
                ));
            }
            let v: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))?;
            probe_reply(v["candidates"][0]["content"]["parts"][0]["text"].as_str())?
        }
    };

    Ok(TestResponse {
        latency_ms: started.elapsed().as_millis() as u64,
        reply: truncate_redacted(&reply, &req.api_key, MAX_REPLY_CHARS),
    })
}

fn probe_reply(text: Option<&str>) -> Result<String, String> {
    text.map(str::trim)
        .filter(|reply| !reply.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "模型未返回文本".into())
}

fn truncate_redacted(text: &str, key: &str, limit: usize) -> String {
    text.replace(key.trim(), "[REDACTED]")
        .chars()
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_key_rejected_before_http() {
        let req = TestRequest {
            provider: Provider::Openai,
            base_url: String::new(),
            model: "gpt-4o-mini".into(),
            api_key: "  ".into(),
            prompt: String::new(),
        };
        let err = test_model_connection(&req).await.unwrap_err();
        assert!(err.contains("API Key"), "{err}");
    }

    #[test]
    fn provider_defaults_and_labels() {
        assert_eq!(Provider::Openai.default_base(), "https://api.openai.com/v1");
        assert_eq!(Provider::from_setting("google"), Some(Provider::Gemini));
        assert_eq!(Provider::Compatible.default_base(), "");
    }

    #[test]
    fn credentials_are_scoped_to_the_provider_destination() {
        let url = "https://example.com/v1";
        let key = model_provider_credential_id("example", Provider::Compatible, url);
        assert_ne!(
            key,
            model_provider_credential_id("other", Provider::Compatible, url)
        );
        assert_ne!(
            key,
            model_provider_credential_id("example", Provider::Anthropic, url)
        );
        assert_ne!(
            key,
            model_provider_credential_id(
                "example",
                Provider::Compatible,
                "https://other.example/v1"
            )
        );
    }

    #[tokio::test]
    async fn anthropic_probes_use_distinct_authenticated_endpoints() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (route, body) in [
                ("GET /v1/models HTTP/1.1", r#"{"data":[]}"#),
                (
                    "POST /v1/messages HTTP/1.1",
                    r#"{"content":[{"type":"text","text":"OK"}]}"#,
                ),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut start_line = String::new();
                reader.read_line(&mut start_line).unwrap();
                assert_eq!(start_line.trim_end(), route);
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                assert!(headers.contains("x-api-key: local-fixture-key\r\n"));
                assert!(headers.contains("anthropic-version: 2023-06-01\r\n"));
                if route.starts_with("POST") {
                    let mut payload = vec![0; length];
                    reader.read_exact(&mut payload).unwrap();
                    let message: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                    assert_eq!(message["model"], "fixture-model");
                }
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let req = TestRequest {
            provider: Provider::Anthropic,
            base_url,
            model: "fixture-model".into(),
            api_key: "local-fixture-key".into(),
            prompt: "Reply with OK.".into(),
        };
        assert!(test_model_api(&req).await.unwrap().reply.is_empty());
        assert_eq!(test_model_connection(&req).await.unwrap().reply, "OK");
        server.join().unwrap();
    }
}
