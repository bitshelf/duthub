//! 传输：stdio（`rmcp::transport::io::stdio`）与 Streamable HTTP。
//!
//! HTTP 默认只绑 loopback；宿主显式开启局域网（`agentAccess.lanAccess`）时绑定
//! `0.0.0.0`，同一端口同时接受回环与局域网地址。非回环绑定必须开启 Bearer 认证。
//! 认证关闭时 HTTP 统一拒绝。

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::IntoResponse;

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
use crate::notify::UserNotifier;
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
        | ClientRequest::ListToolsRequest(_)
        | ClientRequest::ListResourcesRequest(_)
        | ClientRequest::ListResourceTemplatesRequest(_)
        | ClientRequest::ReadResourceRequest(_)
        | ClientRequest::ListPromptsRequest(_)
        | ClientRequest::GetPromptRequest(_) => true,
        ClientRequest::CallToolRequest(call) => plan_tool_allowed(&call.params.name),
        _ => false,
    }
}

/// Maximum accepted `/mcp` request-body size (1 MiB).
pub const MAX_MCP_REQUEST_BYTES: usize = 1024 * 1024;

/// Reject oversized `/mcp` bodies before authentication or tool dispatch.
async fn limit_mcp_request_body(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if request.method() != axum::http::Method::POST {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    match axum::body::to_bytes(body, MAX_MCP_REQUEST_BYTES).await {
        Ok(bytes) => {
            parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
            if let Ok(length) = axum::http::HeaderValue::from_str(&bytes.len().to_string()) {
                parts
                    .headers
                    .insert(axum::http::header::CONTENT_LENGTH, length);
            }
            next.run(axum::http::Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Err(_) => (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            [("content-type", "application/json")],
            "{\"error\":\"payload_too_large\"}",
        )
            .into_response(),
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
    http_router_for_listener(state, access, listener_id, None, None, None)
}

/// DUT 专用监听器的 Router（端到端测试/嵌入宿主用）。不接通知器：
/// `notify_user` 只属于用户配置的监听器。
pub fn http_router_on_dut(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    listener_dut: Option<Arc<str>>,
) -> axum::Router {
    http_router_on_dut_with_notifier(state, access, listener_id, listener_dut, None)
}

/// DUT 专用监听器的 Router，可附加宿主的用户通知器。
///
/// `notifier = Some(..)` 时该端点暴露 14 个工具（13 个只读 Plan + `notify_user`）；
/// `None` 时维持 13 个，`notify_user` 既不出现在 `tools/list` 也不可调用。
pub fn http_router_on_dut_with_notifier(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    listener_dut: Option<Arc<str>>,
    notifier: Option<Arc<dyn UserNotifier>>,
) -> axum::Router {
    http_router_for_listener(state, access, listener_id, None, listener_dut, notifier)
}

fn http_router_for_listener(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    endpoint: Option<SocketAddr>,
    listener_dut: Option<Arc<str>>,
    notifier: Option<Arc<dyn UserNotifier>>,
) -> axum::Router {
    let core = McpServerCore::new(state, access.clone())
        .with_listener_endpoint(endpoint)
        .with_listener_dut(listener_dut.as_deref())
        .with_user_notifier(notifier);
    let service = StreamableHttpService::new(
        move || Ok(core.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .enforce_origin_validation()
            .with_json_response(true),
    );
    let router = match access {
        Some(ctx) => axum::Router::new().route_service(
            "/mcp",
            crate::auth_http::AuthSvc::new((*ctx).clone(), service).on_listener(listener_id),
        ),
        None => axum::Router::new().route(
            "/mcp",
            axum::routing::any(|| async { crate::auth_http::unauthorized_response() }),
        ),
    };
    router.layer(axum::middleware::from_fn(limit_mcp_request_body))
}

/// 已启动 HTTP 监听器的生命周期句柄：会话结束 / 关闭时先 `shutdown` 再 `join`，
/// 释放端口与线程（T07）。
pub struct ListenerHandle {
    /// 实际对客户端展示/可连接的地址（通配绑定替换为探测到的局域网 IPv4）。
    pub endpoint: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ListenerHandle {
    /// 发送 shutdown 信号并等待服务线程退出。
    /// 返回 `false` = 在 `timeout` 内 join 未完成（信号已发出，端口随后释放）。
    pub fn shutdown_and_join_with_timeout(mut self, timeout: Duration) -> bool {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        let Some(join) = self.join.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !join.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        join.join().is_ok()
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        // 未显式关闭就丢弃句柄：至少发出 shutdown 信号，避免线程无限期占用端口。
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
    }
}

/// 在已绑定（且校验过绑定策略）的 TCP listener 上启动 HTTP 服务线程。
///
/// `notifier = Some(..)` 时端点额外暴露 `notify_user`（仅供用户配置的监听器）；
/// 桌面发起的 Agent 会话 listener 一律传 `None`。
pub fn spawn_http_listener_on(
    listener: std::net::TcpListener,
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    listener_dut: Option<Arc<str>>,
    notifier: Option<Arc<dyn UserNotifier>>,
) -> Result<ListenerHandle, String> {
    let addr = listener
        .local_addr()
        .map_err(|e| format!("取监听地址失败: {e}"))?;
    let auth_enabled = access
        .as_ref()
        .is_some_and(|ctx| ctx.auth.settings().enabled);
    assert_bind_allowed(&addr, auth_enabled)?;
    let advertised = advertised_endpoint(addr);
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener 设非阻塞失败: {e}"))?;
    let rt = build_http_runtime()?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let join = std::thread::Builder::new()
        .name(format!("duthub-mcp-http-{}", addr.port()))
        .spawn(move || {
            let result = rt.block_on(serve_listener(
                listener,
                state,
                access,
                listener_id,
                listener_dut,
                notifier,
                advertised,
                async move {
                    let _ = shutdown_rx.await;
                },
            ));
            if let Err(error) = result {
                eprintln!("[mcp] HTTP listener stopped: {error}");
            }
        })
        .map_err(|e| format!("MCP HTTP 线程启动失败: {e}"))?;
    Ok(ListenerHandle {
        endpoint: advertised,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    })
}

/// 绑定 `127.0.0.1:0`（临时端口）并启动 HTTP 服务线程（每会话 / 测试用）。
///
/// 桌面发起的 Agent 会话走这条路径，因此永远不接通知器：`notify_user`
/// 对它们不可见也不可调用。
pub fn spawn_http_listener_loopback(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    listener_dut: Option<Arc<str>>,
) -> Result<ListenerHandle, String> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|e| format!("绑定回环端口失败: {e}"))?;
    spawn_http_listener_on(listener, state, access, listener_id, listener_dut, None)
}

fn build_http_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("MCP runtime 启动失败: {e}"))
}

/// 单一服务实现：阻塞版与带句柄版共用（保证路由/认证/日志形状一致）。
#[expect(
    clippy::too_many_arguments,
    reason = "one wiring call: server identity, transport options, endpoint and shutdown signal"
)]
async fn serve_listener(
    listener: std::net::TcpListener,
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    listener_dut: Option<Arc<str>>,
    notifier: Option<Arc<dyn UserNotifier>>,
    advertised: SocketAddr,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let router = http_router_for_listener(
        state,
        access,
        listener_id,
        Some(advertised),
        listener_dut,
        notifier,
    );
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| format!("listener 迁移失败: {e}"))?;
    eprintln!(
        "[mcp] HTTP serving on {}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(|e| format!("MCP HTTP 服务错误: {e}"))
}

