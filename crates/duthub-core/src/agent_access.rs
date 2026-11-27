//! Agent integration with secrets, authorization models, and authentication processes are described
//! in `docs/SECURITY.md`.
//!
//! The first and second levels of the fourth-level semantics (SPEC §14): **Authentication
//! (authenticate) = Who can come;** **Authorization (authorize) = Who can do things.** The lease
//! (lease.rs) determines who controls at this moment. **Write channel review (duthub-device) = Who
//! ultimately executes hardware actions.**
//!
//! - HTTP requests fail closed with status 401 while authentication is disabled. The stdio
//!   transport and in-process GUI still use `Principal::LocalAdmin`. With authentication enabled,
//!   requests need a valid credential and an explicit grant. `AgentAccessRuntime::enabled` reports
//!   whether listeners are configured for the GUI, independently of the HTTP authorization setting.
//! - Credential records and grants are stored in the `agentAccess` settings section. Default
//!   settings omit that section.
//! - The runtime indexes tokens by SHA-256 fingerprint and compares them in constant time. Audit
//!   records never contain raw tokens.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Default value for the MCP HTTP listening port (range: 8; configurable; takes effect after
/// restart; by default, it is only bound to the loopback interface).
fn default_http_port() -> u16 {
    8797
}

/// The lease uses the default TTL in seconds (SPEC §6.1).
fn default_lease_ttl() -> u32 {
    120
}

fn default_true() -> bool {
    true
}

/// The `agentAccess` field in settings.json: list of credentials, auth switch, port number, and
/// TTL.
///
/// Hole #19/#24: The default value means that this section is skipped during serialization
/// (`agent_access_is_default`). In older versions of settings.json, this section did not exist at
/// all → default setting (auth disabled, no credentials used).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccessSettings {
    /// Auth switch: `false` (default) = HTTP always returns 401 (fail closed; stdio/GUI still use
    /// `LocalAdmin`); `true` = deny by default. Tools require authorization and a lease.
    #[serde(default)]
    pub enabled: bool,
    /// Local network access (default: false; only binds to 127.0.0.1). When enabled, MCP will
    /// listen on `0.0.0.0`. Both loopback addresses and local network addresses can be reached
    /// simultaneously. Authentication must also be enabled (fail-close during runtime).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lan_access: bool,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_lease_ttl")]
    pub lease_ttl_secs: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentCredential>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<AgentListener>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentListener {
    pub id: String,
    pub port: u16,
    /// `Some(dut_id)` = Dedicated endpoint for the DUT (reads data related to that DUT, used for
    /// connection management and maintenance);
    /// `None` = General-purpose listener (used by agents for access pages/LLM operations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dut_id: Option<String>,
}

impl AgentAccessSettings {
    /// Select the dedicated listener based on the DUT (each DUT can have at most one such
    /// listener).
    #[must_use]
    pub fn listener_for_dut(&self, dut_id: &str) -> Option<&AgentListener> {
        self.listeners
            .iter()
            .find(|listener| listener.dut_id.as_deref() == Some(dut_id))
    }

    /// Check the bound credentials using the listener (one per listener for DUT-specific
    /// endpoints).
    #[must_use]
    pub fn credential_for_listener(&self, listener_id: &str) -> Option<&AgentCredential> {
        self.agents
            .iter()
            .find(|cred| cred.listener_id.as_deref() == Some(listener_id))
    }
}

impl Default for AgentAccessSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            lan_access: false,
            http_port: default_http_port(),
            lease_ttl_secs: default_lease_ttl(),
            agents: Vec::new(),
            listeners: Vec::new(),
        }
    }
}

/// Default value determination (used in model.rs `skip_serializing_if`): The entire default value
/// segment is not serialized at all.
#[must_use]
pub fn agent_access_is_default(settings: &AgentAccessSettings) -> bool {
    settings == &AgentAccessSettings::default()
}

/// Authorization for each DUT's actions (SPEC §5.2: Deny by default; No grants = no authorization
/// granted to any DUT).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DutGrant {
    pub dut_id: String,
    /// Actions this credential may perform on the DUT.
    ///
    /// Values written by a newer build deserialize to [`DutAction::Unknown`]
    /// (serde `other`) and are dropped here, at load time: a downgraded build
    /// must neither fail to parse the whole settings.json nor write back an
    /// action it cannot act on.
    #[serde(deserialize_with = "deserialize_known_actions")]
    pub actions: Vec<DutAction>,
}

