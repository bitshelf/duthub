//! HTTP Bearer 认证服务包装 + MCP 层租约/授权辅助（`docs/SECURITY.md`）。
//!
//! - 每请求读 `Authorization: Bearer <token>`（禁止 query 参数，SPEC §5.1.4）；
//!   auth 开启：指纹定位候选 → `subtle` 常量时间比较 → `Principal::Agent` 注入
//!   request extensions；未超限的失败统一 `401 {"error":"unauthorized"}`
//!   （不泄露「token 是否存在/属于哪个 agent」）+ 审计 auth.fail（无 token 内容）。
//! - 同一监听器在滑动窗口内失败超过 [`AUTH_FAILURE_LIMIT`] 后统一 `429`
//!   （`Retry-After: 60`），且只记一条聚合 auth.fail（防 LAN 暴力猜 token）。
//! - auth 关闭时 HTTP 返回 401，无法通过回环地址获得本机管理员权限；
//!   写工具的租约要求仍由 device 写入通道 + server 层复核（GUI 优先无条件生效）。
//! - stdio 无 HTTP parts：[`CurrentPrincipal`] 提取器回退 `LocalAdmin`（偏差 5）；
//!   带认证上下文的服务器（HTTP）缺少 Parts 时 fail closed（401），不再误回退。
//! - Principal 不从工具参数取（SPEC §7）：全部来自本服务注入。
//!
//! 实现注：auth 层是手写 tower `Service` 包装（而非 `axum::middleware::from_fn`）——
//! from_fn 的提取器元组推断在本工作区 rmcp+axum 组合下不收敛（tokio/axum 0.8.9
//! 最小复现通过、接入 rmcp 路由后 Service 绑定不满足），手写实现完全确定且
//! 不引入额外依赖形态。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use axum::response::{IntoResponse, Response};

use duthub_core::Principal;
use duthub_core::agent_access::{AgentAccessRuntime, AuthError};
use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::lease::{ControlLeaseManager, LeaseError, LeaseHolder};

use rmcp::handler::server::common::{AsRequestContext, FromContextPart};
use rmcp::handler::server::tool::ToolCallContext;

use crate::server::McpServerCore;

/// MCP 层共享的接入上下文：认证运行时 + 租约管理器 + 审计（desktop 构造注入，
/// 测试可全内存构造）。server.rs 与 auth 服务共用同一实例（复核口径一致）。
#[derive(Clone)]
pub struct AccessContext {
    pub auth: Arc<AgentAccessRuntime>,
    pub lease: Arc<ControlLeaseManager>,
    pub audit: Arc<AuditSink>,
}

impl AccessContext {
    pub fn new(
        auth: Arc<AgentAccessRuntime>,
        lease: Arc<ControlLeaseManager>,
        audit: Arc<AuditSink>,
    ) -> Self {
        Self { auth, lease, audit }
    }
}

/// Default failed-auth budget per sliding window (LAN brute-force hardening).
pub const AUTH_FAILURE_LIMIT: usize = 60;
/// Default sliding window for [`AUTH_FAILURE_LIMIT`].
pub const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// How a failed authentication attempt should be handled by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailureDecision {
    /// Still within budget: audit the failure normally.
    Audit,
    /// The budget was just exceeded: audit one aggregated failure event.
    Aggregated { total: usize },
    /// Already limited: suppress the per-request audit line.
    Suppressed,
}

#[derive(Default)]
struct AuthFailureWindow {
    failures: VecDeque<Instant>,
    tripped: bool,
}

/// Sliding-window limiter for failed Bearer authentication attempts.
///
/// Successful requests are not recorded. Once the number of failures inside
/// the window exceeds the configured limit, callers should answer `429` and
/// stop writing one audit line per attempt (only the first trip is audited).
pub struct AuthFailureLimiter {
    limit: usize,
    window: Duration,
    state: Mutex<AuthFailureWindow>,
}

impl Default for AuthFailureLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthFailureLimiter {
    pub fn new() -> Self {
        Self::with_limits(AUTH_FAILURE_LIMIT, AUTH_FAILURE_WINDOW)
    }

    /// Test/host override for the failure budget.
    pub fn with_limits(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            state: Mutex::new(AuthFailureWindow::default()),
        }
    }

    /// Record one failed attempt and report whether it should be audited.
    pub fn record_failure(&self) -> AuthFailureDecision {
        let mut state = self.state.lock().unwrap();
        self.prune(&mut state);
        if state.failures.len() <= self.limit {
            state.tripped = false;
        }
        state.failures.push_back(Instant::now());
        let total = state.failures.len();
        if total > self.limit {
            if state.tripped {
                AuthFailureDecision::Suppressed
            } else {
                state.tripped = true;
                AuthFailureDecision::Aggregated { total }
            }
        } else {
            AuthFailureDecision::Audit
        }
    }

    /// True while the failure count inside the window exceeds the limit.
    pub fn is_limited(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        self.prune(&mut state);
        if state.failures.len() <= self.limit {
            state.tripped = false;
        }
        state.failures.len() > self.limit
    }

    fn prune(&self, state: &mut AuthFailureWindow) {
        let now = Instant::now();
        while state
            .failures
            .front()
            .is_some_and(|seen| now.saturating_duration_since(*seen) >= self.window)
        {
            state.failures.pop_front();
        }
    }
}