/// 绑定策略：回环始终允许；非回环（含 `0.0.0.0` 通配）必须开启 Bearer 认证。
/// 局域网模式由宿主显式绑定 `0.0.0.0`，认证关闭时这里 fail closed。
pub fn assert_bind_allowed(addr: &SocketAddr, auth_enabled: bool) -> Result<(), String> {
    if addr.ip().is_loopback() || auth_enabled {
        return Ok(());
    }
    Err(format!(
        "MCP HTTP 监听 {addr} 需要开启 Agent 接入认证（Bearer）；\
         未认证的端点只允许绑定 loopback"
    ))
}

/// 对外展示地址：`0.0.0.0` 通配绑定替换为探测到的本机局域网 IPv4
/// （探测失败回退 127.0.0.1）；具体地址原样返回。
pub fn advertised_endpoint(addr: SocketAddr) -> SocketAddr {
    if !addr.ip().is_unspecified() {
        return addr;
    }
    SocketAddr::new(
        IpAddr::V4(detect_lan_ipv4().unwrap_or(Ipv4Addr::LOCALHOST)),
        addr.port(),
    )
}

/// Reject RFC 2544 benchmarking addresses (`198.18.0.0/15`) commonly used by
/// local TUN proxies (e.g. Clash/Mihomo), which are unreachable from LAN peers.
fn is_virtual_tun_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 198 && (b & 0xfe) == 18
}

