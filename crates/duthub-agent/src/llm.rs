//! LLM provider connectivity probes (6 provider protocols).
//!
//! API keys are passed in memory only; callers persist secrets in the OS keyring
//! (`duthub_core::keys`) and never serialize them to `settings.json`.

use std::time::{Duration, Instant};

use duthub_core::Provider;
use thiserror::Error;

/// Typed errors for LLM endpoint validation, model listing, and connectivity probes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LlmError {
    #[error("Invalid Base URL")]
    InvalidBaseUrl,
    #[error(
        "Base URL must be an HTTP/HTTPS URL without credentials, query parameters, or fragments"
    )]
    DisallowedBaseUrl,
    #[error("Invalid model list endpoint URL")]
    InvalidModelListUrl,
    #[error("API Key is empty")]
    EmptyApiKey,
    #[error("Failed to build HTTP client: {0}")]
    ClientBuild(String),
    #[error("Request failed: {0}")]
    Request(String),
    #[error("{0}")]
    HttpStatus(String),
    #[error("Invalid model list response")]
    InvalidModelListResponse,
    #[error("Response did not include a model list")]
    MissingModelList,
    #[error("Failed to parse response JSON: {0}")]
    InvalidJson(String),
    #[error("Model returned empty reply")]
    EmptyReply,
}

#[derive(Clone)]
pub struct TestRequest {
    pub provider: Provider,
    /// Base URL (including `/v1`, etc.); empty falls back to the provider default.
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// Probe prompt (defaults to "Reply with OK.").
    pub prompt: String,
}

impl std::fmt::Debug for TestRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestRequest")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"[REDACTED]")
            .field("prompt", &self.prompt)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct TestResponse {
    pub latency_ms: u64,
    pub reply: String,
}

const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REPLY_CHARS: usize = 400;

/// Validate and normalize an HTTP/HTTPS model base URL (rejects credentials,
/// query strings, and fragments).
pub fn validate_model_base(value: &str) -> Result<String, LlmError> {
    let input = value.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(input).map_err(|_| LlmError::InvalidBaseUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(LlmError::DisallowedBaseUrl);
    }
    Ok(input.to_owned())
}

fn endpoint(req: &TestRequest, suffix: &str) -> Result<String, LlmError> {
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

fn model_list_endpoint(req: &TestRequest) -> Result<(reqwest::Url, Provider), LlmError> {
    let mut url = reqwest::Url::parse(&endpoint(req, "models")?)
        .map_err(|_| LlmError::InvalidModelListUrl)?;
    let mut provider = req.provider;
    if provider == Provider::Anthropic
        && url.scheme() == "https"
        && url.host_str() == Some("api.deepseek.com")
        && url.port().is_none()
        && url.path() == "/anthropic/v1/models"
    {
        url.set_path("/models");
        provider = Provider::Deepseek;
    }
    Ok((url, provider))
}

fn probe_client() -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| LlmError::ClientBuild(error.to_string()))
}

async fn send_json_probe(
    request: reqwest::RequestBuilder,
    api_key: &str,
) -> Result<serde_json::Value, LlmError> {
    let resp = request
        .send()
        .await
        .map_err(|e| LlmError::Request(e.without_url().to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
        return Err(LlmError::HttpStatus(probe_http_error(
            status, &text, api_key,
        )));
    }
    serde_json::from_str(&text).map_err(|e| LlmError::InvalidJson(e.to_string()))
}

fn probe_http_error(status: u16, body: &str, api_key: &str) -> String {
    let json = serde_json::from_str::<serde_json::Value>(body).ok();
    let reason = json
        .as_ref()
        .and_then(|value| {
            value["error"]["message"]
                .as_str()
                .or(value["message"].as_str())
        })
        .unwrap_or(body)
        .trim();
    if reason.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {}", truncate_redacted(reason, api_key, 200))
    }
}

