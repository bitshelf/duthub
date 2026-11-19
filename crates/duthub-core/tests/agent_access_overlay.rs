//! 共享 `AgentAccessRuntime` 的 runtime overlay 回归（W1 / T06）：
//!
//! - internal 认证只查 overlay（持久 auth 关闭也能用会话 token）；
//! - http 认证只查持久表（会话 token 永远打不开主监听器）；
//! - authorize / grants_for / lease_credential_enabled 合并两表；
//! - `update(settings)` 保留 overlay，`settings()`/`enabled()` 只看持久表；
//! - set_runtime_listener 的 4 条拒绝规则、remove 幂等与吊销；
//! - AgentCredential Debug 打码（F25）与“会话 token 永不落 settings.json”。

use std::path::{Path, PathBuf};

use duthub_core::agent_access::{
    AgentAccessSettings, AgentCredential, AgentListener, AuthError, DutAction, DutGrant,
    generate_token,
};
use duthub_core::{AgentAccessRuntime, Principal, store};

fn new_credential(
    id: &str,
    listener_id: Option<&str>,
    token: &str,
    enabled: bool,
    grants: Vec<DutGrant>,
) -> AgentCredential {
    AgentCredential {
        id: id.into(),
        listener_id: listener_id.map(str::to_owned),
        agent_name: format!("agent-{id}"),
        token: token.into(),
        enabled,
        grants,
    }
}

fn listener(id: &str) -> AgentListener {
    AgentListener {
        id: id.into(),
        port: 0,
        dut_id: Some("dut-1".into()),
    }
}

fn grant(dut_id: &str, actions: Vec<DutAction>) -> DutGrant {
    DutGrant {
        dut_id: dut_id.into(),
        actions,
    }
}

fn agent(credential_id: &str) -> Principal {
    Principal::Agent {
        agent_name: format!("agent-{credential_id}"),
        credential_id: credential_id.into(),
    }
}

