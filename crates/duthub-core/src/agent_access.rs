//! Agent 接入凭据 / 授权模型与认证运行时（`docs/SECURITY.md`）。
//!
//! 四层语义的第一、二层（SPEC §14）：**认证（authenticate）= 谁能来；
//! 授权（authorize）= 谁能做**。租约（lease.rs）= 此刻谁能控制；
//! 写入通道复核（duthub-device）= 谁最终执行硬件动作。
//!
//! - auth 总开关默认关（`AgentAccessSettings::default().enabled == false`）：
//!   关闭时任何请求都视为 [`Principal::LocalAdmin`]（现状兼容，偏差 7）；
//!   开启后 deny by default——无有效凭据一律拒绝，未授予的动作一律拒绝。
//! - 凭据与 Grant 持久化在 settings.json `agentAccess` 段（偏差 1；
//!   默认值整段跳过序列化，`docs/examples/settings.json` 往返测试不动一行仍绿）。
//! - token 明文存储是**用户明确要求**（界面始终可见并标注风险）；
//!   运行时索引只用 SHA-256 指纹，比较用 `subtle` 常量时间等价，
//!   审计/日志绝不落 token（audit.rs 脱敏）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::DeviceResult;
use crate::store;

/// MCP HTTP 监听端口默认值（偏差 8：可配置，重启生效，绑定地址仍强制 loopback）。
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
    /// auth 总开关：false（默认）= HTTP 不要求 Bearer（现状兼容）；
    /// true = deny by default，写工具必须授权 + 租约。
    #[serde(default)]
    pub enabled: bool,
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
}

impl Default for AgentAccessSettings {
    fn default() -> Self {
        Self {
            enabled: false,
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
    pub actions: Vec<DutAction>,
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
        }
    }
}

/// Agent 凭据（SPEC §5.1）：token 明文存储（用户明确要求原文可见；风险由 UI 标注）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCredential {
    /// 稳定标识（`cred-<slug>`）
    pub id: String,
    /// Legacy credentials are scoped to the primary MCP listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener_id: Option<String>,
    /// 展示名（SPEC §5.1.3：不能作为认证凭据）
    pub agent_name: String,
    /// 明文 token（`dth-<43 字符>`，≥256 bit 熵）
    pub token: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 空 = 未授权任何 DUT（deny by default）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<DutGrant>,
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
}

