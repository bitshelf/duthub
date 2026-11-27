//! MCP transports for stdio and Streamable HTTP.
//!
//! HTTP binds to loopback by default.
//! Explicit LAN mode binds all IPv4 interfaces and requires Bearer authentication.
//! HTTP requests are denied when authentication is disabled.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
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

/// Serve stdio until the client disconnects.
/// The local stdio subprocess uses the administrator context.
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

/// Construct the Streamable HTTP router for the host's listening socket.
/// Requests are rejected when no authentication context is present.
pub fn http_router(state: Arc<dyn McpState>, access: Option<Arc<AccessContext>>) -> axum::Router {
    http_router_on(state, access, None)
}

pub fn http_router_on(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
) -> axum::Router {
    http_router_for_listener(state, access, listener_id, None, None, None, None)
}

/// Create a DUT-scoped router without the user notifier.
/// User-configured listeners opt in to notifications separately.
pub fn http_router_on_dut(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    listener_dut: Option<Arc<str>>,
) -> axum::Router {
    http_router_on_dut_with_notifier(state, access, listener_id, listener_dut, None)
}

/// Create a DUT-scoped router with an optional user notifier.
///
/// With a notifier, the listener adds the notification tool to the 13 Plan tools.
/// Without one, the notification tool is neither advertised nor callable.
pub fn http_router_on_dut_with_notifier(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    listener_dut: Option<Arc<str>>,
    notifier: Option<Arc<dyn UserNotifier>>,
) -> axum::Router {
    http_router_for_listener(
        state,
        access,
        listener_id,
        None,
        None,
        listener_dut,
        notifier,
    )
}

fn http_router_for_listener(
    state: Arc<dyn McpState>,
    access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>,
    bind_addr: Option<SocketAddr>,
    endpoint: Option<SocketAddr>,
    listener_dut: Option<Arc<str>>,
    notifier: Option<Arc<dyn UserNotifier>>,
) -> axum::Router {
    let configured_hosts = access
        .as_ref()
        .map_or_default(|ctx| ctx.auth.settings().allowed_hosts);
    let allowed_hosts = allowed_hosts_for_listener(bind_addr, endpoint, &configured_hosts);
    let core = McpServerCore::new(state.clone(), access.clone())
        .with_listener_endpoint(endpoint)
        .with_listener_dut_arc(listener_dut.clone())
        .with_user_notifier(notifier);
    let service = StreamableHttpService::new(
        move || Ok(core.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(allowed_hosts.clone())
            .enforce_origin_validation()
            .with_json_response(true),
    );
    let mcp_router = match access.clone() {
        Some(ctx) => axum::Router::new().route_service(
            "/mcp",
            crate::auth_http::AuthSvc::new((*ctx).clone(), service)
                .on_listener(listener_id.clone()),
        ),
        None => axum::Router::new().route(
            "/mcp",
            axum::routing::any(|| async { crate::auth_http::unauthorized_response() }),
        ),
    }
    .layer(axum::middleware::from_fn(limit_mcp_request_body));
    let transfer_state = crate::http_transfer::HttpTransferState::new(
        state,
        access,
        listener_id,
        listener_dut,
        allowed_hosts,
    );
    crate::http_transfer::mount_transfer_routes(mcp_router, transfer_state)
}

/// Own a live HTTP listener.
/// Signal shutdown before joining the thread to release the port.
pub struct ListenerHandle {
    /// Address shown to clients. Wildcard binds use the detected LAN address.
    pub endpoint: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for ListenerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenerHandle")
            .field("endpoint", &self.endpoint)
            .field("active", &self.shutdown.is_some())
            .finish_non_exhaustive()
    }
}

impl ListenerHandle {
    /// Signal shutdown and wait for the service thread to exit.
    /// False means the deadline elapsed after signaling the thread.
    #[must_use]
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
        // Signal shutdown even when the owner drops the handle without joining.
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
    }
}