fn registration(id: &str, token: &str, grants: Vec<DutGrant>) -> (AgentListener, AgentCredential) {
    (
        listener(id),
        new_credential(&format!("cred-{id}"), Some(id), token, true, grants),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-overlay-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings_file_in(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

/// internal 认证只查 overlay：持久 auth 关闭 + 无持久监听器也成立；空 id/token、
/// 错 token、跨监听器 token 一律 Unauthorized。
#[test]
fn internal_auth_uses_only_overlay_when_persistent_auth_is_off() {
    let token = generate_token();
    let other_token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    assert!(!rt.enabled());
    let (session, credential) = registration(
        "session-a",
        &token,
        vec![grant("dut-1", vec![DutAction::Read])],
    );
    rt.set_runtime_listener(session, credential).unwrap();
    let (other, other_credential) = registration(
        "session-b",
        &other_token,
        vec![grant("dut-1", vec![DutAction::Read])],
    );
    rt.set_runtime_listener(other, other_credential).unwrap();

    let principal = rt.authenticate_internal("session-a", &token).unwrap();
    assert_eq!(
        principal.audit_identity(),
        Some(("agent-cred-session-a", "cred-session-a"))
    );
    assert_eq!(
        rt.authenticate_internal("session-a", ""),
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        rt.authenticate_internal("", &token),
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        rt.authenticate_internal("session-a", "   "),
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        rt.authenticate_internal("session-a", "dth-wrong"),
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        rt.authenticate_internal("session-a", &other_token),
        Err(AuthError::Unauthorized),
        "另一个会话的 token 不得跨监听器使用"
    );
    assert_eq!(
        rt.authenticate_internal("missing", &token),
        Err(AuthError::Unauthorized)
    );

    // overlay 不改变持久视图，也不打开持久 HTTP 入口。
    assert!(!rt.enabled(), "overlay 不改变 enabled() 的持久语义");
    assert!(
        rt.settings().agents.is_empty(),
        "settings() 不含 overlay 凭据"
    );
    assert_eq!(
        rt.authenticate_http(Some("session-a"), Some(&token)),
        Err(AuthError::Unauthorized),
        "持久表不认识 session-a"
    );
    assert_eq!(
        rt.authenticate_http(None, Some(&token)),
        Err(AuthError::Unauthorized),
        "auth 关闭时 HTTP 一律拒绝"
    );
}

/// 两张表严格隔离：持久 token 不能通过 internal 认证，会话 token 不能通过
/// `authenticate_http` 打开持久监听器。
#[test]
fn http_auth_never_accepts_runtime_tokens_and_internal_never_accepts_persistent_tokens() {
    let persistent_token = generate_token();
    let session_token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![new_credential(
            "cred-persist",
            Some("lab"),
            &persistent_token,
            true,
            vec![grant("dut-1", vec![DutAction::Read])],
        )],
        listeners: vec![AgentListener {
            id: "lab".into(),
            port: 8799,
            dut_id: None,
        }],
        ..Default::default()
    });
    let (session, credential) = registration(
        "session-a",
        &session_token,
        vec![grant("dut-1", vec![DutAction::Read])],
    );
    rt.set_runtime_listener(session, credential).unwrap();

    assert_eq!(
        rt.authenticate_http(Some("lab"), Some(&persistent_token))
            .unwrap()
            .audit_identity(),
        Some(("agent-cred-persist", "cred-persist"))
    );
    assert_eq!(
        rt.authenticate_http(Some("lab"), Some(&session_token)),
        Err(AuthError::Unauthorized),
        "会话 token 不得通过持久表"
    );
    assert_eq!(
        rt.authenticate_internal("session-a", &persistent_token),
        Err(AuthError::Unauthorized),
        "持久 token 不得进入 overlay 认证"
    );
    assert!(
        rt.authenticate_internal("session-a", &session_token)
            .is_ok()
    );
}

/// 授权合并：持久与 overlay 凭据都可见；同一 id 的 grants 按 DUT 取动作并集；
/// 任一命中凭据停用 → None（fail closed）。
#[test]
fn merged_grants_cover_persistent_and_overlay_and_deny_disabled() {
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: false,
        agents: vec![
            new_credential(
                "cred-persist",
                None,
                &generate_token(),
                true,
                vec![grant("dut-1", vec![DutAction::Read])],
            ),
            new_credential(
                "cred-dup",
                None,
                &generate_token(),
                true,
                vec![grant("dut-1", vec![DutAction::Read])],
            ),
            new_credential("cred-off", None, &generate_token(), false, vec![]),
        ],
        ..Default::default()
    });
    let (session, session_credential) = (
        listener("session-a"),
        new_credential(
            "cred-session",
            Some("session-a"),
            &generate_token(),
            true,
            vec![grant("dut-2", vec![DutAction::Read])],
        ),
    );
    rt.set_runtime_listener(session, session_credential)
        .unwrap();
    let (dup_session, dup_credential) = (
        listener("session-dup"),
        new_credential(
            "cred-dup",
            Some("session-dup"),
            &generate_token(),
            true,
            vec![grant("dut-2", vec![DutAction::SerialWrite])],
        ),
    );
    rt.set_runtime_listener(dup_session, dup_credential)
        .unwrap();

    // 持久凭据
    assert!(rt.grants_for("cred-persist").is_some());
    rt.authorize(&agent("cred-persist"), "dut-1", DutAction::Read)
        .unwrap();
    assert!(
        rt.authorize(&agent("cred-persist"), "dut-2", DutAction::Read)
            .is_err()
    );

    // overlay 凭据（持久 auth 关闭也可见）
    assert!(rt.grants_for("cred-session").is_some());
    rt.authorize(&agent("cred-session"), "dut-2", DutAction::Read)
        .unwrap();
    assert!(
        rt.authorize(&agent("cred-session"), "dut-1", DutAction::Read)
            .is_err()
    );

    // 同 id 合并：dut-1 read（持久）+ dut-2 serialWrite（overlay）
    let merged = rt.grants_for("cred-dup").unwrap();
    assert_eq!(merged.len(), 2, "{merged:?}");
    rt.authorize(&agent("cred-dup"), "dut-1", DutAction::Read)
        .unwrap();
    rt.authorize(&agent("cred-dup"), "dut-2", DutAction::SerialWrite)
        .unwrap();

    // 停用 / 未知 → None；LocalAdmin 不受限
    assert_eq!(rt.grants_for("cred-off"), None);
    assert_eq!(rt.grants_for("cred-unknown"), None);
    rt.authorize(&Principal::LocalAdmin, "dut-9", DutAction::Flash)
        .unwrap();
}

/// `update(settings)` 只重建持久表：overlay 监听器/凭据保留，token 仍可用。
#[test]
fn update_preserves_overlay_and_persistent_views_stay_clean() {
    let session_token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let (session, credential) = registration(
        "session-a",
        &session_token,
        vec![grant("dut-1", vec![DutAction::Read])],
    );
    rt.set_runtime_listener(session, credential).unwrap();

    rt.update(AgentAccessSettings {
        enabled: true,
        agents: vec![new_credential(
            "cred-persist",
            None,
            &generate_token(),
            true,
            vec![],
        )],
        ..Default::default()
    });

    assert!(rt.is_runtime_listener("session-a"), "update 不得清 overlay");
    assert!(
        rt.authenticate_internal("session-a", &session_token)
            .is_ok()
    );
    assert!(rt.grants_for("cred-session-a").is_some());
    let settings = rt.settings();
    assert_eq!(settings.agents.len(), 1);
    assert_eq!(settings.agents[0].id, "cred-persist");
    assert!(
        !settings
            .agents
            .iter()
            .any(|credential| credential.token == session_token),
        "settings() 永远不暴露会话 token"
    );
}