impl AuthError {
    pub fn message(&self) -> String {
        match self {
            AuthError::Unauthorized => "未认证：缺少或无效的 Bearer Token".into(),
            AuthError::NotGranted { dut_id, action } => {
                format!("未授权：凭据未被授予 {dut_id} 的 {action} 动作")
            }
        }
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

/// Agent 接入运行时（desktop 构造，Arc 共享给 MCP 中间件 / 租约管理器 / GUI）。
pub struct AgentAccessRuntime {
    snapshot: RwLock<Arc<AuthSnapshot>>,
}

impl AgentAccessRuntime {
    pub fn from_settings(settings: AgentAccessSettings) -> Arc<Self> {
        Arc::new(Self {
            snapshot: RwLock::new(Arc::new(AuthSnapshot::build(settings))),
        })
    }

    /// GUI 保存凭据 / 开关后整体热替换（settings.json 原子写由调用方负责）。
    pub fn update(&self, settings: AgentAccessSettings) {
        let mut guard = self.snapshot.write().unwrap();
        *guard = Arc::new(AuthSnapshot::build(settings));
    }

    /// 当前配置快照（端口 / TTL / 凭据列表）。
    pub fn settings(&self) -> AgentAccessSettings {
        self.snapshot.read().unwrap().settings.clone()
    }

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

    pub(crate) fn lease_credential_enabled(&self, id: &str) -> bool {
        let snapshot = self.snapshot.read().unwrap();
        let settings = &snapshot.settings;
        if id == "local-admin" {
            return !settings.enabled && settings.listeners.is_empty();
        }
        settings
            .agents
            .iter()
            .any(|cred| cred.id == id && cred.enabled)
    }

    /// 认证（SPEC §5.1）：enabled=false → 一律 LocalAdmin（现状兼容）；
    /// enabled=true → Bearer 必填 → 指纹索引 O(1) 定位候选 → subtle 常量时间
    /// 比较原文（时序不泄露 token 字节）→ Principal。
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

    /// 授权（SPEC §5.2）：LocalAdmin 全通过；Agent 按凭据 grants 判定，
    /// deny by default（无凭据 / 凭据停用 / 无该 DUT / 无该动作都拒绝）。
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
        let snapshot = self.snapshot.read().unwrap().clone();
        let cred = snapshot
            .settings
            .agents
            .iter()
            .find(|c| &c.id == credential_id)
            .filter(|c| c.enabled);
        let granted = cred.is_some_and(|c| {
            c.grants
                .iter()
                .any(|g| g.dut_id == dut_id && g.actions.contains(&action))
        });
        if granted {
            Ok(())
        } else {
            Err(AuthError::NotGranted {
                dut_id: dut_id.to_string(),
                action: action.as_str().to_string(),
            })
        }
    }
}

/// DAO 边界预留（偏差 1）：M1 实现是 settings.json；SQLite 迁移只换实现，
/// auth/lease/GUI 不感知。
pub trait AgentAccessStore: Send + Sync {
    fn load(&self) -> DeviceResult<AgentAccessSettings>;
    fn save(&self, settings: &AgentAccessSettings) -> DeviceResult<()>;
}

/// settings.json 实现：读改写只动 `agentAccess` 段（其余段原样保留），
/// 原子写复用 `store::save_settings_atomic`。
pub struct JsonAgentAccessStore {
    config_dir: PathBuf,
}

impl JsonAgentAccessStore {
    pub fn new(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }
}

impl AgentAccessStore for JsonAgentAccessStore {
    fn load(&self) -> DeviceResult<AgentAccessSettings> {
        Ok(store::load_settings(&self.config_dir)?.agent_access)
    }

    fn save(&self, agent_access: &AgentAccessSettings) -> DeviceResult<()> {
        let mut settings = store::load_settings(&self.config_dir)?;
        settings.agent_access = agent_access.clone();
        store::save_settings_atomic(&self.config_dir, &settings)
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
    fn json_store_roundtrip_preserves_other_sections() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("duthub-agentaccess-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 预置 settings.json（appearance 段 + 无 agentAccess）
        let base = serde_json::json!({
            "version": 1,
            "appearance": {"theme": "dark", "language": "zh-CN", "platform": "windows"}
        });
        std::fs::write(
            dir.join("settings.json"),
            serde_json::to_string_pretty(&base).unwrap(),
        )
        .unwrap();
        let store = JsonAgentAccessStore::new(dir.clone());
        // 加载旧文件 → 默认（auth 关）
        let loaded = store.load().unwrap();
        assert_eq!(loaded, AgentAccessSettings::default());
        // 保存凭据 → 其他段保留 + agentAccess 落盘
        let token = generate_token();
        let next = AgentAccessSettings {
            enabled: true,
            agents: vec![cred("cred-x", &token, true, vec![])],
            ..Default::default()
        };
        store.save(&next).unwrap();
        let raw = std::fs::read_to_string(dir.join("settings.json")).unwrap();
        assert!(raw.contains("\"theme\": \"dark\""), "其他段必须保留: {raw}");
        assert!(raw.contains("\"agentAccess\""), "{raw}");
        let reloaded = store.load().unwrap();
        assert!(reloaded.enabled);
        assert_eq!(reloaded.agents.len(), 1);
        assert_eq!(reloaded.agents[0].token, token, "原文存储（用户要求）");
        std::fs::remove_dir_all(&dir).ok();
    }
}
