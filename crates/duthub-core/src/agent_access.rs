//! Agent 接入凭据 / 授权模型与认证运行时（`docs/SECURITY.md`）。
//!
//! 四层语义的第一、二层（SPEC §14）：**认证（authenticate）= 谁能来；
//! 授权（authorize）= 谁能做**。租约（lease.rs）= 此刻谁能控制；
//! 写入通道复核（duthub-device）= 谁最终执行硬件动作。
//!
//! - auth 总开关默认关（`AgentAccessSettings::default().enabled == false`）：
//!   **HTTP 传输在关闭时一律 401（fail closed）**；stdio/进程内 GUI 仍按
//!   [`Principal::LocalAdmin`] 处理。开启后 deny by default——无有效凭据一律拒绝，
//!   未授予的动作一律拒绝。注意 [`AgentAccessRuntime::enabled`] 只要存在监听器就
//!   返回 true（用于 GUI 语义），与 HTTP 门使用的 `settings.enabled` 不是同一判定。
//! - 凭据与 Grant 持久化在 settings.json `agentAccess` 段（偏差 1；
//!   默认值整段跳过序列化，`docs/examples/settings.json` 往返测试不动一行仍绿）。
//! - token 明文存储是**用户明确要求**（界面始终可见并标注风险）；
//!   运行时索引只用 SHA-256 指纹，比较用 `subtle` 常量时间等价，
//!   审计/日志绝不落 token（audit.rs 脱敏）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// MCP HTTP 监听端口默认值（偏差 8：可配置，重启生效；默认只绑 loopback）。
fn default_http_port() -> u16 {
    8797
}

/// 租约默认 TTL 秒（SPEC §6.1）。
fn default_lease_ttl() -> u32 {
    120
}

fn default_true() -> bool {
    true
}

/// settings.json `agentAccess` 段：凭据列表 + auth 开关 + 端口 + TTL。
///
/// 坑 #19/#24：默认值整段序列化跳过（`agent_access_is_default`），
/// 旧 settings.json 无此段 → default（auth 关、无凭据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccessSettings {
    /// auth 总开关：false（默认）= HTTP 一律 401（fail closed；stdio/GUI 仍为
    /// LocalAdmin）；true = deny by default，写工具必须授权 + 租约。
    #[serde(default)]
    pub enabled: bool,
    /// 局域网访问（默认 false，只绑 127.0.0.1）。开启后 MCP 监听 `0.0.0.0`，
    /// 回环与局域网地址同时可达；必须同时开启认证（运行时 fail closed）。
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
    /// `Some(dut_id)` = DUT 专用端点（只读该 DUT，连接管理维护）；
    /// `None` = 通用监听器（Agent 接入页/LLM 使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dut_id: Option<String>,
}

impl AgentAccessSettings {
    /// 按 DUT 查专用监听器（每台 DUT 至多一个）。
    pub fn listener_for_dut(&self, dut_id: &str) -> Option<&AgentListener> {
        self.listeners
            .iter()
            .find(|listener| listener.dut_id.as_deref() == Some(dut_id))
    }

    /// 按监听器查绑定凭据（DUT 专用端点每监听器一个）。
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

/// 默认值判定（model.rs `skip_serializing_if` 用）：默认值段整段不序列化。
pub fn agent_access_is_default(settings: &AgentAccessSettings) -> bool {
    settings == &AgentAccessSettings::default()
}

/// 每 DUT 动作授权（SPEC §5.2 deny by default；空 grants = 未授权任何 DUT）。
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

/// DUT 控制动作分类（覆盖面见方案 §4.2 表；read 不需要租约，其余写类需要）。
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

/// Agent 凭据（SPEC §5.1）：token 明文存储（用户明确要求原文可见；风险由 UI 标注）。
///
/// `Debug` 手写并给 token 打码：凭据常出现在配置/错误上下文里，默认 derive 会把
/// 明文 token 带进日志（F25）。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCredential {
    /// 稳定标识（`cred-<slug>`）
    pub id: String,
    /// Legacy credentials are scoped to the primary MCP listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener_id: Option<String>,
    /// 展示名（SPEC §5.1.3：不能作为认证凭据）
    pub agent_name: String,
    /// Bearer token (`dth-<43 chars>`, >=256-bit entropy). Persisted in the OS
    /// keyring (`duthub/mcp/<id>`) by desktop startup/save paths rather than
    /// written in plaintext to `settings.json`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 空 = 未授权任何 DUT（deny by default）
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

/// 运行时（每会话）监听器：不写 settings.json，只存在于 [`AgentAccessRuntime`]
/// 的 overlay 中；生命周期与会话绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeListener {
    pub listener: AgentListener,
    pub credential: AgentCredential,
}