/// Start an HTTP service on a bound socket whose bind policy has been checked.
///
/// User-configured listeners may expose the user notification tool.
/// Desktop-initiated Agent sessions omit the notifier.
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

/// Bind an ephemeral loopback port and start an HTTP service.
///
/// Desktop-initiated Agent sessions use this path without a user notifier.
/// Their clients cannot list or call the user notification tool.
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

/// Share routing, authentication, and logging between the blocking and managed server entry points.
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
    let bind_addr = listener.local_addr().ok();
    let router = http_router_for_listener(
        state,
        access,
        listener_id,
        bind_addr,
        Some(advertised),
        listener_dut,
        notifier,
    );
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| format!("listener 迁移失败: {e}"))?;
    eprintln!(
        "[mcp] HTTP serving on {} (advertised {advertised})",
        listener.local_addr().map_or_default(|a| a.to_string())
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(|e| format!("MCP HTTP 服务错误: {e}"))
}

/// Allow loopback binds and require Bearer authentication for other addresses.
/// A wildcard bind requires the host to explicitly enable LAN access.
pub fn assert_bind_allowed(addr: &SocketAddr, auth_enabled: bool) -> Result<(), String> {
    if addr.ip().is_loopback() || auth_enabled {
        return Ok(());
    }
    Err(format!(
        "MCP HTTP 监听 {addr} 需要开启 Agent 接入认证（Bearer）；\
         未认证的端点只允许绑定 loopback"
    ))
}

/// Replace an IPv4 wildcard with the detected LAN address for client display.
/// Fall back to loopback when detection fails. Preserve concrete addresses.
#[must_use]
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

fn is_usable_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_usable_lan_ipv4(v4),
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_unicast_link_local()
                && !v6.is_multicast()
        }
    }
}

fn probe_route_ipv4(target: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((target, 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if is_usable_lan_ipv4(ip) => Some(ip),
        _ => None,
    }
}

const LAN_PROBE_TARGETS: [Ipv4Addr; 6] = [
    Ipv4Addr::new(8, 8, 8, 8),
    Ipv4Addr::new(192, 168, 1, 1),
    Ipv4Addr::new(192, 168, 0, 1),
    Ipv4Addr::new(10, 0, 0, 1),
    Ipv4Addr::new(172, 16, 0, 1),
    Ipv4Addr::new(100, 64, 0, 1),
];

/// Detect the host's primary reachable LAN IPv4 address via routing-table lookup
/// (UDP connect sends no packets; result is cached per process). Falls back to
/// RFC 1918 / CGNAT gateway probes when the default internet route enters a virtual TUN.
fn detect_lan_ipv4() -> Option<Ipv4Addr> {
    static CACHE: OnceLock<Option<Ipv4Addr>> = OnceLock::new();
    *CACHE.get_or_init(|| LAN_PROBE_TARGETS.into_iter().find_map(probe_route_ipv4))
}

/// Sanitize a hostname or `host:port` entry for the `rmcp` Host header allowlist.
fn sanitize_allowed_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.chars().any(|ch| {
            ch.is_control() || ch.is_whitespace() || matches!(ch, '/' | '?' | '#' | '@' | '*')
        })
    {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

fn push_unique_host(hosts: &mut Vec<String>, candidate: &str) {
    let Some(clean) = sanitize_allowed_host(candidate) else {
        return;
    };
    if !hosts.iter().any(|existing| existing == &clean) {
        hosts.push(clean);
    }
}

fn local_hostname() -> Option<String> {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty()
                && trimmed
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
            {
                return Some(trimmed.to_ascii_lowercase());
            }
        }
    }
    None
}

