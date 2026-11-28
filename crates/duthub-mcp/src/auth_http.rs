//! Bearer authentication and the MCP authorization context.
//!
//! Read the bearer token from the authorization header.
//! Use its fingerprint to locate a candidate, then verify it in constant time.
//! Attach the authenticated Agent principal to the request extensions.
//! Return the same unauthorized response for every failure and audit without token contents.
//! Apply the authentication failure limit per listener and time window.
//! Rate-limited requests receive 429 with a retry delay and share an aggregate audit event.
//! HTTP requests without authentication never acquire local administrator privileges.
//! The tool handler and device admission still enforce grants and leases.
//! Stdio requests have no HTTP parts and use the local administrator principal.
//! An HTTP request without the expected parts is denied.
//! Tools get the principal from the server, never from tool arguments.
//!
//! Wrap the router with a Tower service.
//! The middleware function's inferred extractor types did not compose with the rmcp service.
//! The service wrapper makes the request context explicit.
//!

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::response::{IntoResponse, Response};

use duthub_core::Principal;
use duthub_core::agent_access::{AgentAccessRuntime, AuthError};
use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::lease::{ControlLeaseManager, LeaseError, LeaseHolder};

use rmcp::handler::server::common::{AsRequestContext, FromContextPart};
use rmcp::handler::server::tool::ToolCallContext;

use crate::server::McpServerCore;

/// Shared authentication, lease, audit, and active-upload tracking context used by
/// middleware, HTTP transfer routes, and MCP tool handlers.
/// Tests can supply in-memory implementations.
#[derive(Clone)]
pub struct AccessContext {
    pub auth: Arc<AgentAccessRuntime>,
    pub lease: Arc<ControlLeaseManager>,
    pub audit: Arc<AuditSink>,
    active_uploads: Arc<Mutex<HashSet<(String, String)>>>,
}

impl std::fmt::Debug for AccessContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessContext").finish_non_exhaustive()
    }
}

fn normalized_upload_key(dut_id: &str, filename: &str) -> (String, String) {
    let base = filename.trim().rsplit(['/', '\\']).next().unwrap_or("");
    (dut_id.to_owned(), base.to_ascii_lowercase())
}

impl AccessContext {
    pub fn new(
        auth: Arc<AgentAccessRuntime>,
        lease: Arc<ControlLeaseManager>,
        audit: Arc<AuditSink>,
    ) -> Self {
        Self {
            auth,
            lease,
            audit,
            active_uploads: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Reserve an active upload slot for `(dut_id, filename)` (case-insensitive basename).
    /// Returns `false` if another upload for the exact same DUT and filename is already active.
    pub(crate) fn try_begin_upload(&self, dut_id: &str, filename: &str) -> bool {
        let mut guard = duthub_core::util::lock_unpoisoned(&self.active_uploads);
        guard.insert(normalized_upload_key(dut_id, filename))
    }

    /// Release an active upload slot for `(dut_id, filename)`.
    pub(crate) fn end_upload(&self, dut_id: &str, filename: &str) {
        let mut guard = duthub_core::util::lock_unpoisoned(&self.active_uploads);
        guard.remove(&normalized_upload_key(dut_id, filename));
    }

    /// Check whether an upload for `(dut_id, filename)` is currently in progress.
    pub(crate) fn is_upload_active(&self, dut_id: &str, filename: &str) -> bool {
        let guard = duthub_core::util::lock_unpoisoned(&self.active_uploads);
        guard.contains(&normalized_upload_key(dut_id, filename))
    }
}

/// Default failed-auth budget per source inside the sliding window.
pub const AUTH_FAILURE_LIMIT: usize = 60;
/// Default sliding window for [`AUTH_FAILURE_LIMIT`].
pub const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Upper bound on distinct source buckets tracked in memory per listener so
/// spoofed or rotating source addresses cannot grow memory unboundedly.
pub const MAX_TRACKED_AUTH_SOURCES: usize = 1024;
const DEFAULT_AUTH_SOURCE: &str = "unknown";

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
    last_seen: Option<Instant>,
}

/// Per-source sliding-window limiter for failed Bearer authentication attempts.
///
/// Buckets are partitioned by client source IP so brute-force traffic from one
/// peer cannot exhaust the failure budget of legitimate clients on the same
/// listener. Successful requests are never recorded.
pub struct AuthFailureLimiter {
    limit: usize,
    window: Duration,
    max_sources: usize,
    sources: Mutex<HashMap<String, AuthFailureWindow>>,
}

impl std::fmt::Debug for AuthFailureLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthFailureLimiter")
            .field("limit", &self.limit)
            .field("window", &self.window)
            .field("max_sources", &self.max_sources)
            .field("tracked_sources", &self.tracked_sources())
            .finish_non_exhaustive()
    }
}