/// update 引入同 id 的持久监听器时：overlay 仍保留（update 不清 overlay），但
/// internal 入口让位给持久表——临时 token 不能打开主监听器。
#[test]
fn persistent_listener_added_after_overlay_wins_the_auth_path() {
    let session_token = generate_token();
    let persistent_token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let (session, credential) = registration("lab", &session_token, vec![]);
    rt.set_runtime_listener(session, credential).unwrap();
    assert!(rt.is_runtime_listener("lab"));

    rt.update(AgentAccessSettings {
        enabled: true,
        agents: vec![new_credential(
            "cred-persist",
            Some("lab"),
            &persistent_token,
            true,
            vec![],
        )],
        listeners: vec![AgentListener {
            id: "lab".into(),
            port: 8799,
            dut_id: None,
        }],
        ..Default::default()
    });
    assert!(
        rt.authenticate_internal("lab", &session_token).is_ok(),
        "update 必须保留 overlay"
    );
    assert!(
        !rt.is_runtime_listener("lab"),
        "持久监听器必须赢回 http 认证路径"
    );
    assert_eq!(
        rt.authenticate_http(Some("lab"), Some(&session_token)),
        Err(AuthError::Unauthorized)
    );
    assert!(
        rt.authenticate_http(Some("lab"), Some(&persistent_token))
            .is_ok()
    );
}

/// set_runtime_listener 的 4 条拒绝规则。
#[test]
fn runtime_listener_registration_rejects_four_invalid_shapes() {
    let token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());

    // 规则 1：监听器 id 为空/全空白
    for id in ["", "   "] {
        let error = rt
            .set_runtime_listener(
                listener(id),
                new_credential("cred-x", Some(id), &token, true, vec![]),
            )
            .unwrap_err();
        assert!(matches!(error, AuthError::InvalidListener(_)), "{error:?}");
    }

    // 规则 2：凭据 id 或 token 为空
    for credential in [
        new_credential("", Some("session-x"), &token, true, vec![]),
        new_credential("cred-x", Some("session-x"), "", true, vec![]),
        new_credential("cred-x", Some("session-x"), "  ", true, vec![]),
    ] {
        let error = rt
            .set_runtime_listener(listener("session-x"), credential)
            .unwrap_err();
        assert!(matches!(error, AuthError::InvalidListener(_)), "{error:?}");
    }

    // 规则 3：credential.listener_id 与 listener.id 不一致
    for listener_id in [None, Some("session-other")] {
        let error = rt
            .set_runtime_listener(
                listener("session-x"),
                new_credential("cred-x", listener_id, &token, true, vec![]),
            )
            .unwrap_err();
        assert!(matches!(error, AuthError::InvalidListener(_)), "{error:?}");
    }

    // 规则 4：id 与持久监听器冲突（防止临时 token 劫持主监听器）
    let persistent = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        listeners: vec![AgentListener {
            id: "lab".into(),
            port: 8799,
            dut_id: None,
        }],
        ..Default::default()
    });
    let error = persistent
        .set_runtime_listener(
            listener("lab"),
            new_credential("cred-x", Some("lab"), &token, true, vec![]),
        )
        .unwrap_err();
    assert!(matches!(error, AuthError::InvalidListener(_)), "{error:?}");

    // 规则 4（续）：同 id 重复注册 runtime 监听器
    rt.set_runtime_listener(
        listener("session-x"),
        new_credential("cred-x", Some("session-x"), &token, true, vec![]),
    )
    .unwrap();
    let error = rt
        .set_runtime_listener(
            listener("session-x"),
            new_credential("cred-y", Some("session-x"), &token, true, vec![]),
        )
        .unwrap_err();
    assert!(matches!(error, AuthError::InvalidListener(_)), "{error:?}");
}

/// remove 幂等且立即吊销 token；移除后可重新注册。
#[test]
fn remove_runtime_listener_is_idempotent_and_revokes_token() {
    let token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let (session, credential) = registration("session-a", &token, vec![]);
    rt.set_runtime_listener(session, credential).unwrap();
    assert!(rt.authenticate_internal("session-a", &token).is_ok());

    rt.remove_runtime_listener("session-a").unwrap();
    assert!(!rt.is_runtime_listener("session-a"));
    assert_eq!(
        rt.authenticate_internal("session-a", &token),
        Err(AuthError::Unauthorized)
    );
    assert_eq!(rt.grants_for("cred-session-a"), None);
    // 幂等：重复删除不报错
    rt.remove_runtime_listener("session-a").unwrap();
    rt.remove_runtime_listener("never-registered").unwrap();
    // 吊销后可重新注册（端口/线程回基线场景）
    let (session, credential) = registration("session-a", &token, vec![]);
    rt.set_runtime_listener(session, credential).unwrap();
    assert!(rt.authenticate_internal("session-a", &token).is_ok());
}