/// Bearer 认证服务包装：包住 axum Route（/mcp 端点），认证通过时把
/// [`Principal`] 注入 request extensions——rmcp StreamableHttpService 会把整个
/// `http::request::Parts`（含 extensions）透传进工具调用上下文
/// （tower.rs 注入，方案 §1.2 已验证），工具经 [`CurrentPrincipal`] 取回。
#[derive(Clone)]
pub struct AuthSvc<S> {
    ctx: AccessContext,
    inner: S,
    listener_id: Option<Arc<str>>,
    /// Per-listener failed-auth budget (an attack on one listener cannot lock
    /// out the other listeners sharing the same access context).
    failures: Arc<AuthFailureLimiter>,
}

impl<S> AuthSvc<S> {
    pub fn new(ctx: AccessContext, inner: S) -> Self {
        Self {
            ctx,
            inner,
            listener_id: None,
            failures: Arc::new(AuthFailureLimiter::new()),
        }
    }

    pub(crate) fn on_listener(mut self, listener_id: Option<Arc<str>>) -> Self {
        self.listener_id = listener_id;
        self
    }
}

impl<S> tower_service::Service<Request<Body>> for AuthSvc<S>
where
    S: tower_service::Service<Request<Body>, Error = std::convert::Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Response: IntoResponse,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let ctx = self.ctx.clone();
        let listener_id = self.listener_id.clone();
        let failures = self.failures.clone();
        let mut inner = self.inner.clone();
        Box::pin(async move {
            // Reject before touching credentials while the window is exhausted:
            // LAN clients cannot keep probing tokens at line rate.
            if failures.is_limited() {
                return Ok(rate_limited_response());
            }
            let bearer = bearer_token(req.headers());
            // 每会话监听器（runtime overlay）走 internal 认证：只查 overlay，
            // 不受持久 auth 总开关影响；其余监听器维持持久表语义。
            let internal = listener_id
                .as_deref()
                .is_some_and(|id| ctx.auth.is_runtime_listener(id));
            let identity = match (internal, listener_id.as_deref()) {
                (true, Some(id)) => ctx.auth.authenticate_internal(id, bearer.unwrap_or("")),
                _ => ctx.auth.authenticate_http(listener_id.as_deref(), bearer),
            };
            match identity {
                Ok(principal) => {
                    if let Some((agent, credential_id)) = principal.audit_identity() {
                        ctx.audit.record(
                            "auth.ok",
                            Some(agent),
                            Some(credential_id),
                            None,
                            AuditOutcome::Ok,
                            Some("Bearer 认证通过"),
                        );
                    }
                    req.extensions_mut().insert(principal);
                    inner.call(req).await.map(|resp| resp.into_response())
                }
                Err(err) => {
                    // 失败细节只进审计（脱敏），响应统一 401 不区分原因（SPEC §5.1.6）。
                    // 超过失败预算后只记一条聚合事件，后续失败不再逐条落审计。
                    let decision = failures.record_failure();
                    match decision {
                        AuthFailureDecision::Audit => {
                            ctx.audit.record(
                                "auth.fail",
                                None,
                                None,
                                None,
                                AuditOutcome::Denied,
                                Some(auth_fail_detail(&err).as_str()),
                            );
                        }
                        AuthFailureDecision::Aggregated { total } => {
                            ctx.audit.record(
                                "auth.fail",
                                None,
                                None,
                                None,
                                AuditOutcome::Denied,
                                Some(&format!("认证失败 {total} 次/窗口，已限流 (rate_limited)")),
                            );
                        }
                        AuthFailureDecision::Suppressed => {}
                    }
                    if matches!(decision, AuthFailureDecision::Aggregated { .. })
                        || failures.is_limited()
                    {
                        Ok(rate_limited_response())
                    } else {
                        Ok(unauthorized_response())
                    }
                }
            }
        })
    }
}

/// 401 统一响应体：固定 `{"error":"unauthorized"}`（无任何原因区分）。
pub(crate) fn unauthorized_response() -> Response {
    (
        axum::http::StatusCode::UNAUTHORIZED,
        [("content-type", "application/json")],
        "{\"error\":\"unauthorized\"}",
    )
        .into_response()
}