/// 认证后的主体（服务端确认的身份，不接受客户端自报角色，SPEC §7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// GUI / stdio / auth 关闭时的 HTTP 请求
    LocalAdmin,
    Agent {
        agent_name: String,
        credential_id: String,
    },
}

impl Principal {
    /// 审计用的 agent 标识（GUI 主体返回 None——本地人不进 agent 字段）。
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

/// 认证 / 授权失败类别（HTTP 层统一 401 `{"error":"unauthorized"}`，
/// 不泄露「token 是否存在/属于哪个 agent」，SPEC §5.1.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 未携带 token / token 无效 / 凭据停用（对外一律 401，不区分原因）
    Unauthorized,
    /// 已认证但未授予该 DUT 动作（deny by default）
    NotGranted { dut_id: String, action: String },
    /// 运行时监听器注册参数非法（内部调用错误；HTTP 层不会产生此变体）
    InvalidListener(String),
}

impl AuthError {
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

/// token → SHA-256 指纹（运行时索引键；明文不进任何索引/日志）。
pub fn token_fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 生成 Agent token：32 字节 CSPRNG → base64url（43 字符）→ `dth-<43>`，
/// ≥256 bit 熵（SPEC §5.1.4）。GUI「生成」按钮与测试共用。
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("CSPRNG 不可用");
    format!("dth-{}", B64.encode(bytes))
}

/// 按指纹索引的不可变运行时快照（GUI 保存后整体热替换）。
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

/// 运行时 overlay：每会话监听器 + 其凭据（持久设置之外的第二张表）。
///
/// 与持久表严格分离：
/// - `authenticate_http` 只查持久表（临时 token 永远打不开主监听器）；
/// - `authenticate_internal` 只查本表；
/// - `authorize` / `grants_for` / `lease_credential_enabled` 合并两表；
/// - `update(settings)` 重建持久表但保留本表；
/// - `settings()` / `enabled()` 只看持久表（会话 token 永不落 settings.json）。
#[derive(Default)]
struct RuntimeOverlay {
    by_fingerprint: HashMap<String, Option<Arc<AgentCredential>>>,
    listeners: HashMap<String, Arc<RuntimeListener>>,
}

impl RuntimeOverlay {
    /// token 指纹索引整体重建（remove 后释放旧 token 索引）。
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

/// Agent 接入运行时（desktop 构造，Arc 共享给 MCP 中间件 / 租约管理器 / GUI）。
pub struct AgentAccessRuntime {
    snapshot: RwLock<Arc<AuthSnapshot>>,
    runtime: RwLock<RuntimeOverlay>,
}

impl AgentAccessRuntime {
    pub fn from_settings(settings: AgentAccessSettings) -> Arc<Self> {
        Arc::new(Self {
            snapshot: RwLock::new(Arc::new(AuthSnapshot::build(settings))),
            runtime: RwLock::new(RuntimeOverlay::default()),
        })
    }

    /// GUI 保存凭据 / 开关后整体热替换持久表；**保留** runtime overlay。
    pub fn update(&self, settings: AgentAccessSettings) {
        let mut guard = self.snapshot.write().unwrap();
        *guard = Arc::new(AuthSnapshot::build(settings));
    }

