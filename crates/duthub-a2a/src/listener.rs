//! HTTP listener assembly and the per-DUT A2A runtime.
//!
//! Wire shape (outermost → innermost):
//! `Vary: A2A-Version` middleware → merged router → per-route:
//! - `/.well-known/agent-card.json` GET: public discovery (ADR-8; the card
//!   itself contains no sensitive data), CORS headers from `a2a-server-lf`.
//! - `/` POST: Bearer middleware (401 fail-closed) → `RequestBodyLimitLayer`
//!   (256 KiB → 413) → `jsonrpc_router` (A2A-Version negotiation, -32009).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::AgentCapabilities;
use a2a_server::handler::DefaultRequestHandler;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::task_store::InMemoryTaskStore;
use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use duthub_mcp::state::McpState;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::auth::{self, SharedToken, TokenSource};
use crate::card;
use crate::executor::DuthubExecutor;
use crate::ports::{PortOwner, PortRegistry};
use crate::task::{StrictRequestHandler, TaskOwnership};

/// Maximum accepted JSON-RPC request body (W3 gate): larger bodies get
/// `413 Payload Too Large` before parsing.
pub const A2A_BODY_LIMIT_BYTES: usize = 256 * 1024;

/// Grace period for graceful listener shutdown before the serve task aborts.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The only bind address W3 accepts (ADR-2): `None`/`"127.0.0.1"` resolve to
/// the IPv4 loopback; every other value needs product sign-off and is refused.
fn resolve_loopback(bind_addr: Option<&str>) -> Result<IpAddr, String> {
    match bind_addr.map(str::trim) {
        None | Some("" | "127.0.0.1") => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        Some(other) => Err(format!(
            "bind_addr {other:?} is not 127.0.0.1: non-loopback bind requires product sign-off (ADR-2)"
        )),
    }
}

/// Listener status as surfaced by `A2aRuntime::status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum A2aStatus {
    /// No listener for this DUT (or stopped).
    Disabled,
    /// Bind in progress (transient).
    Starting,
    /// Serving on a loopback endpoint.
    Running { endpoint: String },
    /// Startup failed (e.g. token missing) and nothing is bound.
    Failed { reason: String },
    /// Configured address needs product sign-off (ADR-2); nothing is bound.
    NeedsProductSignoff { reason: String },
    /// Port is 0, duplicated, reserved by another subsystem, or OS-bind
    /// failed; nothing is bound.
    PortConflict,
}

/// Per-DUT listener configuration handed to `A2aRuntime::start`.
/// `agent_name` is retained for UI parity only — the Agent Card name is the
/// fixed product word `duthub` (ADR-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aDutConfig {
    pub dut_id: String,
    pub port: u16,
    pub bind_addr: Option<String>,
    pub agent_name: String,
}

/// Fully assembled listener stack; also the test seam (tests arm
/// `with_test_delay` on the executor and drive the router with `oneshot`).
pub struct A2aListener {
    pub router: Router,
    pub executor: DuthubExecutor,
    pub ownership: Arc<TaskOwnership>,
    pub token: SharedToken,
}

impl std::fmt::Debug for A2aListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aListener")
            .field("executor", &self.executor)
            .field("ownership", &self.ownership)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Assemble the A2A HTTP stack for one DUT. Used by [`A2aRuntime::start`]
/// and by the crate e2e tests.
pub fn build_listener(
    state: Arc<dyn McpState>,
    dut_id: &str,
    token: SharedToken,
    card_addr: IpAddr,
    card_port: u16,
) -> A2aListener {
    build_listener_with_executor(
        DuthubExecutor::new(state, dut_id.to_owned()),
        token,
        card_addr,
        card_port,
    )
}

/// Like [`build_listener`], but with a caller-provided executor (tests arm
/// `with_test_delay` on it before assembly).
pub fn build_listener_with_executor(
    executor: DuthubExecutor,
    token: SharedToken,
    card_addr: IpAddr,
    card_port: u16,
) -> A2aListener {
    let dut_id = executor.dut_id();
    let ownership = executor.ownership();
    let handler = DefaultRequestHandler::new(executor.clone(), InMemoryTaskStore::new())
        .with_authorizer(auth::EvidenceAuthorizer::new(
            token.clone(),
            ownership.clone(),
            dut_id,
        ))
        // Card capabilities say false; the handler must refuse the gated
        // operations too (defense in depth on top of StrictRequestHandler).
        .with_capabilities(AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: Some(false),
        });

    let strict = Arc::new(StrictRequestHandler::new(Arc::new(handler)));
    let auth_state = auth::AuthMiddlewareState::new(token.clone());
    let rpc = jsonrpc_router(strict)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            A2A_BODY_LIMIT_BYTES,
        ))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth::require_bearer,
        ));

    let card = card::build_agent_card_at(card_addr, card_port);
    let discovery =
        a2a_server::agent_card::agent_card_router(Arc::new(a2a_server::StaticAgentCard::new(card)));

    let router = rpc
        .merge(discovery)
        .layer(axum::middleware::from_fn(vary_a2a_version));

    A2aListener {
        router,
        executor,
        ownership,
        token,
    }
}

