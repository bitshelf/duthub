//! 每会话 loopback MCP listener（W2）。
//!
//! - `127.0.0.1:0` 临时端口，ID `session-<uuid>`，`listener_dut = Some(dut)`；
//! - 注册到 [`AgentAccessRuntime`] 的 runtime overlay（不写 settings.json），
//!   凭据只读该 DUT（`DutGrant { actions: [Read] }`）；
//! - `AuthSvc` 按 listener id 是否属于 runtime overlay 选择 `authenticate_internal`；
//! - 吊销只做内存删除 + 非阻塞 shutdown 信号（不 join UI 线程）。

use std::fmt;
use std::sync::Arc;

use duthub_core::agent_access::{
    AgentAccessRuntime, AgentCredential, AgentListener, DutAction, DutGrant,
};
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::state::McpState;
use duthub_mcp::transport::{ListenerHandle, spawn_http_listener_loopback};

pub struct SessionListener {
    pub id: String,
    pub port: u16,
    pub token: String,
    /// Runtime credential id used by this session (for audit filtering).
    pub credential_id: String,
    auth: Arc<AgentAccessRuntime>,
    handle: Option<ListenerHandle>,
}

impl fmt::Debug for SessionListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionListener")
            .field("id", &self.id)
            .field("port", &self.port)
            .field("token", &"[REDACTED]")
            .field("credential_id", &self.credential_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum ListenerError {
    Invalid(String),
    Auth(String),
    Bind(String),
}

impl fmt::Display for ListenerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListenerError::Invalid(reason) => write!(formatter, "会话 listener 参数无效: {reason}"),
            ListenerError::Auth(reason) => {
                write!(formatter, "会话 listener 凭据注册失败: {reason}")
            }
            ListenerError::Bind(reason) => write!(formatter, "会话 listener 启动失败: {reason}"),
        }
    }
}

impl std::error::Error for ListenerError {}

/// 签发每会话 listener + 只读 Token。
pub fn issue(
    state: Arc<dyn McpState>,
    access: Arc<AccessContext>,
    auth: Arc<AgentAccessRuntime>,
    dut_id: &str,
) -> Result<SessionListener, ListenerError> {
    let dut_id = dut_id.trim();
    if dut_id.is_empty() {
        return Err(ListenerError::Invalid("DUT id 为空".into()));
    }
    if !Arc::ptr_eq(&auth, &access.auth) {
        return Err(ListenerError::Invalid(
            "auth 必须是 access.auth 的同一实例".into(),
        ));
    }
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let id = format!("session-{uuid}");
    let token = format!(
        "dth-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let listener = AgentListener {
        id: id.clone(),
        port: 0,
        dut_id: Some(dut_id.to_owned()),
    };
    let credential_id = format!("cred-session-{uuid}");
    let credential = AgentCredential {
        id: credential_id.clone(),
        listener_id: Some(id.clone()),
        agent_name: "duthub-agent".into(),
        token: token.clone(),
        enabled: true,
        grants: vec![DutGrant {
            dut_id: dut_id.to_owned(),
            actions: vec![DutAction::Read],
        }],
    };
    auth.set_session_listener(listener, credential)
        .map_err(|error| ListenerError::Auth(error.message()))?;

    let listener_id: Arc<str> = Arc::from(id.as_str());
    let scoped_dut: Arc<str> = Arc::from(dut_id);
    let handle = match spawn_http_listener_loopback(
        state,
        Some(access),
        Some(listener_id),
        Some(scoped_dut),
    ) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = auth.remove_runtime_listener(&id);
            return Err(ListenerError::Bind(error));
        }
    };
    let port = handle.endpoint.port();
    Ok(SessionListener {
        id,
        port,
        token,
        credential_id,
        auth,
        handle: Some(handle),
    })
}

/// 吊销：先删 runtime overlay（立即 401），再丢弃句柄（发 shutdown 信号，不阻塞）。
pub fn revoke(listener: SessionListener, auth: &AgentAccessRuntime) {
    let _ = auth.remove_runtime_listener(&listener.id);
    drop(listener);
}

impl Drop for SessionListener {
    fn drop(&mut self) {
        // 防止忘记 revoke 时 token 永久留在 overlay。
        let _ = self.auth.remove_runtime_listener(&self.id);
        if let Some(handle) = self.handle.take() {
            drop(handle);
        }
    }
}