    /// 当前持久配置快照（端口 / TTL / 凭据列表；不含 runtime overlay）。
    pub fn settings(&self) -> AgentAccessSettings {
        self.snapshot.read().unwrap().settings.clone()
    }

    /// GUI 视角的「接入是否生效」：auth 开关或任意**持久**监听器存在。
    /// **不是** HTTP 认证门，也不看 runtime overlay（会话 listener 不改变启用态）。
    pub fn enabled(&self) -> bool {
        let snapshot = self.snapshot.read().unwrap();
        snapshot.settings.enabled || !snapshot.settings.listeners.is_empty()
    }

    pub fn http_port(&self) -> u16 {
        self.snapshot.read().unwrap().settings.http_port
    }

    pub fn lease_ttl_secs(&self) -> u32 {
        self.snapshot.read().unwrap().settings.lease_ttl_secs
    }

    /// 注册每会话监听器（overlay；不写盘）。
    ///
    /// 拒绝规则（4 条）：
    /// 1. `listener.id` 为空（含全空白）；
    /// 2. 凭据 `id` 或 `token` 为空；
    /// 3. `credential.listener_id != Some(listener.id)`（作用域不匹配）；
    /// 4. `listener.id` 与持久监听器或已注册 runtime 监听器冲突——
    ///    否则临时凭据可把主监听器切进 internal 认证路径。
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
        let mut runtime = self.runtime.write().unwrap();
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

    /// 吊销每会话监听器（幂等）：删除后该 token 立即无法通过 internal 认证。
    pub fn remove_runtime_listener(&self, listener_id: &str) -> Result<(), AuthError> {
        let mut runtime = self.runtime.write().unwrap();
        runtime.listeners.remove(listener_id);
        runtime.rebuild_fingerprints();
        Ok(())
    }

    /// 该 id 是否由 runtime overlay 注册（`AuthSvc` 据此选择 internal 认证入口）。
    /// 持久监听器同 id 时返回 false：临时 token 不得把主监听器切进 internal 认证。
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

    /// 会话监听器认证：**只查 runtime overlay**（不查持久表、不看 auth 总开关）。
    /// 空 listener id / 空 token / 未注册 / 跨监听器 token / 停用凭据 → Unauthorized。
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
        let runtime = self.runtime.read().unwrap();
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