/// Collect all usable local LAN IP addresses reachable on this host without unsafe code.
fn collect_local_lan_ips(hostname: Option<&str>) -> Vec<IpAddr> {
    let mut ips = Vec::new();
    for target in LAN_PROBE_TARGETS {
        if let Some(ip) = probe_route_ipv4(target) {
            let addr = IpAddr::V4(ip);
            if !ips.contains(&addr) {
                ips.push(addr);
            }
        }
    }
    if let Some(name) = hostname
        && let Ok(resolved) = (name, 0u16).to_socket_addrs()
    {
        for socket_addr in resolved {
            let ip = socket_addr.ip();
            if is_usable_lan_ip(ip) && !ips.contains(&ip) {
                ips.push(ip);
            }
        }
    }
    ips
}

/// Build the `Host` header allowlist for an MCP Streamable HTTP listener.
///
/// Loopback listeners keep strict loopback defaults (`localhost`, `127.0.0.1`, `::1`).
/// Non-loopback and wildcard (`0.0.0.0` / `[::]`) listeners additionally allow the bound
/// address, advertised LAN address, detected local interface IPs, local hostname, and any
/// operator-configured `agentAccess.allowedHosts` entries. The returned list is always
/// non-empty so `rmcp`'s DNS-rebinding Host validation remains active.
#[must_use]
pub fn allowed_hosts_for_listener(
    bind_addr: Option<SocketAddr>,
    advertised: Option<SocketAddr>,
    configured: &[String],
) -> Vec<String> {
    let mut hosts = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Some(bind) = bind_addr
        && !bind.ip().is_loopback()
    {
        if !bind.ip().is_unspecified() {
            push_unique_host(&mut hosts, &bind.ip().to_string());
        }
        if let Some(adv) = advertised
            && !adv.ip().is_unspecified()
        {
            push_unique_host(&mut hosts, &adv.ip().to_string());
        }
        if bind.ip().is_unspecified() {
            let hostname = local_hostname();
            for ip in collect_local_lan_ips(hostname.as_deref()) {
                push_unique_host(&mut hosts, &ip.to_string());
            }
            if let Some(name) = hostname {
                push_unique_host(&mut hosts, &name);
            }
        }
    }
    for entry in configured {
        push_unique_host(&mut hosts, entry);
    }
    hosts
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
            "LAN bind must require authentication"
        );
        assert!(
            assert_bind_allowed(&wildcard, false).is_err(),
            "wildcard bind must require authentication"
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
        assert!(
            !advertised.ip().is_unspecified(),
            "advertised address must not be 0.0.0.0"
        );

        let loopback: SocketAddr = "127.0.0.1:18797".parse().unwrap();
        assert_eq!(advertised_endpoint(loopback), loopback);
        assert!(is_virtual_tun_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(!is_usable_lan_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(is_usable_lan_ipv4(Ipv4Addr::new(192, 168, 1, 175)));
    }

    #[test]
    fn allowed_hosts_covers_lan_binds_and_sanitizes_configured_entries() {
        let loopback: SocketAddr = "127.0.0.1:8797".parse().unwrap();
        let loopback_hosts = allowed_hosts_for_listener(Some(loopback), Some(loopback), &[]);
        assert_eq!(loopback_hosts, vec!["localhost", "127.0.0.1", "::1"]);

        let lan: SocketAddr = "192.168.1.175:33612".parse().unwrap();
        let lan_hosts = allowed_hosts_for_listener(
            Some("0.0.0.0:33612".parse().unwrap()),
            Some(lan),
            &[
                "  LAB-BOX.local:33612 ".into(),
                "*".into(),
                "http://bad.example/mcp".into(),
                "192.168.1.175".into(),
            ],
        );
        assert!(lan_hosts.contains(&"127.0.0.1".to_string()));
        assert!(lan_hosts.contains(&"192.168.1.175".to_string()));
        assert!(lan_hosts.contains(&"lab-box.local:33612".to_string()));
        assert!(
            !lan_hosts
                .iter()
                .any(|host| host == "*" || host.contains('/'))
        );
        assert_eq!(
            lan_hosts
                .iter()
                .filter(|host| *host == "192.168.1.175")
                .count(),
            1,
            "duplicate host entries must be deduplicated"
        );
    }
}