impl Default for AuthFailureLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthFailureLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(AUTH_FAILURE_LIMIT, AUTH_FAILURE_WINDOW)
    }

    /// Test/host override for the failure budget.
    #[must_use]
    pub fn with_limits(limit: usize, window: Duration) -> Self {
        Self::with_limits_and_capacity(limit, window, MAX_TRACKED_AUTH_SOURCES)
    }

    /// Test override for both the failure budget and tracked source capacity.
    #[must_use]
    pub fn with_limits_and_capacity(limit: usize, window: Duration, max_sources: usize) -> Self {
        Self {
            limit,
            window,
            max_sources: max_sources.max(1),
            sources: Mutex::new(HashMap::new()),
        }
    }

    /// Record one failed attempt on the default bucket (legacy/single-source callers).
    pub fn record_failure(&self) -> AuthFailureDecision {
        self.record_failure_for(DEFAULT_AUTH_SOURCE)
    }

    /// Record one failed attempt for `source` and report whether it should be audited.
    pub fn record_failure_for(&self, source: &str) -> AuthFailureDecision {
        let now = Instant::now();
        let mut map = self
            .sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.evict_if_needed(&mut map, source, now);
        let state = map.entry(source.to_owned()).or_default();
        self.prune(state, now);
        if state.failures.len() <= self.limit {
            state.tripped = false;
        }
        state.failures.push_back(now);
        state.last_seen = Some(now);
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

    /// True while the default bucket exceeds the failure limit.
    pub fn is_limited(&self) -> bool {
        self.is_limited_for(DEFAULT_AUTH_SOURCE)
    }

    /// True while `source` exceeds the failure limit inside the active window.
    pub fn is_limited_for(&self, source: &str) -> bool {
        let now = Instant::now();
        let mut map = self
            .sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = map.get_mut(source) else {
            return false;
        };
        self.prune(state, now);
        if state.failures.is_empty() {
            map.remove(source);
            return false;
        }
        if state.failures.len() <= self.limit {
            state.tripped = false;
        }
        state.failures.len() > self.limit
    }

    /// Number of currently tracked source buckets (diagnostics / tests).
    pub fn tracked_sources(&self) -> usize {
        self.sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn evict_if_needed(
        &self,
        map: &mut HashMap<String, AuthFailureWindow>,
        incoming: &str,
        now: Instant,
    ) {
        if map.contains_key(incoming) || map.len() < self.max_sources {
            return;
        }
        // First reclaim any buckets whose failures have already expired.
        map.retain(|_, window| {
            self.prune(window, now);
            !window.failures.is_empty()
        });
        if map.len() < self.max_sources {
            return;
        }
        // Still full: evict the least-recently-seen source so memory stays bounded.
        if let Some(oldest_key) = map
            .iter()
            .min_by_key(|(_, window)| window.last_seen)
            .map(|(key, _)| key.clone())
        {
            map.remove(&oldest_key);
        }
    }

    fn prune(&self, state: &mut AuthFailureWindow, now: Instant) {
        while state
            .failures
            .front()
            .is_some_and(|seen| now.saturating_duration_since(*seen) >= self.window)
        {
            state.failures.pop_front();
        }
    }
}

/// Wrap a Streamable HTTP route with bearer authentication.
/// Put the verified principal in the request extensions.
/// The transport forwards the request parts into the tool request context.
/// Tools retrieve the principal with `CurrentPrincipal`.
#[derive(Clone)]
pub struct AuthSvc<S> {
    ctx: AccessContext,
    inner: S,
    listener_id: Option<Arc<str>>,
    /// Per-listener failed-auth budget (an attack on one listener cannot lock
    /// out the other listeners sharing the same access context).
    failures: Arc<AuthFailureLimiter>,
}

impl<S> std::fmt::Debug for AuthSvc<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSvc")
            .field("ctx", &self.ctx)
            .field("listener_id", &self.listener_id)
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
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
        let source_key = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map_or_else(
                || DEFAULT_AUTH_SOURCE.to_owned(),
                |ConnectInfo(addr)| addr.ip().to_string(),
            );
        Box::pin(async move {
            // Reject before touching credentials while this source's window is
            // exhausted: brute-force traffic from one peer cannot keep probing
            // at line rate, while other source IPs keep their own budget.
            if failures.is_limited_for(&source_key) {
                return Ok(rate_limited_response());
            }
            let bearer = bearer_token(req.headers());
            // Internal session listeners authenticate against the runtime overlay.
            // Other listeners use persistent credentials and their authentication settings.
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
                    inner
                        .call(req)
                        .await
                        .map(axum::response::IntoResponse::into_response)
                }
                Err(err) => {
                    // Log the redacted failure reason and return a generic 401.
                    // After the failure limit, log a single aggregate event.
                    let decision = failures.record_failure_for(&source_key);
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
                                Some(&format!(
                                    "来源 {source_key} 认证失败 {total} 次/窗口，已限流 (rate_limited)"
                                )),
                            );
                        }
                        AuthFailureDecision::Suppressed => {}
                    }
                    if matches!(decision, AuthFailureDecision::Aggregated { .. })
                        || failures.is_limited_for(&source_key)
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