fn deserialize_known_actions<'de, D>(deserializer: D) -> Result<Vec<DutAction>, D::Error>
where
    D: Deserializer<'de>,
{
    let actions = Vec::<DutAction>::deserialize(deserializer)?;
    Ok(actions
        .into_iter()
        .filter(|action| *action != DutAction::Unknown)
        .collect())
}

/// DUT control action classification (see scheme §4.2 table for details; read does not require a
/// lease, while other actions do).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DutAction {
    Read,
    SerialWrite,
    Adb,
    Relay,
    Power,
    Reset,
    Flash,
    /// Snapshot / recording control on the DUT's capture sources
    /// (`media_streams` with an `op`). Granted explicitly; never implied by
    /// `Read`, so read-only tokens keep working for queries only.
    Capture,
    /// Sink for actions written by a newer build (serde `other`). It never
    /// matches a grant (deny by default) and is stripped from
    /// `grants[].actions` while settings load, so it is never written back.
    #[serde(other)]
    Unknown,
}

impl DutAction {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DutAction::Read => "read",
            DutAction::SerialWrite => "serialWrite",
            DutAction::Adb => "adb",
            DutAction::Relay => "relay",
            DutAction::Power => "power",
            DutAction::Reset => "reset",
            DutAction::Flash => "flash",
            DutAction::Capture => "capture",
            DutAction::Unknown => "unknown",
        }
    }
}

/// Agent credentials (SPEC §5.1): Token plaintext storage is required by users; this information is
/// clearly visible. The risk level is indicated on the UI.
///
/// Handwriting the `Debug` function and encoding the token: Secrets are often mentioned in
/// configuration/error-related contexts. By default, the `derive` function will include the
/// plaintext token in the logs (F25).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCredential {
    /// Stable identifiers (`cred-<slug>`)
    pub id: String,
    /// Legacy credentials are scoped to the primary MCP listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener_id: Option<String>,
    /// Display name (SPEC §5.1.3: cannot be used as authentication credentials)
    pub agent_name: String,
    /// Bearer token (`dth-<43 chars>`, >=256-bit entropy). Persisted in the OS
    /// keyring (`duthub/mcp/<id>`) by desktop startup/save paths rather than
    /// written in plaintext to `settings.json`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// An empty grant list authorizes no DUT.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<DutGrant>,
}

impl std::fmt::Debug for AgentCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentCredential")
            .field("id", &self.id)
            .field("listener_id", &self.listener_id)
            .field("agent_name", &self.agent_name)
            .field("token", &"[REDACTED]")
            .field("enabled", &self.enabled)
            .field("grants", &self.grants)
            .finish()
    }
}

/// Listener during runtime (per session): No settings.json file is written to; it exists only
/// within the overlay of [`AgentAccessRuntime`]. Its lifecycle is tied to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeListener {
    pub listener: AgentListener,
    pub credential: AgentCredential,
}

/// The authenticated entity (the identity confirmed by the server; clients cannot report their own
/// roles. SPEC §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// HTTP requests when GUI/stdio/auth is disabled
    LocalAdmin,
    Agent {
        agent_name: String,
        credential_id: String,
    },
}

impl Principal {
    /// The agent identifier used for auditing purposes (The GUI returns `None` for this field—local
    /// users are not included in the agent list).
    #[must_use]
    pub fn audit_identity(&self) -> Option<(&str, &str)> {
        match self {
            Principal::LocalAdmin => None,
            Principal::Agent {
                agent_name,
                credential_id,
            } => Some((agent_name.as_str(), credential_id.as_str())),
        }
    }
}

/// Authentication/Authorization Failure Category (HTTP layer unified response: 401
/// `{"error":"unauthorized"}`, ensuring that the presence of the "token" or which agent it belongs
/// to is not disclosed. SPEC §5.1.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No token present / Invalid token / Credentials disabled (Always returns 401, regardless of
    /// the reason)
    Unauthorized,
    /// The DUT has been authenticated, but no actions have been performed on it (denied by
    /// default).
    NotGranted { dut_id: String, action: String },
    /// Invalid parameters were provided during the listener registration process during execution
    /// (internal call error; this variant does not occur at the HTTP layer).
    InvalidListener(String),
}