/// Fetch available model IDs from the provider's authenticated model-list endpoint.
pub async fn fetch_available_models(req: &TestRequest) -> Result<Vec<String>, LlmError> {
    if req.api_key.trim().is_empty() {
        return Err(LlmError::EmptyApiKey);
    }
    let (url, provider) = model_list_endpoint(req)?;
    let request = probe_client()?.get(url);
    let request = match provider {
        Provider::Anthropic => request
            .header("x-api-key", req.api_key.trim())
            .header("anthropic-version", "2023-06-01"),
        Provider::Gemini => request.header("x-goog-api-key", req.api_key.trim()),
        _ => request.bearer_auth(req.api_key.trim()),
    };
    let response = request
        .send()
        .await
        .map_err(|error| LlmError::Request(error.without_url().to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(LlmError::HttpStatus(probe_http_error(
            status.as_u16(),
            &body,
            &req.api_key,
        )));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| LlmError::InvalidModelListResponse)?;
    let mut models = parse_model_ids(&body);
    if models.is_empty() && !body["data"].is_array() && !body["models"].is_array() {
        return Err(LlmError::MissingModelList);
    }
    models.sort();
    models.dedup();
    Ok(models)
}

fn parse_model_ids(body: &serde_json::Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(items) = body["data"].as_array() {
        for item in items {
            if let Some(id) = item["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                ids.push(id.to_owned());
            }
        }
    }
    if let Some(items) = body["models"].as_array() {
        for item in items {
            let raw = item["id"].as_str().or_else(|| item["name"].as_str());
            if let Some(id) = raw
                .map(str::trim)
                .map(|s| s.strip_prefix("models/").unwrap_or(s))
                .filter(|s| !s.is_empty())
            {
                ids.push(id.to_owned());
            }
        }
    }
    ids
}

/// Test the provider's authenticated model-list endpoint without generating a reply.
pub async fn test_model_api(req: &TestRequest) -> Result<TestResponse, LlmError> {
    let started = Instant::now();
    let _ = fetch_available_models(req).await?;
    Ok(TestResponse {
        latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        reply: String::new(),
    })
}

/// Send a minimal 1-turn connectivity probe and return latency + first reply.
pub async fn test_model_connection(req: &TestRequest) -> Result<TestResponse, LlmError> {
    if req.api_key.trim().is_empty() {
        return Err(LlmError::EmptyApiKey);
    }
    let prompt = if req.prompt.trim().is_empty() {
        "Reply with OK.".to_string()
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
                "messages": [{ "role": "user", "content": prompt }],
                "max_tokens": 16,
            });
            let v = send_json_probe(
                client
                    .post(&url)
                    .bearer_auth(req.api_key.trim())
                    .json(&body),
                &req.api_key,
            )
            .await?;
            probe_reply(v["choices"][0]["message"]["content"].as_str())?
        }
        Provider::OpenaiResponses => {
            let url = endpoint(req, "responses")?;
            let body = serde_json::json!({
                "model": req.model,
                "input": prompt,
                "max_output_tokens": 16,
            });
            let v = send_json_probe(
                client
                    .post(&url)
                    .bearer_auth(req.api_key.trim())
                    .json(&body),
                &req.api_key,
            )
            .await?;
            let text = v["output_text"].as_str().or_else(|| {
                v["output"].as_array().and_then(|items| {
                    items.iter().find_map(|item| {
                        item["content"].as_array().and_then(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|block| block["text"].as_str())
                                .find(|text| !text.trim().is_empty())
                        })
                    })
                })
            });
            probe_reply(text)?
        }
        Provider::Anthropic => {
            let url = endpoint(req, "messages")?;
            let body = serde_json::json!({
                "model": req.model,
                "max_tokens": 16,
                "messages": [{ "role": "user", "content": prompt }],
            });
            let v = send_json_probe(
                client
                    .post(&url)
                    .header("x-api-key", req.api_key.trim())
                    .header("anthropic-version", "2023-06-01")
                    .json(&body),
                &req.api_key,
            )
            .await?;
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
                "contents": [{ "parts": [{ "text": prompt }] }],
                "generationConfig": { "maxOutputTokens": 16 },
            });
            let v = send_json_probe(
                client
                    .post(&url)
                    .header("x-goog-api-key", req.api_key.trim())
                    .json(&body),
                &req.api_key,
            )
            .await?;
            probe_reply(v["candidates"][0]["content"]["parts"][0]["text"].as_str())?
        }
    };

    Ok(TestResponse {
        latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        reply: truncate_redacted(&reply, &req.api_key, MAX_REPLY_CHARS),
    })
}