/// Use a uniform unauthorized response regardless of the reason.
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

/// Extract a bearer token from the authorization header only.
pub(crate) fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
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

/// Get the principal injected into the HTTP tool context.
/// Local stdio requests use the local administrator principal.
#[derive(Debug)]
pub struct CurrentPrincipal(pub Principal);

/// Resolve the tool-call principal from injected HTTP parts.
///
/// `has_access = true` means this server has an auth context (HTTP listener):
/// a missing `Parts` must fail closed instead of silently degrading to
/// [`Principal::LocalAdmin`]. Only access-less servers (stdio/in-process) fall
/// back to `LocalAdmin`.
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

/// Build the lease identity from the server-verified principal.
/// Local administrator leases use a stable identity.
#[must_use]
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

/// Map lease errors to the machine-readable MCP error codes.
/// Approval errors use the same prefix-plus-code convention.
#[must_use]
pub fn lease_error_code(err: &LeaseError) -> &'static str {
    match err {
        // P2-019: WaitTimeout and QueuedAhead are busy-family wait outcomes;
        // they share the DUT_BUSY code while the message carries the detail.
        LeaseError::Busy { .. }
        | LeaseError::WaitTimeout { .. }
        | LeaseError::QueuedAhead { .. } => "DUT_BUSY",
        LeaseError::Expired => "LEASE_EXPIRED",
        LeaseError::UnknownLease => "LEASE_UNKNOWN",
        LeaseError::NotHolder => "NOT_HOLDER",
        LeaseError::Protected { .. } | LeaseError::NeedsInspection => "DUT_PROTECTED",
        LeaseError::Generation { .. } => "GENERATION_STALE",
        LeaseError::Unauthorized => "UNAUTHORIZED",
        LeaseError::LeaseRequired => "Missing",
    }
}

/// Report a lease failure as a tool error with its stable code.
#[must_use]
pub fn lease_error(err: &LeaseError) -> rmcp::ErrorData {
    rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32000),
        format!("LeaseRequired/{}: {}", lease_error_code(err), err.message()),
        Some(serde_json::json!({ "kind": "lease", "code": lease_error_code(err) })),
    )
}

/// Report denied authorization as a tool error rather than a transport-level 401.
#[must_use]
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
        // Send the probe through the authenticated route rather than calling the handler directly.
        let router = axum::Router::new().route_service(
            "/probe",
            AuthSvc::new(
                ctx.clone(),
                axum::routing::post(|req: Request<Body>| async move {
                    assert!(
                        req.extensions().get::<Principal>().is_some(),
                        "AuthSvc must inject Principal"
                    );
                    "ok"
                }),
            ),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Use a nonblocking socket for the Tokio listener.
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
                        "successful auth must inject an Agent principal"
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
        // Missing and invalid tokens must produce the same 401. A valid token exposes its Agent identity.
        let resp = send_probe(addr, None).await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        assert!(resp.contains("{\"error\":\"unauthorized\"}"), "{resp}");
        let resp = send_probe(addr, Some("dth-wrong")).await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        let resp = send_probe(addr, Some(&good)).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        // Count both failures without recording either token.
        let fails = audit
            .recent(50)
            .iter()
            .filter(|e| e.action == "auth.fail")
            .count();
        assert_eq!(fails, 2, "both auth failures must be audited");
        let raw_events = format!("{:?}", audit.recent(50));
        assert!(
            !raw_events.contains("dth-wrong"),
            "token must not appear in audit records"
        );
    }

    async fn send_probe(addr: std::net::SocketAddr, auth: Option<&str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream =
            tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
                .await
                .expect("connect timeout")
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

    #[test]
    fn per_source_failure_limiter_isolates_attackers_and_bounds_memory() {
        let limiter = AuthFailureLimiter::with_limits_and_capacity(3, Duration::from_secs(60), 4);
        // Attacker exhausts its own source bucket.
        for _ in 0..3 {
            assert_eq!(
                limiter.record_failure_for("10.0.0.99"),
                AuthFailureDecision::Audit
            );
        }
        assert_eq!(
            limiter.record_failure_for("10.0.0.99"),
            AuthFailureDecision::Aggregated { total: 4 }
        );
        assert_eq!(
            limiter.record_failure_for("10.0.0.99"),
            AuthFailureDecision::Suppressed
        );
        assert!(limiter.is_limited_for("10.0.0.99"));

        // Legitimate peers on other IPs remain completely unaffected.
        assert!(!limiter.is_limited_for("10.0.0.10"));
        assert!(!limiter.is_limited_for("127.0.0.1"));

        // Flood of distinct source IPs stays bounded at max_sources.
        for idx in 0..20 {
            let _ = limiter.record_failure_for(&format!("192.168.1.{idx}"));
        }
        assert!(limiter.tracked_sources() <= 4);
    }
}