/// F25：`AgentCredential` 的 Debug 不得包含 token 明文。
#[test]
fn credential_debug_masks_token() {
    let token = generate_token();
    let credential = new_credential(
        "cred-a",
        Some("session-a"),
        &token,
        true,
        vec![grant("dut-1", vec![DutAction::Read])],
    );
    let debug = format!("{credential:?}");
    assert!(!debug.contains(&token), "Debug 泄露 token: {debug}");
    assert!(debug.contains("[REDACTED]"), "{debug}");
    assert!(debug.contains("cred-a"), "{debug}");

    let settings = AgentAccessSettings {
        enabled: true,
        agents: vec![credential.clone()],
        ..Default::default()
    };
    let debug = format!("{settings:?}");
    assert!(!debug.contains(&token), "嵌套 Debug 泄露 token: {debug}");
}

/// 会话 token 永远不写入 settings.json：GUI 保存持久段后磁盘文件不含 overlay。
#[test]
fn session_token_is_never_written_to_settings_json() {
    let dir = temp_dir("settings-json");
    let session_token = generate_token();
    let persistent_token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![new_credential(
            "cred-persist",
            None,
            &persistent_token,
            true,
            vec![],
        )],
        ..Default::default()
    });
    let (session, credential) = registration("session-a", &session_token, vec![]);
    rt.set_runtime_listener(session, credential).unwrap();

    let mut settings = store::load_settings(&dir).unwrap();
    settings.agent_access = rt.settings();
    store::save_settings_atomic(&dir, &settings).unwrap();

    let raw = std::fs::read_to_string(settings_file_in(&dir)).unwrap();
    assert!(!raw.contains(&session_token), "会话 token 不得落盘: {raw}");
    assert!(
        !raw.contains("cred-session-a"),
        "overlay 凭据不得落盘: {raw}"
    );
    assert!(!raw.contains("session-a"), "overlay 监听器不得落盘: {raw}");

    // Reloading restores persistent credentials without reviving runtime overlay listeners.
    let reloaded = store::load_settings(&dir).unwrap().agent_access;
    assert_eq!(reloaded.agents.len(), 1);
    let restored = AgentAccessRuntime::from_settings(reloaded);
    assert!(!restored.is_runtime_listener("session-a"));
    assert_eq!(
        restored.authenticate_internal("session-a", &session_token),
        Err(AuthError::Unauthorized)
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 租约归属复核（`lease_credential_enabled`）同样合并 overlay：
/// 会话凭据不被误判停用；停用/未知凭据拒绝；local-admin 口径保持既有语义。
#[test]
fn lease_credential_enabled_merges_overlay() {
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    assert!(
        rt.lease_credential_enabled("local-admin"),
        "auth 关 + 无持久监听器时 local-admin 有效（既有语义）"
    );

    let token = generate_token();
    let (session, credential) = registration("session-a", &token, vec![]);
    rt.set_runtime_listener(session.clone(), credential)
        .unwrap();
    assert!(
        rt.lease_credential_enabled("cred-session-a"),
        "overlay 凭据不得被误判为停用"
    );
    assert!(!rt.lease_credential_enabled("cred-unknown"));

    // 持久凭据启用/停用：合并路径与既有语义一致
    let mut settings = AgentAccessSettings {
        enabled: true,
        agents: vec![new_credential(
            "cred-p",
            None,
            &generate_token(),
            true,
            vec![],
        )],
        ..Default::default()
    };
    rt.update(settings.clone());
    assert!(rt.lease_credential_enabled("cred-p"));
    assert!(
        !rt.lease_credential_enabled("local-admin"),
        "auth 开启后禁止占位凭据"
    );
    settings.agents[0].enabled = false;
    rt.update(settings);
    assert!(!rt.lease_credential_enabled("cred-p"));

    // 停用 overlay 凭据：拒绝（fail closed）
    rt.remove_runtime_listener("session-a").unwrap();
    let (session, credential) = (
        session,
        credential_for_disable("cred-session-a", "session-a"),
    );
    rt.set_runtime_listener(session, credential).unwrap();
    assert!(!rt.lease_credential_enabled("cred-session-a"));
}

fn credential_for_disable(id: &str, listener_id: &str) -> AgentCredential {
    new_credential(id, Some(listener_id), &generate_token(), false, vec![])
}
