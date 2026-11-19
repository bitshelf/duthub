//! 传输：stdio（`rmcp::transport::io::stdio`）与 Streamable HTTP（强制 loopback）。
//!
//! HTTP 路径要求有效的 Bearer 凭据，认证关闭时统一拒绝。

use std::sync::Arc;

use rmcp::model::{
    ClientJsonRpcMessage, ClientNotification, ClientRequest, ServerJsonRpcMessage, ServerResult,
};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::transport::{Transport, async_rw::AsyncRwTransport};
use rmcp::{serve_server, transport::io::stdio};

use crate::auth_http::AccessContext;
use crate::server::{McpServerCore, plan_mode_error, plan_tool_allowed};
use crate::state::McpState;

/// stdio 传输：阻塞直到客户端断开。由独立进程（bin duthub-mcp）调用。
/// stdio 视为本地管理员（偏差 5）：McpServerCore 不接 AccessContext。
pub async fn serve_stdio(
    state: Arc<dyn McpState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = McpServerCore::new(state, None);
    let service = serve_server(server, stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Forward read-only Duthub tools through the authenticated HTTP transport.
pub async fn bridge_stdio(endpoint: &str, token: String) -> Result<(), &'static str> {
    let uri: http::Uri = endpoint.parse().map_err(|_| "Invalid MCP endpoint")?;
    let authority = uri
        .authority()
        .filter(|authority| !authority.as_str().contains('@'))
        .ok_or("Invalid MCP endpoint")?;
    let host = authority.host();
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    if uri.scheme_str() != Some("https") && !(uri.scheme_str() == Some("http") && loopback) {
        return Err("MCP bridge requires HTTPS or a loopback HTTP endpoint");
    }
    if uri.query().is_some() || token.is_empty() || token.contains(char::is_whitespace) {
        return Err("Use a separate bearer token and an MCP endpoint without a query");
    }
    let config = StreamableHttpClientTransportConfig::with_uri(endpoint).auth_header(token);
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| "Could not initialize the MCP HTTP client")?;
    let mut upstream = StreamableHttpClientTransport::with_client(client, config);
    let (read, write) = rmcp::transport::io::stdio_pipes();
    let mut local = AsyncRwTransport::new_server(read, write);
    loop {
        tokio::select! {
            request = local.receive() => match request {
                Some(request) => {
                    if let ClientJsonRpcMessage::Notification(message) = &request
                        && !matches!(message.notification, ClientNotification::InitializedNotification(_) | ClientNotification::CancelledNotification(_))
                    {
                        continue;
                    }
                    if let ClientJsonRpcMessage::Request(message) = &request
                        && !bridge_request_allowed(&message.request)
                    {
                        local.send(ServerJsonRpcMessage::error(plan_mode_error(), Some(message.id.clone())))
                            .await.map_err(|_| "MCP stdio output failed")?;
                        continue;
                    }
                    upstream.send(request).await.map_err(|_| "MCP upstream rejected the request")?;
                }
                None => return upstream.close().await.map_err(|_| "MCP upstream could not close"),
            },
            response = upstream.receive() => match response {
                Some(mut response) => {
                    if let ServerJsonRpcMessage::Request(message) = &response {
                        upstream.send(ClientJsonRpcMessage::error(plan_mode_error(), Some(message.id.clone())))
                            .await.map_err(|_| "MCP upstream rejected the response")?;
                        continue;
                    }
                    if let ServerJsonRpcMessage::Response(message) = &mut response
                        && let ServerResult::ListToolsResult(tools) = &mut message.result
                    {
                        tools.tools.retain(|tool| plan_tool_allowed(&tool.name));
                    }
                    local.send(response).await.map_err(|_| "MCP stdio output failed")?;
                }
                None => return Err("MCP upstream closed"),
            },
        }
    }
}

fn bridge_request_allowed(request: &ClientRequest) -> bool {
    match request {
        ClientRequest::InitializeRequest(_)
        | ClientRequest::PingRequest(_)
        | ClientRequest::DiscoverRequest(_)
        | ClientRequest::ListToolsRequest(_) => true,
        ClientRequest::CallToolRequest(call) => plan_tool_allowed(&call.params.name),
        _ => false,
    }
}

/// Streamable HTTP 的 axum Router（route /mcp）。宿主自行 bind + serve。
/// 在没有认证上下文时拒绝 HTTP 请求。
pub fn http_router(state: Arc<dyn McpState>, access: Option<Arc<AccessContext>>) -> axum::Router {
    http_router_on(state, access, None)
}

pub fn http_router_on(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
) -> axum::Router {
    http_router_for_listener(state, access, listener_id, None)
}

fn http_router_for_listener(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    port: Option<u16>,
) -> axum::Router {
    let core = McpServerCore::new(state, access.clone()).with_listener_port(port);
    let service = StreamableHttpService::new(
        move || Ok(core.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .enforce_origin_validation()
            .with_json_response(true),
    );
    match access {
        Some(ctx) => axum::Router::new().route_service(
            "/mcp",
            crate::auth_http::AuthSvc::new((*ctx).clone(), service).on_listener(listener_id),
        ),
        None => axum::Router::new().route(
            "/mcp",
            axum::routing::any(|| async { crate::auth_http::unauthorized_response() }),
        ),
    }
}

/// Streamable HTTP：阻塞循环（desktop 后台线程调用）。强制 loopback 由调用方 bind 保证，
/// 但仍断言 listener 地址（防误绑非回环）。
pub fn serve_http_blocking(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener: std::net::TcpListener,
) -> Result<(), String> {
    serve_http_listener_blocking(state, access, listener, None)
}

pub fn serve_http_listener_blocking(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener: std::net::TcpListener,
    listener_id: Option<Arc<str>>,
) -> Result<(), String> {
    let addr = listener
        .local_addr()
        .map_err(|e| format!("取监听地址失败: {e}"))?;
    assert_loopback(&addr)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("MCP runtime 启动失败: {e}"))?;
    // tokio::net::TcpListener::from_std 要求 socket 已是非阻塞模式，
    // 否则首次 accept 会阻塞整个 runtime 线程（症状：HTTP 请求全部无响应）。
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener 设非阻塞失败: {e}"))?;
    rt.block_on(async move {
        let router = http_router_for_listener(state, access, listener_id, Some(addr.port()));
        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|e| format!("listener 迁移失败: {e}"))?;
        eprintln!(
            "[mcp] HTTP serving on {}",
            listener
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_default()
        );
        axum::serve(listener, router)
            .await
            .map_err(|e| format!("MCP HTTP 服务错误: {e}"))
    })
}

/// loopback 强制检查：拒绝非 127.0.0.1 / ::1 绑定地址
pub fn assert_loopback(addr: &std::net::SocketAddr) -> Result<(), String> {
    let ip = addr.ip();
    if !ip.is_loopback() {
        return Err(format!(
            "MCP HTTP 端点只允许绑定 loopback（当前 {ip}）；远程部署请经认证网关"
        ));
    }
    Ok(())
}