impl AuthError {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            AuthError::Unauthorized => "未认证：缺少或无效的 Bearer Token".into(),
            AuthError::NotGranted { dut_id, action } => {
                format!("未授权：凭据未被授予 {dut_id} 的 {action} 动作")
            }
            AuthError::InvalidListener(reason) => {
                format!("运行时监听器注册被拒绝：{reason}")
            }
        }
    }

    fn invalid_listener(reason: &str) -> Self {
        AuthError::InvalidListener(reason.to_string())
    }
}

/// Token → SHA-256 fingerprint (runtime index key; plaintext is not used for any indexing/logging
/// purposes).
#[must_use]
pub fn token_fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    crate::util::hex_lower(&digest)
}

/// Generate an Agent bearer token: 32 CSPRNG bytes -> base64url (43 chars) -> `dth-<43>`
/// (>=256 bits of entropy, SPEC §5.1.4). Shared by the GUI generate button and tests.
///
/// # Panics
///
/// Panics if the OS cryptographic RNG (`getrandom`) fails.
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG is available");
    format!("dth-{}", B64.encode(bytes))
}

/// Immutable runtime snapshot indexed by fingerprint (can be hot-replaced in its entirety after GUI
/// saving).
struct AuthSnapshot {
    settings: AgentAccessSettings,
    by_fingerprint: HashMap<String, Option<Arc<AgentCredential>>>,
}

impl AuthSnapshot {
    fn build(settings: AgentAccessSettings) -> Self {
        let mut by_fingerprint = HashMap::new();
        for cred in &settings.agents {
            by_fingerprint
                .entry(token_fingerprint(&cred.token))
                .and_modify(|candidate| *candidate = None)
                .or_insert_with(|| Some(Arc::new(cred.clone())));
        }
        Self {
            settings,
            by_fingerprint,
        }
    }
}

/// Runtime overlay: A listener per session, along with its credentials (a second table, separate
/// from the persistent settings).
///
/// Separated from the persistent table strictly:
/// – `authenticate_http` only accesses the persistent table (temporary tokens never allow access to
/// the main listener);
/// – `authenticate_internal` only accesses this table itself;
/// – `authorize`, `grants_for`, and `lease_credential_enabled` combine data from both tables;
/// – `update_settings()` reconstructs the persistent table while retaining information from this
/// table;
/// – `settings()` and `enabled()` only consider data in the persistent table (session tokens are
/// never stored in settings.json).
#[derive(Default)]
struct RuntimeOverlay {
    by_fingerprint: HashMap<String, Option<Arc<AgentCredential>>>,
    listeners: HashMap<String, Arc<RuntimeListener>>,
}

impl RuntimeOverlay {
    /// Reconstructed the token fingerprint index completely (old token indexes are released after
    /// removal).
    fn rebuild_fingerprints(&mut self) {
        self.by_fingerprint.clear();
        for listener in self.listeners.values() {
            self.by_fingerprint
                .entry(token_fingerprint(&listener.credential.token))
                .and_modify(|candidate| *candidate = None)
                .or_insert_with(|| Some(Arc::new(listener.credential.clone())));
        }
    }
}

/// Agent connects during runtime (desktop construction, Arc is shared with MCP middleware/lease
/// manager/GUI).
pub struct AgentAccessRuntime {
    snapshot: RwLock<Arc<AuthSnapshot>>,
    runtime: RwLock<RuntimeOverlay>,
}

impl std::fmt::Debug for AgentAccessRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = crate::util::read_unpoisoned(&self.snapshot);
        let runtime = crate::util::read_unpoisoned(&self.runtime);
        f.debug_struct("AgentAccessRuntime")
            .field("enabled", &snapshot.settings.enabled)
            .field("http_port", &snapshot.settings.http_port)
            .field("persistent_listeners", &snapshot.settings.listeners.len())
            .field("runtime_listeners", &runtime.listeners.len())
            .finish_non_exhaustive()
    }
}

impl AgentAccessRuntime {
    #[must_use]
    pub fn from_settings(settings: AgentAccessSettings) -> Arc<Self> {
        Arc::new(Self {
            snapshot: RwLock::new(Arc::new(AuthSnapshot::build(settings))),
            runtime: RwLock::new(RuntimeOverlay::default()),
        })
    }