/// 429 response after the failed-auth budget is exhausted.
pub(crate) fn rate_limited_response() -> Response {
    (
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        [("content-type", "application/json"), ("retry-after", "60")],
        "{\"error\":\"too_many_requests\"}",
    )
        .into_response()
}

fn auth_fail_detail(err: &AuthError) -> String {
    match err {
        AuthError::Unauthorized => "Bearer 缺失/无效或凭据停用".into(),
        AuthError::NotGranted { .. } => "授权不足".into(),
        AuthError::InvalidListener(_) => "运行时监听器注册参数非法".into(),
    }
}

/// 提取 `Authorization: Bearer <token>`（仅 header，禁止 query 参数）。
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = rest.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// 工具调用上下文中的当前主体：HTTP 请求经 AuthSvc 注入；stdio（无 HTTP parts）
/// 回退 LocalAdmin（偏差 5）。rmcp `#[tool]` 参数提取器。
pub struct CurrentPrincipal(pub Principal);

/// Resolve the tool-call principal from injected HTTP parts.
///
/// `has_access = true` means this server has an auth context (HTTP listener):
/// a missing `Parts` must fail closed instead of silently degrading to
/// [`Principal::LocalAdmin`]. Only access-less servers (stdio/in-process) fall
/// back to LocalAdmin.
pub fn principal_from_parts(
    parts: Option<&http::request::Parts>,
    has_access: bool,
) -> Result<Principal, rmcp::ErrorData> {
    match parts {
        Some(parts) => parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| unauthorized_error("*", "authenticate")),
        None if has_access => Err(unauthorized_error("*", "authenticate")),
        None => Ok(Principal::LocalAdmin),
    }
}

impl FromContextPart<ToolCallContext<'_, McpServerCore>> for CurrentPrincipal {
    fn from_context_part(
        context: &mut ToolCallContext<'_, McpServerCore>,
    ) -> Result<Self, rmcp::ErrorData> {
        let parts = context
            .as_request_context()
            .extensions
            .get::<http::request::Parts>();
        principal_from_parts(parts, context.service.has_access()).map(CurrentPrincipal)
    }
}

/// 服务端确认的租约持有者（SPEC §7：不接受客户端自报身份）。
/// LocalAdmin（auth 关/stdio）以稳定占位凭据持有租约——句柄即权限（SPEC §6.3）。
pub fn holder_for(principal: &Principal) -> LeaseHolder {
    match principal {
        Principal::LocalAdmin => LeaseHolder::Agent {
            agent_name: "local-admin".into(),
            credential_id: "local-admin".into(),
        },
        Principal::Agent {
            agent_name,
            credential_id,
        } => LeaseHolder::Agent {
            agent_name: agent_name.clone(),
            credential_id: credential_id.clone(),
        },
    }
}

/// 租约错误 → MCP 错误码（`LeaseRequired/{code}` 前缀约定，方案 §6.2；
/// 与既有 `ApprovalRequired/{code}` 同型）。
pub fn lease_error_code(err: &LeaseError) -> &'static str {
    match err {
        LeaseError::Busy { .. } => "DUT_BUSY",
        // P2-019：等待超时/排队拒均为忙族的等待侧结果，码面与工具描述
        // 「超时返回 LeaseRequired/DUT_BUSY」一致，语义由 message 承载
        LeaseError::WaitTimeout { .. } | LeaseError::QueuedAhead { .. } => "DUT_BUSY",
        LeaseError::Expired => "LEASE_EXPIRED",
        LeaseError::UnknownLease => "LEASE_UNKNOWN",
        LeaseError::NotHolder => "NOT_HOLDER",
        LeaseError::Protected { .. } | LeaseError::NeedsInspection => "DUT_PROTECTED",
        LeaseError::Generation { .. } => "GENERATION_STALE",
        LeaseError::Unauthorized => "UNAUTHORIZED",
        LeaseError::LeaseRequired => "Missing",
    }
}

/// 租约错误 → isError 工具结果（客户端可按前缀+码解析）。
pub fn lease_error(err: &LeaseError) -> rmcp::ErrorData {
    rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32000),
        format!("LeaseRequired/{}: {}", lease_error_code(err), err.message()),
        Some(serde_json::json!({ "kind": "lease", "code": lease_error_code(err) })),
    )
}