fn is_usable_lan_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() && !is_virtual_tun_ipv4(ip)
}

fn probe_route_ipv4(target: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((target, 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if is_usable_lan_ipv4(ip) => Some(ip),
        _ => None,
    }
}

/// Detect the host's reachable LAN IPv4 address via routing-table lookup
/// (UDP connect sends no packets; result is cached per process). Falls back to
/// RFC 1918 gateway probes when the default internet route enters a virtual TUN.
fn detect_lan_ipv4() -> Option<Ipv4Addr> {
    static CACHE: OnceLock<Option<Ipv4Addr>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        const PROBES: [Ipv4Addr; 5] = [
            Ipv4Addr::new(8, 8, 8, 8),
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(192, 168, 0, 1),
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(172, 16, 0, 1),
        ];
        PROBES.into_iter().find_map(probe_route_ipv4)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_binding_requires_auth() {
        let loopback: SocketAddr = "127.0.0.1:8797".parse().unwrap();
        let lan: SocketAddr = "192.168.1.175:8797".parse().unwrap();
        let wildcard: SocketAddr = "0.0.0.0:8797".parse().unwrap();

        assert!(assert_bind_allowed(&loopback, false).is_ok());
        assert!(
            assert_bind_allowed(&lan, false).is_err(),
            "局域网绑定必须要求认证"
        );
        assert!(
            assert_bind_allowed(&wildcard, false).is_err(),
            "通配绑定必须要求认证"
        );
        assert!(assert_bind_allowed(&lan, true).is_ok());
        assert!(assert_bind_allowed(&wildcard, true).is_ok());
    }

    #[tokio::test]
    async fn mcp_rejects_request_bodies_over_one_mib() {
        use tower_service::Service as _;

        let mut service = http_router(Arc::new(crate::snapshot::SnapshotState::empty()), None)
            .into_service::<Body>();
        let oversized = Body::from(vec![b'x'; MAX_MCP_REQUEST_BYTES + 1]);
        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri("/mcp")
            .body(oversized)
            .unwrap();
        let response = service.call(request).await.unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "oversized /mcp bodies must be rejected before authentication"
        );

        let mut service = http_router(Arc::new(crate::snapshot::SnapshotState::empty()), None)
            .into_service::<Body>();
        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri("/mcp")
            .body(Body::from("{}"))
            .unwrap();
        let response = service.call(request).await.unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::UNAUTHORIZED,
            "small bodies must still reach the route"
        );
    }

    #[test]
    fn advertised_endpoint_replaces_wildcard_bind() {
        let wildcard: SocketAddr = "0.0.0.0:18797".parse().unwrap();
        let advertised = advertised_endpoint(wildcard);
        assert_eq!(advertised.port(), 18797);
        assert!(!advertised.ip().is_unspecified(), "对外地址不能是 0.0.0.0");

        let loopback: SocketAddr = "127.0.0.1:18797".parse().unwrap();
        assert_eq!(advertised_endpoint(loopback), loopback);
        assert!(is_virtual_tun_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(!is_usable_lan_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(is_usable_lan_ipv4(Ipv4Addr::new(192, 168, 1, 175)));
    }
}
