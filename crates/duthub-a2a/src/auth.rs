//! Bearer authentication for the A2A loopback surface (ADR-2: fail-closed).
//!
//! Two independent checkpoints share one live token handle
//! ([`SharedToken`]): the HTTP middleware rejects before the handler runs
//! (401 + `WWW-Authenticate`), and [`EvidenceAuthorizer`] re-checks the same
//! header from transport-agnostic `ServiceParams` inside
//! `DefaultRequestHandler` — the SDK defaults to fail-open when no authorizer
//! is installed, so one is always installed here.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use a2a::A2AError;
use a2a_server::RequestAuthorizer;
use a2a_server::middleware::ServiceParams;
use axum::response::IntoResponse;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::task::TaskOwnership;

/// Live bearer token: `Arc<RwLock<Arc<str>>>` so `A2aRuntime::set_token` can
/// swap the credential without rebinding the listener.
pub type SharedToken = Arc<RwLock<Arc<str>>>;

#[must_use]
pub fn shared_token(initial: &str) -> SharedToken {
    Arc::new(RwLock::new(Arc::from(initial)))
}

/// Snapshot of the current token (never hold the lock across an await).
pub fn token_snapshot(token: &SharedToken) -> String {
    match token.read() {
        Ok(guard) => guard.to_string(),
        Err(poisoned) => poisoned.into_inner().to_string(),
    }
}

/// Split `Authorization: Bearer <token>` (scheme case-insensitive per RFC 7235).
#[must_use]
pub fn parse_bearer(header_value: &str) -> Option<&str> {
    let (scheme, rest) = header_value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

/// Constant-time comparison: both sides are hashed first, so response timing
/// never reflects token content or length.
#[must_use]
pub fn bearer_matches(header_value: &str, expected: &str) -> bool {
    let Some(provided) = parse_bearer(header_value) else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }
    let provided_digest = Sha256::digest(provided.as_bytes());
    let expected_digest = Sha256::digest(expected.as_bytes());
    provided_digest
        .as_slice()
        .ct_eq(expected_digest.as_slice())
        .into()
}

fn bearer_from_params(params: &ServiceParams) -> Option<&str> {
    params.get("authorization")?.first().map(String::as_str)
}

/// Where the per-DUT bearer token comes from. Production uses the OS keyring
/// (`duthub_core::keys`, service `duthub`, entry `a2a/<dut_id>`); tests inject
/// [`InMemoryTokenSource`].
pub trait TokenSource: Send + Sync + 'static {
    /// `Ok(None)` = no token stored (listener start must fail closed).
    fn load(&self, dut_id: &str) -> Result<Option<String>, String>;
    /// Persist a token issued via `A2aRuntime::set_token`.
    fn store(&self, dut_id: &str, token: &str) -> Result<(), String>;
}

/// OS keyring backed token source.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringTokenSource;

impl TokenSource for KeyringTokenSource {
    fn load(&self, dut_id: &str) -> Result<Option<String>, String> {
        duthub_core::keys::get_a2a_token(dut_id).map_err(|error| error.to_string())
    }

    fn store(&self, dut_id: &str, token: &str) -> Result<(), String> {
        duthub_core::keys::set_a2a_token(dut_id, token).map_err(|error| error.to_string())
    }
}

/// In-memory token source for tests (never touches the OS keyring).
#[derive(Debug, Default)]
pub struct InMemoryTokenSource {
    tokens: Mutex<HashMap<String, String>>,
}

impl InMemoryTokenSource {
    pub fn new(pairs: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>) -> Self {
        Self {
            tokens: Mutex::new(
                pairs
                    .into_iter()
                    .map(|(dut_id, token)| (dut_id.into(), token.into()))
                    .collect(),
            ),
        }
    }
}

impl TokenSource for InMemoryTokenSource {
    fn load(&self, dut_id: &str) -> Result<Option<String>, String> {
        Ok(self
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dut_id)
            .cloned())
    }

    fn store(&self, dut_id: &str, token: &str) -> Result<(), String> {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(dut_id.to_owned(), token.to_owned());
        Ok(())
    }
}

/// Request-level authorizer installed on `DefaultRequestHandler`: re-checks
/// the Bearer header and refuses tasks this listener's DUT does not own.
pub struct EvidenceAuthorizer {
    token: SharedToken,
    ownership: Arc<TaskOwnership>,
    dut_id: Arc<str>,
}

impl std::fmt::Debug for EvidenceAuthorizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvidenceAuthorizer")
            .field("token", &"<redacted>")
            .field("ownership", &self.ownership)
            .field("dut_id", &self.dut_id)
            .finish()
    }
}

impl EvidenceAuthorizer {
    pub fn new(token: SharedToken, ownership: Arc<TaskOwnership>, dut_id: Arc<str>) -> Self {
        Self {
            token,
            ownership,
            dut_id,
        }
    }
}