/// Append `Vary: A2A-Version` to every response (append, never overwrite an
/// existing `Vary: Origin` from the card route — ADR-8 §2.2).
pub async fn vary_a2a_version(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("A2A-Version"));
    response
}

struct DutListener {
    status: A2aStatus,
    port: u16,
    owner: PortOwner,
    shutdown: watch::Sender<bool>,
    handle: JoinHandle<()>,
    token: SharedToken,
}

/// Per-DUT A2A listener runtime: binds loopback listeners, tracks their
/// status, swaps tokens live and releases ports on stop.
pub struct A2aRuntime {
    registry: Arc<PortRegistry>,
    token_source: Arc<dyn TokenSource>,
    listeners: Mutex<HashMap<String, DutListener>>,
    /// DUT IDs currently inside `start_one` (observable as `Starting`).
    pending: Mutex<HashSet<String>>,
    /// Last startup failure per DUT (survives until a successful start).
    failures: Mutex<HashMap<String, A2aStatus>>,
}

impl std::fmt::Debug for A2aRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active_duts: Vec<String> = self
            .listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        f.debug_struct("A2aRuntime")
            .field("registry", &self.registry)
            .field("active_duts", &active_duts)
            .field("pending", &self.pending)
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

impl Default for A2aRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl A2aRuntime {
    /// Runtime with the production keyring token source
    /// (`duthub_core::keys`, service `duthub`, entry `a2a/<dut_id>`).
    #[must_use]
    pub fn new() -> Self {
        Self::with_token_source(Arc::new(auth::KeyringTokenSource))
    }

    /// Runtime with an injected token source (tests use in-memory).
    pub fn with_token_source(token_source: Arc<dyn TokenSource>) -> Self {
        Self {
            registry: Arc::new(PortRegistry::new()),
            token_source,
            listeners: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Port registry shared by all listeners of this runtime.
    pub fn port_registry(&self) -> Arc<PortRegistry> {
        self.registry.clone()
    }

    /// Start listeners for every config, in order. `reserved_ports` seeds
    /// the registry with ports held by other subsystems (e.g. MCP
    /// listeners); A2A refuses to bind them.
    pub async fn start(
        &self,
        configs: Vec<A2aDutConfig>,
        state: Arc<dyn McpState>,
        reserved_ports: Vec<u16>,
    ) -> Vec<(String, A2aStatus)> {
        for port in reserved_ports {
            // Reserve failure means someone already holds it; that is fine —
            // the gate below only needs the port to be occupied.
            let _ = self.registry.reserve(
                port,
                PortOwner::External {
                    label: "reserved".to_owned(),
                },
            );
        }
        let mut outcomes = Vec::with_capacity(configs.len());
        for config in configs {
            let status = self.start_one(&config, state.clone()).await;
            outcomes.push((config.dut_id.clone(), status));
        }
        outcomes
    }

    async fn start_one(&self, config: &A2aDutConfig, state: Arc<dyn McpState>) -> A2aStatus {
        let dut_id = config.dut_id.trim().to_owned();
        if dut_id.is_empty() {
            return A2aStatus::Failed {
                reason: "DUT ID is empty".to_owned(),
            };
        }
        if let Some(existing) = self.entry(&dut_id)
            && matches!(existing, A2aStatus::Running { .. })
        {
            return existing;
        }
        self.mark_pending(&dut_id);
        let owner = PortOwner::A2a {
            dut_id: dut_id.clone(),
        };

        // ── Port gate ────────────────────────────────────────────────────
        if config.port == 0 {
            return self.reject(&dut_id, None, A2aStatus::PortConflict);
        }
        if let Err(taken) = self.registry.reserve(config.port, owner.clone()) {
            tracing::info!(
                dut_id,
                port = config.port,
                holder = %taken.by.label(),
                "A2A listener port conflict"
            );
            return self.reject(&dut_id, None, A2aStatus::PortConflict);
        }

        // ── Bind-address gate (ADR-2) ────────────────────────────────────
        let bind_ip = match resolve_loopback(config.bind_addr.as_deref()) {
            Ok(ip) => ip,
            Err(reason) => {
                return self.reject(
                    &dut_id,
                    Some(config.port),
                    A2aStatus::NeedsProductSignoff { reason },
                );
            }
        };

        // ── Token gate (fail closed: no credential, no bind) ─────────────
        let token = match self.load_token(&dut_id).await {
            Ok(token) if !token.trim().is_empty() => token,
            Ok(_) => {
                return self.reject(
                    &dut_id,
                    Some(config.port),
                    A2aStatus::Failed {
                        reason: format!(
                            "A2A token not configured (keyring duthub/a2a/{dut_id}); refusing to bind (fail closed)"
                        ),
                    },
                );
            }
            Err(error) => {
                return self.reject(
                    &dut_id,
                    Some(config.port),
                    A2aStatus::Failed {
                        reason: format!("failed to read A2A token: {error}"),
                    },
                );
            }
        };

        // ── Bind ─────────────────────────────────────────────────────────
        let tcp = match tokio::net::TcpListener::bind((bind_ip, config.port)).await {
            Ok(tcp) => tcp,
            Err(error) => {
                tracing::warn!(dut_id, port = config.port, %error, "A2A listener bind failed");
                return self.reject(&dut_id, Some(config.port), A2aStatus::PortConflict);
            }
        };
        let endpoint = match tcp.local_addr() {
            Ok(addr) => format!("http://{addr}"),
            Err(_) => format!("http://{bind_ip}:{}", config.port),
        };

        let shared_token = auth::shared_token(&token);
        let listener = build_listener(state, &dut_id, shared_token.clone(), bind_ip, config.port);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            let _ = axum::serve(tcp, listener.router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.wait_for(|stopped| *stopped).await;
                })
                .await;
        });