    /// 合并授权视图（持久表 + runtime overlay）：同一凭据 id 的 grants 按 DUT 合并
    /// （动作取并集）。任一命中凭据停用 / 完全未知 → `None`（fail closed）。
    pub fn grants_for(&self, credential_id: &str) -> Option<Vec<DutGrant>> {
        let mut found = false;
        let mut merged: Vec<DutGrant> = Vec::new();
        {
            let snapshot = self.snapshot.read().unwrap();
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
            let runtime = self.runtime.read().unwrap();
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
            let snapshot = self.snapshot.read().unwrap();
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
            let runtime = self.runtime.read().unwrap();
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

    /// 租约持有者的凭据是否仍启用（持久表 + runtime overlay 合并）；供
    /// [`crate::lease::ControlLeaseManager`] 的归属复核使用。
    pub fn lease_credential_enabled(&self, id: &str) -> bool {
        if id == "local-admin" {
            let snapshot = self.snapshot.read().unwrap();
            let settings = &snapshot.settings;
            return !settings.enabled && settings.listeners.is_empty();
        }
        self.grants_for(id).is_some()
    }

    /// 认证（SPEC §5.1）：enabled=false → 一律 LocalAdmin（现状兼容）；
    /// enabled=true → Bearer 必填 → 指纹索引 O(1) 定位候选 → subtle 常量时间
    /// 比较原文（时序不泄露 token 字节）→ Principal。只查持久表。
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
    /// **只查持久表**：runtime overlay token 不能打开持久监听器。
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
        let snapshot = self.snapshot.read().unwrap().clone();
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
            let snapshot = self.snapshot.read().unwrap();
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
            let runtime = self.runtime.read().unwrap();
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

/// 把 `incoming` 的 grants 并入 `merged`：按 DUT 合并动作并集（顺序保持首次出现）。
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
            "auth 关：无 Bearer 也放行（现状兼容）"
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
        // 无 / 空 / 错 token → 一律 Unauthorized（不区分原因）
        assert_eq!(rt.authenticate(None), Err(AuthError::Unauthorized));
        assert_eq!(rt.authenticate(Some("  ")), Err(AuthError::Unauthorized));
        assert_eq!(
            rt.authenticate(Some("dth-wrong")),
            Err(AuthError::Unauthorized)
        );
        // 正确 token → Agent 主体
        let p = rt.authenticate(Some(&token)).unwrap();
        let (name, id) = p.audit_identity().unwrap();
        assert_eq!(name, "agent-cred-a");
        assert_eq!(id, "cred-a");
        // 停用凭据：指纹仍命中但必须拒绝
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
        // 已授予：read + serial_write 通过
        rt.authorize(&p, "dut-1", DutAction::Read).unwrap();
        rt.authorize(&p, "dut-1", DutAction::SerialWrite).unwrap();
        // 未授予动作 / 未授予 DUT → NotGranted
        assert!(matches!(
            rt.authorize(&p, "dut-1", DutAction::Power),
            Err(AuthError::NotGranted { .. })
        ));
        assert!(matches!(
            rt.authorize(&p, "dut-2", DutAction::Read),
            Err(AuthError::NotGranted { .. })
        ));
        // 空 grants = 未授权任何 DUT
        let token_b = generate_token();
        rt.update(AgentAccessSettings {
            enabled: true,
            agents: vec![cred("cred-b", &token_b, true, vec![])],
            ..Default::default()
        });
        let p_b = rt.authenticate(Some(&token_b)).unwrap();
        assert!(rt.authorize(&p_b, "dut-1", DutAction::Read).is_err());
        // LocalAdmin 不受限
        rt.authorize(&Principal::LocalAdmin, "dut-9", DutAction::Flash)
            .unwrap();
    }

    #[test]
    fn generated_token_shape_and_fingerprint_stability() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert!(t1.starts_with("dth-"), "{t1}");
        assert_eq!(t1.len(), 4 + 43, "32 字节 base64url 无填充 = 43 字符");
        assert_ne!(t1, t2, "两次生成必须不同");
        assert_eq!(token_fingerprint(&t1), token_fingerprint(&t1));
        assert_ne!(token_fingerprint(&t1), token_fingerprint(&t2));
    }

    #[test]
    fn settings_roundtrips_camel_case_and_defaults_compat() {
        // 坑 #15：往返必须写 camelCase 键
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
        // 旧文件无 agentAccess 段：由 SettingsFile 级 default 兜底（集成测试见 tests/）
        // 默认结构序列化：agents 为空 → 键省略
        let back = serde_json::to_value(AgentAccessSettings::default()).unwrap();
        assert!(back.get("agents").is_none());
        assert_eq!(back["enabled"], serde_json::json!(false));
    }

    #[test]
    fn lan_access_defaults_off_and_roundtrips_camel_case() {
        // 旧配置没有 lanAccess：必须默认关闭（坑 #19/#24 同款兼容）。
        let legacy: AgentAccessSettings = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "httpPort": 8800
        }))
        .unwrap();
        assert!(!legacy.lan_access, "旧配置无 lanAccess 时必须默认关闭");

        let back = serde_json::to_value(AgentAccessSettings::default()).unwrap();
        assert!(
            back.get("lanAccess").is_none(),
            "默认值 false 不应序列化: {back}"
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
            "旧 listeners[] 无 dutId 必须是通用监听器"
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