/// 授权拒绝 → `LeaseRequired/UNAUTHORIZED`（SPEC §6.4：业务互斥不伪装 401）。
pub fn unauthorized_error(dut_id: &str, action: &str) -> rmcp::ErrorData {
    rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32000),
        format!("LeaseRequired/UNAUTHORIZED: 未授权：凭据未被授予 {dut_id} 的 {action} 动作"),
        Some(serde_json::json!({ "kind": "authorization", "code": "UNAUTHORIZED" })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::agent_access::{AgentAccessSettings, DutAction, DutGrant};
    use std::time::Duration;

    fn ctx(enabled: bool, token: &str) -> AccessContext {
        AccessContext::new(
            AgentAccessRuntime::from_settings(AgentAccessSettings {
                enabled,
                agents: vec![duthub_core::AgentCredential {
                    listener_id: None,
                    id: "cred-a".into(),
                    agent_name: "bot".into(),
                    token: token.into(),
                    enabled: true,
                    grants: vec![DutGrant {
                        dut_id: "dut-1".into(),
                        actions: vec![DutAction::Read, DutAction::SerialWrite],
                    }],
                }],
                ..Default::default()
            }),
            Arc::new(ControlLeaseManager::new(Some(AuditSink::memory()))),
            AuditSink::memory(),
        )
    }

    #[tokio::test]
    async fn auth_off_denies_even_with_a_previously_configured_bearer() {
        let ctx = ctx(false, "dth-nope");
        // 探针必须打在 AuthSvc 保护的路由上（而非旁路直连 handler）
        let router = axum::Router::new().route_service(
            "/probe",
            AuthSvc::new(
                ctx.clone(),
                axum::routing::post(|req: Request<Body>| async move {
                    assert!(
                        req.extensions().get::<Principal>().is_some(),
                        "AuthSvc 必须注入 Principal"
                    );
                    "ok"
                }),
            ),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // tokio from_std 要求非阻塞（阻塞模式会卡死 current_thread runtime）
        listener.set_nonblocking(true).unwrap();
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, router).await.unwrap();
        });
        for bearer in [None, Some("dth-nope")] {
            let resp = send_probe(addr, bearer).await;
            assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        }
    }

    #[tokio::test]
    async fn auth_on_rejects_missing_and_bad_bearer_with_unified_401() {
        let good = duthub_core::agent_access::generate_token();
        let ctx = ctx(true, &good);
        let audit = ctx.audit.clone();
        let router = axum::Router::new().route_service(
            "/probe",
            AuthSvc::new(
                ctx.clone(),
                axum::routing::post(|req: Request<Body>| async move {
                    let p = req.extensions().get::<Principal>().cloned();
                    assert!(
                        matches!(p, Some(Principal::Agent { .. })),
                        "认证通过必须注入 Agent 主体"
                    );
                    "ok"
                }),
            ),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, router).await.unwrap();
        });
        // 无 Bearer / 错 Bearer → 统一 401 + 固定 body；正确 Bearer → 200 + Agent 主体
        let resp = send_probe(addr, None).await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        assert!(resp.contains("{\"error\":\"unauthorized\"}"), "{resp}");
        let resp = send_probe(addr, Some("dth-wrong")).await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        let resp = send_probe(addr, Some(&good)).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        // 失败审计 auth.fail ×2（无 token 内容）
        let fails = audit
            .recent(50)
            .iter()
            .filter(|e| e.action == "auth.fail")
            .count();
        assert_eq!(fails, 2, "两次失败都落审计");
        let raw_events = format!("{:?}", audit.recent(50));
        assert!(!raw_events.contains("dth-wrong"), "token 不得落审计");
    }

    async fn send_probe(addr: std::net::SocketAddr, auth: Option<&str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream =
            tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
                .await
                .expect("连接超时")
                .unwrap();
        let auth_line = match auth {
            Some(t) => format!("Authorization: Bearer {t}\r\n"),
            None => String::new(),
        };
        let req = format!(
            "POST /probe HTTP/1.1\r\nHost: x\r\n{auth_line}Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
        String::from_utf8_lossy(&buf).to_string()
    }

    #[test]
    fn lease_error_codes_cover_all_variants() {
        assert_eq!(
            lease_error_code(&LeaseError::Busy {
                holder: "x".into(),
                queue_len: 0
            }),
            "DUT_BUSY"
        );
        assert_eq!(lease_error_code(&LeaseError::Expired), "LEASE_EXPIRED");
        assert_eq!(lease_error_code(&LeaseError::UnknownLease), "LEASE_UNKNOWN");
        assert_eq!(lease_error_code(&LeaseError::NotHolder), "NOT_HOLDER");
        assert_eq!(
            lease_error_code(&LeaseError::Protected {
                op: "烧录".into()
            }),
            "DUT_PROTECTED"
        );
        assert_eq!(
            lease_error_code(&LeaseError::NeedsInspection),
            "DUT_PROTECTED"
        );
        assert_eq!(
            lease_error_code(&LeaseError::Generation { current: 3 }),
            "GENERATION_STALE"
        );
        assert_eq!(lease_error_code(&LeaseError::Unauthorized), "UNAUTHORIZED");
        let err = lease_error(&LeaseError::UnknownLease);
        assert!(
            err.message.starts_with("LeaseRequired/LEASE_UNKNOWN"),
            "{}",
            err.message
        );
    }
}
