//! Probe a running duthub MCP endpoint for its exposed tool surface.
//!
//! Used by the desktop Agent host to re-validate a resumed ACP session: a
//! restored `session/load` must still expose the duthub MCP tools, otherwise
//! the Agent could keep running with a silently empty (or foreign) tool set.
//!
//! The probe performs a real MCP handshake (initialize + tools/list) over the
//! Streamable HTTP transport with the session's read-only Bearer token. It is
//! synchronous and owns a short-lived current-thread Tokio runtime, so callers
//! on plain worker threads (the ACP host) do not need an async context.

use std::time::Duration;

use rmcp::model::ClientConfig;
use rmcp::service::serve_client;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};

/// Failure of an MCP tool-surface probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// Endpoint or token is missing/malformed; no request was sent.
    Invalid(String),
    /// The whole handshake did not finish inside the caller's timeout.
    Timeout(Duration),
    /// Transport, authentication, or JSON-RPC failure.
    Failed(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Invalid(reason) => write!(formatter, "MCP 探测参数无效: {reason}"),
            ProbeError::Timeout(timeout) => {
                write!(formatter, "MCP 工具面探测超时（{}s）", timeout.as_secs())
            }
            ProbeError::Failed(reason) => write!(formatter, "MCP 工具面探测失败: {reason}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Connect to `url` with `token`, run initialize + `tools/list`, and return the
/// sorted tool names.
///
/// `timeout` bounds the entire handshake, including connection setup.
pub fn list_tool_names(
    url: &str,
    token: &str,
    timeout: Duration,
) -> Result<Vec<String>, ProbeError> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ProbeError::Invalid(format!(
            "MCP 端点必须是 http(s) URL: {url}"
        )));
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(ProbeError::Invalid("缺少 Bearer Token".into()));
    }
    if timeout.is_zero() {
        return Err(ProbeError::Invalid("超时必须大于 0".into()));
    }

    // Plain HTTP loopback does not need TLS, but installing the provider keeps
    // the transport usable for https endpoints and matches the MCP bridge.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ProbeError::Failed(format!("无法创建探测运行时: {error}")))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout)
            .build()
            .map_err(|error| ProbeError::Failed(format!("无法创建 HTTP 客户端: {error}")))?;
        let transport = StreamableHttpClientTransport::with_client(
            client,
            StreamableHttpClientTransportConfig::with_uri(url).auth_header(token.to_owned()),
        );
        let handshake = async {
            let service = serve_client(ClientConfig::default(), transport)
                .await
                .map_err(|error| ProbeError::Failed(error.to_string()))?;
            let tools = service
                .list_all_tools()
                .await
                .map_err(|error| ProbeError::Failed(error.to_string()))?;
            let mut names: Vec<String> = tools
                .into_iter()
                .map(|tool| tool.name.into_owned())
                .collect();
            names.sort();
            service.cancel().await.ok();
            Ok(names)
        };
        match tokio::time::timeout(timeout, handshake).await {
            Ok(result) => result,
            Err(_) => Err(ProbeError::Timeout(timeout)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use duthub_core::agent_access::{
        AgentAccessRuntime, AgentAccessSettings, AgentCredential, DutAction, DutGrant,
    };
    use duthub_core::audit::AuditSink;
    use duthub_core::lease::ControlLeaseManager;

    use crate::auth_http::AccessContext;
    use crate::snapshot::SnapshotState;
    use crate::transport::spawn_http_listener_loopback;

    fn access_context(token: &str) -> Arc<AccessContext> {
        let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
            enabled: true,
            agents: vec![AgentCredential {
                id: "cred-probe".into(),
                listener_id: None,
                agent_name: "probe-agent".into(),
                token: token.into(),
                enabled: true,
                grants: vec![DutGrant {
                    dut_id: "dut-probe".into(),
                    actions: vec![DutAction::Read],
                }],
            }],
            ..Default::default()
        });
        Arc::new(AccessContext::new(
            auth,
            Arc::new(ControlLeaseManager::new(None)),
            AuditSink::memory(),
        ))
    }

    #[test]
    fn lists_tools_and_rejects_bad_token_or_unreachable_endpoint() {
        let token = duthub_core::agent_access::generate_token();
        let handle = spawn_http_listener_loopback(
            Arc::new(SnapshotState::empty()),
            Some(access_context(&token)),
            None,
            None,
        )
        .expect("loopback listener");
        let url = format!("http://{}/mcp", handle.endpoint);

        let names = list_tool_names(&url, &token, Duration::from_secs(15)).unwrap();
        assert!(
            names.iter().any(|name| name == "dut_list"),
            "探测必须看到只读工具面: {names:?}"
        );

        let error = list_tool_names(&url, "dth-wrong-token", Duration::from_secs(10))
            .expect_err("错误 Bearer 必须失败");
        assert!(matches!(error, ProbeError::Failed(_)), "{error}");

        // A closed port may either be refused immediately or burn the whole
        // connect budget; both are failures from the host's point of view.
        let unreachable = list_tool_names("http://127.0.0.1:1/mcp", &token, Duration::from_secs(1))
            .expect_err("不可达端点必须失败");
        assert!(
            matches!(unreachable, ProbeError::Failed(_) | ProbeError::Timeout(_)),
            "{unreachable}"
        );

        assert!(matches!(
            list_tool_names("file:///tmp/mcp", &token, Duration::from_secs(1)),
            Err(ProbeError::Invalid(_))
        ));
        assert!(matches!(
            list_tool_names(&url, "  ", Duration::from_secs(1)),
            Err(ProbeError::Invalid(_))
        ));
    }
}