        let status = A2aStatus::Running { endpoint };
        let mut listeners = self
            .listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        listeners.insert(
            dut_id.clone(),
            DutListener {
                status: status.clone(),
                port: config.port,
                owner,
                shutdown: shutdown_tx,
                handle,
                token: shared_token,
            },
        );
        drop(listeners);
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&dut_id);
        self.unmark_pending(&dut_id);
        status
    }

    /// Current status of one DUT's listener.
    pub fn status(&self, dut_id: &str) -> A2aStatus {
        if let Some(status) = self.entry(dut_id) {
            return status;
        }
        if let Some(status) = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dut_id)
        {
            return status.clone();
        }
        if self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(dut_id)
        {
            return A2aStatus::Starting;
        }
        A2aStatus::Disabled
    }

    /// Swap the bearer token for a running listener. The new token takes
    /// effect immediately (old tokens get 401 on the next request) and is
    /// persisted through the token source for the next start.
    pub async fn set_token(&self, dut_id: &str, token: &str) -> Result<(), String> {
        if token.trim().is_empty() {
            return Err("A2A token cannot be empty".to_owned());
        }
        let source = self.token_source.clone();
        let dut = dut_id.to_owned();
        let owned = token.to_owned();
        tokio::task::spawn_blocking(move || source.store(&dut, &owned))
            .await
            .map_err(|error| format!("token store task failed: {error}"))??;

        let listeners = self
            .listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(listener) = listeners.get(dut_id) {
            let mut guard = match listener.token.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard = Arc::from(token);
        }
        Ok(())
    }

    /// Stop one DUT's listener and release its port reservation.
    pub async fn stop_dut(&self, dut_id: &str) {
        let entry = self
            .listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(dut_id);
        let Some(entry) = entry else {
            return;
        };
        let _ = entry.shutdown.send(true);
        let mut handle = entry.handle;
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
        }
        self.registry.release(entry.port, &entry.owner);
    }

    /// Stop every listener.
    pub async fn shutdown(&self) {
        let dut_ids: Vec<String> = self
            .listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for dut_id in dut_ids {
            self.stop_dut(&dut_id).await;
        }
    }

    fn entry(&self, dut_id: &str) -> Option<A2aStatus> {
        self.listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dut_id)
            .map(|listener| listener.status.clone())
    }

    fn mark_pending(&self, dut_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(dut_id.to_owned());
    }

    fn unmark_pending(&self, dut_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(dut_id);
    }

    /// Drop the pending mark, release a port reserved for this attempt and
    /// remember the failure for `status()` until a later start succeeds.
    fn reject(&self, dut_id: &str, port: Option<u16>, status: A2aStatus) -> A2aStatus {
        self.unmark_pending(dut_id);
        if let Some(port) = port {
            self.registry.release(
                port,
                &PortOwner::A2a {
                    dut_id: dut_id.to_owned(),
                },
            );
        }
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(dut_id.to_owned(), status.clone());
        status
    }

    async fn load_token(&self, dut_id: &str) -> Result<String, String> {
        let source = self.token_source.clone();
        let dut = dut_id.to_owned();
        let loaded = tokio::task::spawn_blocking(move || source.load(&dut))
            .await
            .map_err(|error| format!("token load task failed: {error}"))??;
        Ok(loaded.unwrap_or_default())
    }
}