    /// After saving credentials via GUI or adjusting settings, the persistent tables can be
    /// replaced dynamically. **The runtime overlay remains intact.**
    pub fn update(&self, settings: AgentAccessSettings) {
        let mut guard = crate::util::write_unpoisoned(&self.snapshot);
        *guard = Arc::new(AuthSnapshot::build(settings));
    }

    /// Current persistent configuration snapshot (port/TTL/credentials list; does not include
    /// runtime overlay).
    pub fn settings(&self) -> AgentAccessSettings {
        crate::util::read_unpoisoned(&self.snapshot)
            .settings
            .clone()
    }

    /// From a GUI perspective, the status of connectivity can be determined by checking whether the
    /// auth switch or any **persistent** listeners are enabled. This is not an HTTP-based
    /// authentication mechanism, and it does not rely on the runtime overlay (session listeners
    /// remain enabled regardless of runtime conditions).
    pub fn enabled(&self) -> bool {
        let snapshot = crate::util::read_unpoisoned(&self.snapshot);
        snapshot.settings.enabled || !snapshot.settings.listeners.is_empty()
    }

    pub fn http_port(&self) -> u16 {
        crate::util::read_unpoisoned(&self.snapshot)
            .settings
            .http_port
    }

    pub fn lease_ttl_secs(&self) -> u32 {
        crate::util::read_unpoisoned(&self.snapshot)
            .settings
            .lease_ttl_secs
    }

    /// Registers a listener for each session (overlay mode; does not write to disk).
    ///
    /// Rejection criteria (4 items):
    /// 1. `listener.id` is empty (or contains only whitespace).
    /// 2. Credential `id` or `token` is missing.
    /// 3. `credential.listener_id != Some(listener.id)` – invalid scope match.
    /// 4. `listener.id` conflicts with a persistent listener or an already-registered runtime
    ///    listener.
    ///
    /// Otherwise, a temporary credential could redirect the main listener onto the internal
    /// authentication path.
    pub fn set_runtime_listener(
        &self,
        listener: AgentListener,
        credential: AgentCredential,
    ) -> Result<(), AuthError> {
        if listener.id.trim().is_empty() {
            return Err(AuthError::invalid_listener("监听器 id 不能为空"));
        }
        if credential.id.trim().is_empty() || credential.token.trim().is_empty() {
            return Err(AuthError::invalid_listener("凭据 id 与 token 不能为空"));
        }
        if credential.listener_id.as_deref() != Some(listener.id.as_str()) {
            return Err(AuthError::invalid_listener(
                "凭据 listenerId 必须与监听器 id 一致",
            ));
        }
        if self
            .snapshot
            .read()
            .unwrap()
            .settings
            .listeners
            .iter()
            .any(|known| known.id == listener.id)
        {
            return Err(AuthError::invalid_listener("监听器 id 与持久监听器冲突"));
        }
        let mut runtime = crate::util::write_unpoisoned(&self.runtime);
        if runtime.listeners.contains_key(&listener.id) {
            return Err(AuthError::invalid_listener("监听器 id 已注册"));
        }
        runtime.listeners.insert(
            listener.id.clone(),
            Arc::new(RuntimeListener {
                listener,
                credential,
            }),
        );
        runtime.rebuild_fingerprints();
        Ok(())
    }

    /// Register a per-session listener with its read-only, single-DUT credential.
    ///
    /// This is the enforced session contract on top of
    /// [`Self::set_runtime_listener`]: the listener must be bound to a DUT and
    /// the credential must carry exactly one grant for that DUT with `Read`
    /// only. Wider grants are rejected so a temporary listener can never gain
    /// write access or cross-DUT reads.
    pub fn set_session_listener(
        &self,
        mut listener: AgentListener,
        credential: AgentCredential,
    ) -> Result<(), AuthError> {
        let Some(dut_id) = listener
            .dut_id
            .as_deref()
            .map(str::trim)
            .filter(|dut_id| !dut_id.is_empty())
        else {
            return Err(AuthError::invalid_listener("会话监听器必须绑定 DUT"));
        };
        let read_only = credential.grants.len() == 1
            && credential.grants[0].dut_id == dut_id
            && credential.grants[0].actions.len() == 1
            && credential.grants[0].actions[0] == DutAction::Read;
        if !read_only {
            return Err(AuthError::invalid_listener(
                "会话凭据必须且只能授予绑定 DUT 的 read",
            ));
        }
        // Store the trimmed DUT id so later scoping comparisons match the grant.
        listener.dut_id = Some(dut_id.to_string());
        self.set_runtime_listener(listener, credential)
    }