fn probe_reply(text: Option<&str>) -> Result<String, LlmError> {
    text.map(str::trim)
        .filter(|reply| !reply.is_empty())
        .map(str::to_owned)
        .ok_or(LlmError::EmptyReply)
}

fn truncate_redacted(text: &str, key: &str, limit: usize) -> String {
    text.replace(key.trim(), "[REDACTED]")
        .chars()
        .take(limit)
        .collect()
}

#[cfg(test)]
mod fetch_tests {
    use super::*;

    #[test]
    fn parse_model_ids_handles_openai_anthropic_and_gemini_shapes() {
        let openai = serde_json::json!({
            "data": [
                {"id": "deepseek-flash"},
                {"id": "deepseek-v4-pro"}
            ]
        });
        assert_eq!(
            parse_model_ids(&openai),
            vec!["deepseek-flash".to_owned(), "deepseek-v4-pro".to_owned()]
        );
        let gemini = serde_json::json!({
            "models": [
                {"name": "models/gemini-2.5-pro"},
                {"name": "models/gemini-2.5-flash"}
            ]
        });
        assert_eq!(
            parse_model_ids(&gemini),
            vec!["gemini-2.5-pro".to_owned(), "gemini-2.5-flash".to_owned()]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_failure_reports_the_provider_message_without_the_credential() {
        let error = probe_http_error(
            402,
            r#"{"type":"error","error":{"type":"billing_error","message":"Insufficient Balance: test-secret"}}"#,
            "test-secret",
        );
        assert_eq!(error, "HTTP 402: Insufficient Balance: [REDACTED]");
        assert_eq!(probe_http_error(503, "", "test-secret"), "HTTP 503");
    }

    #[test]
    fn deepseek_anthropic_uses_the_official_model_list() {
        for base_url in [
            "https://api.deepseek.com/anthropic",
            "https://api.deepseek.com/anthropic/v1/",
            "https://api.deepseek.com:443/anthropic",
        ] {
            let req = TestRequest {
                provider: Provider::Anthropic,
                base_url: base_url.into(),
                model: "deepseek-flash".into(),
                api_key: String::new(),
                prompt: String::new(),
            };
            let (url, auth) = model_list_endpoint(&req).unwrap();
            assert_eq!(url.as_str(), "https://api.deepseek.com/models");
            assert_eq!(auth, Provider::Deepseek);
            assert!(
                endpoint(&req, "messages")
                    .unwrap()
                    .ends_with("/anthropic/v1/messages")
            );
        }

        for base_url in [
            "https://api.example.com/anthropic",
            "http://api.deepseek.com/anthropic",
            "https://api.deepseek.com:8443/anthropic",
        ] {
            let req = TestRequest {
                provider: Provider::Anthropic,
                base_url: base_url.into(),
                model: String::new(),
                api_key: String::new(),
                prompt: String::new(),
            };
            let (url, auth) = model_list_endpoint(&req).unwrap();
            assert_eq!(url.as_str(), format!("{base_url}/v1/models"));
            assert_eq!(auth, Provider::Anthropic);
        }
    }

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
        assert_eq!(err, LlmError::EmptyApiKey);
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
        assert_eq!(test_model_api(&req).await.unwrap().reply, "");
        assert_eq!(test_model_connection(&req).await.unwrap().reply, "OK");
        server.join().unwrap();
    }
}