impl RequestAuthorizer for EvidenceAuthorizer {
    fn authorize(&self, params: &ServiceParams, task_id: Option<&str>) -> Result<(), A2AError> {
        let expected = token_snapshot(&self.token);
        let authorized =
            bearer_from_params(params).is_some_and(|header| bearer_matches(header, &expected));
        if !authorized {
            return Err(A2AError::invalid_request(
                "UNAUTHORIZED: bearer token missing or invalid",
            ));
        }
        if let Some(task_id) = task_id
            && !self.ownership.is_owned_by(task_id, &self.dut_id)
        {
            return Err(A2AError::task_not_found(task_id));
        }
        Ok(())
    }
}

/// Auth failure burst window and limit for the A2A loopback listener.
const AUTH_FAIL_WINDOW: Duration = Duration::from_secs(60);
const AUTH_FAIL_BURST_LIMIT: usize = 30;

/// Sliding-window authentication failure limiter to throttle brute-force probes.
#[derive(Debug, Default)]
pub struct AuthFailureLimiter {
    failures: Mutex<VecDeque<Instant>>,
}

impl AuthFailureLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_throttled(&self) -> bool {
        let now = Instant::now();
        let mut guard = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while guard
            .front()
            .is_some_and(|t| now.duration_since(*t) >= AUTH_FAIL_WINDOW)
        {
            guard.pop_front();
        }
        guard.len() >= AUTH_FAIL_BURST_LIMIT
    }

    pub fn record_failure(&self) {
        let now = Instant::now();
        let mut guard = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while guard
            .front()
            .is_some_and(|t| now.duration_since(*t) >= AUTH_FAIL_WINDOW)
        {
            guard.pop_front();
        }
        guard.push_back(now);
    }
}

/// Middleware state combining the live bearer token and failure rate limiter.
#[derive(Clone)]
pub struct AuthMiddlewareState {
    pub token: SharedToken,
    pub limiter: Arc<AuthFailureLimiter>,
}

impl std::fmt::Debug for AuthMiddlewareState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthMiddlewareState")
            .field("token", &"<redacted>")
            .field("limiter", &self.limiter)
            .finish()
    }
}

impl AuthMiddlewareState {
    pub fn new(token: SharedToken) -> Self {
        Self {
            token,
            limiter: Arc::new(AuthFailureLimiter::new()),
        }
    }
}

/// Reject browser-supplied non-loopback `Origin` headers to prevent cross-site
/// DNS-rebinding / CSRF against the loopback JSON-RPC endpoint.
#[must_use]
pub fn is_allowed_loopback_origin(origin: &str) -> bool {
    let trimmed = origin.trim();
    let Some(rest) = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// HTTP middleware:
/// 1. Reject non-loopback `Origin` headers with `403 Forbidden`.
/// 2. Reject brute-force failure bursts with `429 Too Many Requests`.
/// 3. Require the live Bearer token (401 + `WWW-Authenticate: Bearer`).
///
/// Runs before the body-limit layer so rejected requests never read the body.
pub async fn require_bearer(
    axum::extract::State(state): axum::extract::State<AuthMiddlewareState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(origin) = request.headers().get(axum::http::header::ORIGIN) {
        let allowed = origin.to_str().is_ok_and(is_allowed_loopback_origin);
        if !allowed {
            return (
                axum::http::StatusCode::FORBIDDEN,
                "forbidden cross-origin request",
            )
                .into_response();
        }
    }
    if state.limiter.is_throttled() {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "60")],
            "too many authentication failures",
        )
            .into_response();
    }
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let expected = token_snapshot(&state.token);
    let authorized = header.is_some_and(|header| bearer_matches(&header, &expected));
    if !authorized {
        state.limiter.record_failure();
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized",
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bearer_accepts_case_insensitive_scheme() {
        assert_eq!(parse_bearer("Bearer tok-1"), Some("tok-1"));
        assert_eq!(parse_bearer("bearer tok-2"), Some("tok-2"));
        assert_eq!(parse_bearer("BEARER tok-3"), Some("tok-3"));
        assert_eq!(parse_bearer("Basic tok"), None);
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer(""), None);
    }

    #[test]
    fn bearer_matches_rejects_wrong_and_missing() {
        assert!(bearer_matches("Bearer secret", "secret"));
        assert!(!bearer_matches("Bearer nope", "secret"));
        assert!(!bearer_matches("Bearer", "secret"));
        assert!(!bearer_matches("Bearer secret", ""));
        // Empty bearer token never matches anything (fail closed).
        assert!(!bearer_matches("Bearer ", "x"));
    }

    #[test]
    fn loopback_origin_check_blocks_external_hosts() {
        assert!(is_allowed_loopback_origin("http://127.0.0.1:19800"));
        assert!(is_allowed_loopback_origin("http://localhost:3000"));
        assert!(is_allowed_loopback_origin("https://[::1]:8443"));
        assert!(!is_allowed_loopback_origin("https://evil.example"));
        assert!(!is_allowed_loopback_origin("http://127.0.0.1.evil.example"));
        assert!(!is_allowed_loopback_origin("null"));
    }

    #[test]
    fn auth_failure_limiter_throttles_after_burst() {
        let limiter = AuthFailureLimiter::new();
        for _ in 0..AUTH_FAIL_BURST_LIMIT {
            assert!(!limiter.is_throttled());
            limiter.record_failure();
        }
        assert!(limiter.is_throttled());
    }
}