    /// Revoking the listener for each session (idempotent): After deletion, this token will no
    /// longer be valid for internal authentication purposes.
    pub fn remove_runtime_listener(&self, listener_id: &str) -> Result<(), AuthError> {
        let mut runtime = crate::util::write_unpoisoned(&self.runtime);
        runtime.listeners.remove(listener_id);
        runtime.rebuild_fingerprints();
        Ok(())
    }

    /// Whether the ID is registered by the runtime overlay mechanism; `AuthSvc` uses this ID to
    /// determine which internal authentication method should be used. If there is a persistent
    /// listener with the same ID, false will be returned: temporary tokens cannot be used to switch
    /// the main listener to internal authentication mode.
    pub fn is_runtime_listener(&self, listener_id: &str) -> bool {
        if listener_id.is_empty() {
            return false;
        }
        let registered = self
            .runtime
            .read()
            .unwrap()
            .listeners
            .contains_key(listener_id);
        registered
            && !self
                .snapshot
                .read()
                .unwrap()
                .settings
                .listeners
                .iter()
                .any(|listener| listener.id == listener_id)
    }

    /// Session listener authentication: **Only the runtime overlay is checked** (Neither the
    /// persistent tables nor the overall authentication settings are considered).
    /// Empty listener ID / Empty token / Not registered / Token across listeners / Credentials
    /// disabled → Unauthorized.
    pub fn authenticate_internal(
        &self,
        listener_id: &str,
        bearer: &str,
    ) -> Result<Principal, AuthError> {
        let listener_id = listener_id.trim();
        let bearer = bearer.trim();
        if listener_id.is_empty() || bearer.is_empty() {
            return Err(AuthError::Unauthorized);
        }
        let runtime = crate::util::read_unpoisoned(&self.runtime);
        let Some(listener) = runtime.listeners.get(listener_id) else {
            return Err(AuthError::Unauthorized);
        };
        let credential = runtime
            .by_fingerprint
            .get(&token_fingerprint(bearer))
            .and_then(Option::as_ref)
            .filter(|credential| credential.enabled)
            .filter(|credential| credential.id == listener.credential.id)
            .filter(|credential| credential.listener_id.as_deref() == Some(listener_id))
            .filter(|credential| bool::from(bearer.as_bytes().ct_eq(credential.token.as_bytes())))
            .ok_or(AuthError::Unauthorized)?;
        Ok(Principal::Agent {
            agent_name: credential.agent_name.clone(),
            credential_id: credential.id.clone(),
        })
    }

    /// Merge persistent and runtime grants for the same credential by DUT, taking the union of
    /// allowed actions. An unknown or disabled credential returns `None`.
    pub fn grants_for(&self, credential_id: &str) -> Option<Vec<DutGrant>> {
        let mut found = false;
        let mut merged: Vec<DutGrant> = Vec::new();
        {
            let snapshot = crate::util::read_unpoisoned(&self.snapshot);
            for credential in &snapshot.settings.agents {
                if credential.id != credential_id {
                    continue;
                }
                found = true;
                if !credential.enabled {
                    return None;
                }
                merge_grants(&mut merged, &credential.grants);
            }
        }
        {
            let runtime = crate::util::read_unpoisoned(&self.runtime);
            for listener in runtime.listeners.values() {
                let credential = &listener.credential;
                if credential.id != credential_id {
                    continue;
                }
                found = true;
                if !credential.enabled {
                    return None;
                }
                merge_grants(&mut merged, &credential.grants);
            }
        }
        found.then_some(merged)
    }

    /// Whether any credential with this id (persistent or session overlay) is
    /// still enabled. Unknown ids and disabled matches report `false`.
    ///
    /// Used by the audit inspector to hide rows for revoked credentials while
    /// keeping zero-grant but enabled credentials visible.
    pub fn credential_enabled(&self, id: &str) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        let mut found = false;
        {
            let snapshot = crate::util::read_unpoisoned(&self.snapshot);
            for credential in &snapshot.settings.agents {
                if credential.id != id {
                    continue;
                }
                found = true;
                if !credential.enabled {
                    return false;
                }
            }
        }
        {
            let runtime = crate::util::read_unpoisoned(&self.runtime);
            for listener in runtime.listeners.values() {
                let credential = &listener.credential;
                if credential.id != id {
                    continue;
                }
                found = true;
                if !credential.enabled {
                    return false;
                }
            }
        }
        found
    }

    /// Whether the credentials of the lease holder are still enabled (combined from persistent
    /// table and runtime overlay); for use by [`crate::lease::ControlLeaseManager`].
    pub fn lease_credential_enabled(&self, id: &str) -> bool {
        if id == "local-admin" {
            let snapshot = crate::util::read_unpoisoned(&self.snapshot);
            let settings = &snapshot.settings;
            return !settings.enabled && settings.listeners.is_empty();
        }
        self.grants_for(id).is_some()
    }

    /// Authentication (SPEC §5.1):
    ///
    /// - `enabled=false` -> always use `LocalAdmin` (compatible with legacy behavior).
    /// - `enabled=true` -> require Bearer token -> SHA-256 fingerprint index for O(1) candidate
    ///   lookup -> constant-time comparison via `subtle` (timing does not leak token bytes) ->
    ///   `Principal`. Checks persistent tables only.
    pub fn authenticate(&self, bearer: Option<&str>) -> Result<Principal, AuthError> {
        self.authenticate_on(None, bearer)
    }

    pub fn authenticate_on(
        &self,
        listener_id: Option<&str>,
        bearer: Option<&str>,
    ) -> Result<Principal, AuthError> {
        self.authenticate_with_policy(listener_id, bearer, false)
    }

    /// Require an enabled HTTP endpoint and a listener-scoped credential.
    /// **Only check persistent tables**: The runtime overlay token cannot be used to enable
    /// persistent listeners.
    pub fn authenticate_http(
        &self,
        listener_id: Option<&str>,
        bearer: Option<&str>,
    ) -> Result<Principal, AuthError> {
        self.authenticate_with_policy(listener_id, bearer, true)
    }

    fn authenticate_with_policy(
        &self,
        listener_id: Option<&str>,
        bearer: Option<&str>,
        require_enabled: bool,
    ) -> Result<Principal, AuthError> {
        let snapshot = crate::util::read_unpoisoned(&self.snapshot).clone();
        if require_enabled && !snapshot.settings.enabled {
            return Err(AuthError::Unauthorized);
        }
        if let Some(id) = listener_id {
            if !snapshot
                .settings
                .listeners
                .iter()
                .any(|listener| listener.id == id)
            {
                return Err(AuthError::Unauthorized);
            }
        } else if !snapshot.settings.enabled && snapshot.settings.listeners.is_empty() {
            return Ok(Principal::LocalAdmin);
        }
        let Some(token) = bearer.map(str::trim).filter(|t| !t.is_empty()) else {
            return Err(AuthError::Unauthorized);
        };
        let cred = snapshot
            .by_fingerprint
            .get(&token_fingerprint(token))
            .and_then(Option::as_ref)
            .filter(|cred| bool::from(token.as_bytes().ct_eq(cred.token.as_bytes())))
            .filter(|cred| cred.enabled && cred.listener_id.as_deref() == listener_id)
            .ok_or(AuthError::Unauthorized)?;
        Ok(Principal::Agent {
            agent_name: cred.agent_name.clone(),
            credential_id: cred.id.clone(),
        })
    }

    /// Authorize a principal for `(dut_id, action)` without allocating/cloning
    /// grant vectors on the hot path.
    pub fn authorize(
        &self,
        principal: &Principal,
        dut_id: &str,
        action: DutAction,
    ) -> Result<(), AuthError> {
        let Principal::Agent {
            agent_name: _,
            credential_id,
        } = principal
        else {
            return Ok(());
        };
        let mut found = false;
        let mut allowed = false;
        {
            let snapshot = crate::util::read_unpoisoned(&self.snapshot);
            for cred in &snapshot.settings.agents {
                if cred.id != *credential_id {
                    continue;
                }
                found = true;
                if !cred.enabled {
                    return Err(AuthError::NotGranted {
                        dut_id: dut_id.to_string(),
                        action: action.as_str().to_string(),
                    });
                }
                if cred
                    .grants
                    .iter()
                    .any(|g| g.dut_id == dut_id && g.actions.contains(&action))
                {
                    allowed = true;
                }
            }
        }
        {
            let runtime = crate::util::read_unpoisoned(&self.runtime);
            for listener in runtime.listeners.values() {
                let cred = &listener.credential;
                if cred.id != *credential_id {
                    continue;
                }
                found = true;
                if !cred.enabled {
                    return Err(AuthError::NotGranted {
                        dut_id: dut_id.to_string(),
                        action: action.as_str().to_string(),
                    });
                }
                if cred
                    .grants
                    .iter()
                    .any(|g| g.dut_id == dut_id && g.actions.contains(&action))
                {
                    allowed = true;
                }
            }
        }
        if found && allowed {
            Ok(())
        } else {
            Err(AuthError::NotGranted {
                dut_id: dut_id.to_string(),
                action: action.as_str().to_string(),
            })
        }
    }
}

/// Merge the `incoming` grants into `merged`: Combine according to the actions of the DUT, with the
/// order being based on when they first appear.
fn merge_grants(merged: &mut Vec<DutGrant>, incoming: &[DutGrant]) {
    for grant in incoming {
        match merged.iter_mut().find(|known| known.dut_id == grant.dut_id) {
            Some(known) => {
                for action in &grant.actions {
                    if !known.actions.contains(action) {
                        known.actions.push(*action);
                    }
                }
            }
            None => merged.push(grant.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(id: &str, token: &str, enabled: bool, grants: Vec<DutGrant>) -> AgentCredential {
        AgentCredential {
            listener_id: None,
            id: id.into(),
            agent_name: format!("agent-{id}"),
            token: token.into(),
            enabled,
            grants,
        }
    }

    #[test]
    fn auth_disabled_treats_everything_as_local_admin() {
        let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
        assert!(!rt.enabled());
        assert_eq!(
            rt.authenticate(None).unwrap(),
            Principal::LocalAdmin,
            "Auth Off: No Bearer is required for authorization (compatible with current status)."
        );
        assert_eq!(
            rt.authenticate(Some("garbage")).unwrap(),
            Principal::LocalAdmin
        );
    }

    #[test]
    fn authenticate_enabled_requires_valid_bearer_and_is_ct_safe() {
        let token = generate_token();
        let rt = AgentAccessRuntime::from_settings(AgentAccessSettings {
            enabled: true,
            agents: vec![cred("cred-a", &token, true, vec![])],
            ..Default::default()
        });
        // None / Empty / Incorrect token → Always Unauthorized (No distinction made regarding the
        // reason)
        assert_eq!(rt.authenticate(None), Err(AuthError::Unauthorized));
        assert_eq!(rt.authenticate(Some("  ")), Err(AuthError::Unauthorized));
        assert_eq!(
            rt.authenticate(Some("dth-wrong")),
            Err(AuthError::Unauthorized)
        );
        // Correct token → Agent entity
        let p = rt.authenticate(Some(&token)).unwrap();
        let (name, id) = p.audit_identity().unwrap();
        assert_eq!(name, "agent-cred-a");
        assert_eq!(id, "cred-a");
        // Disabling credentials: The fingerprint still matches, but denial is necessary.
        rt.update(AgentAccessSettings {
            enabled: true,
            agents: vec![cred("cred-a", &token, false, vec![])],
            ..Default::default()
        });
        assert_eq!(rt.authenticate(Some(&token)), Err(AuthError::Unauthorized));
    }

    #[test]
    fn authorize_is_deny_by_default_per_dut_and_action() {
        let token = generate_token();
        let rt = AgentAccessRuntime::from_settings(AgentAccessSettings {
            enabled: true,
            agents: vec![cred(
                "cred-a",
                &token,
                true,
                vec![DutGrant {
                    dut_id: "dut-1".into(),
                    actions: vec![DutAction::Read, DutAction::SerialWrite],
                }],
            )],
            ..Default::default()
        });
        let p = rt.authenticate(Some(&token)).unwrap();
        // granted: read + serial_write succeeded
        rt.authorize(&p, "dut-1", DutAction::Read).unwrap();
        rt.authorize(&p, "dut-1", DutAction::SerialWrite).unwrap();
        // Action not granted / DUT not granted → NotGranted
        assert!(matches!(
            rt.authorize(&p, "dut-1", DutAction::Power),
            Err(AuthError::NotGranted { .. })
        ));
        assert!(matches!(
            rt.authorize(&p, "dut-2", DutAction::Read),
            Err(AuthError::NotGranted { .. })
        ));
        // An empty grant list authorizes no DUT.
        let token_b = generate_token();
        rt.update(AgentAccessSettings {
            enabled: true,
            agents: vec![cred("cred-b", &token_b, true, vec![])],
            ..Default::default()
        });
        let p_b = rt.authenticate(Some(&token_b)).unwrap();
        assert!(rt.authorize(&p_b, "dut-1", DutAction::Read).is_err());
        // LocalAdmin operates without any restrictions.
        rt.authorize(&Principal::LocalAdmin, "dut-9", DutAction::Flash)
            .unwrap();
    }

    #[test]
    fn generated_token_shape_and_fingerprint_stability() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert!(t1.starts_with("dth-"), "{t1}");
        assert_eq!(
            t1.len(),
            4 + 43,
            "32 bytes for Base64URL without padding = 43 characters"
        );
        assert_ne!(t1, t2, "The two generations must be different.");
        assert_eq!(token_fingerprint(&t1), token_fingerprint(&t1));
        assert_ne!(token_fingerprint(&t1), token_fingerprint(&t2));
    }

    #[test]
    fn settings_roundtrips_camel_case_and_defaults_compat() {
        // Bug #15: The keys for round-trip operations must be written in camelCase format.
        let value = serde_json::json!({
            "enabled": true,
            "httpPort": 8800,
            "leaseTtlSecs": 60,
            "agents": [{
                "id": "cred-claude",
                "agentName": "Claude",
                "token": "dth-abcdef",
                "enabled": false,
                "grants": [{"dutId": "dut-1", "actions": ["read", "serialWrite"]}]
            }]
        });
        let parsed: AgentAccessSettings = serde_json::from_value(value).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.http_port, 8800);
        assert_eq!(parsed.lease_ttl_secs, 60);
        assert_eq!(parsed.agents.len(), 1);
        assert!(!parsed.agents[0].enabled);
        assert_eq!(
            parsed.agents[0].grants[0].actions,
            vec![DutAction::Read, DutAction::SerialWrite]
        );
        // Old files lack the agentAccess segment: The settingsfile-level default value is used as a
        // fallback (for integration testing, see tests/)
        // Default serialization structure: If agents is empty, the keys will be omitted
        let back = serde_json::to_value(AgentAccessSettings::default()).unwrap();
        assert!(back.get("agents").is_none());
        assert_eq!(back["enabled"], serde_json::json!(false));
    }

    #[test]
    fn lan_access_defaults_off_and_roundtrips_camel_case() {
        // The old configuration lacked the "lanAccess" parameter. It is necessary to disable this
        // parameter by default (compatible with issues #19 and #24).
        let legacy: AgentAccessSettings = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "httpPort": 8800
        }))
        .unwrap();
        assert!(
            !legacy.lan_access,
            "When the old configuration lacks lanAccess, it should be disabled by default."
        );

        let back = serde_json::to_value(AgentAccessSettings::default()).unwrap();
        assert!(
            back.get("lanAccess").is_none(),
            "Default value is false. It should not be serialized: {back}."
        );

        let enabled = AgentAccessSettings {
            enabled: true,
            lan_access: true,
            ..Default::default()
        };
        let value = serde_json::to_value(&enabled).unwrap();
        assert_eq!(value["lanAccess"], serde_json::json!(true), "{value}");
        let parsed: AgentAccessSettings = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, enabled);
    }

    #[test]
    fn listener_dut_id_is_optional_and_backward_compatible() {
        let legacy: AgentListener =
            serde_json::from_value(serde_json::json!({"id": "lab", "port": 8798})).unwrap();
        assert_eq!(
            legacy.dut_id, None,
            "The old listeners[] does not contain a dutId; it must be a universal listener."
        );

        let bound = AgentListener {
            id: "dut-a-mcp".into(),
            port: 8799,
            dut_id: Some("dut-a".into()),
        };
        let value = serde_json::to_value(&bound).unwrap();
        assert_eq!(value["dutId"], "dut-a");
        let parsed: AgentListener = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, bound);
    }
}
