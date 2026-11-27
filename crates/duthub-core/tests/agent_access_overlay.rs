//! Runtime overlay regression tests for shared `AgentAccessRuntime`:
//!
//! - internal auth checks only the overlay (session tokens work even when persistent auth is off);
//! - HTTP auth checks only the persistent table (session tokens never open main listeners);
//! - `authorize` / `grants_for` / `lease_credential_enabled` merge both tables;
//! - `update(settings)` preserves the overlay, while `settings()` / `enabled()` read only the
//!   persistent table;
//! - `set_runtime_listener` enforces its four rejection rules, and removal is idempotent and
//!   revokes tokens;
//! - `AgentCredential` masks tokens in `Debug`, and session tokens never touch `settings.json`.

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

/// Internal auth checks only the overlay: works even when persistent auth is off and no persistent
/// listeners exist; empty id/token, wrong token, and cross-listener token all return `Unauthorized`.
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
        "another session's token must not authenticate across listeners"
    );
    assert_eq!(
        rt.authenticate_internal("missing", &token),
        Err(AuthError::Unauthorized)
    );

    // Overlay does not alter the persistent view or open persistent HTTP endpoints.
    assert!(
        !rt.enabled(),
        "overlay must not change persistent enabled() semantics"
    );
    assert!(
        rt.settings().agents.is_empty(),
        "settings() must not include overlay credentials"
    );
    assert_eq!(
        rt.authenticate_http(Some("session-a"), Some(&token)),
        Err(AuthError::Unauthorized),
        "persistent table must not recognize session-a"
    );
    assert_eq!(
        rt.authenticate_http(None, Some(&token)),
        Err(AuthError::Unauthorized),
        "HTTP must reject all requests when auth is off"
    );
}

/// Both tables stay strictly isolated: persistent tokens cannot authenticate internally, and
/// session tokens cannot open persistent listeners via `authenticate_http`.
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
        "session token must not pass persistent table auth"
    );
    assert_eq!(
        rt.authenticate_internal("session-a", &persistent_token),
        Err(AuthError::Unauthorized),
        "persistent token must not enter overlay auth"
    );
    assert!(
        rt.authenticate_internal("session-a", &session_token)
            .is_ok()
    );
}

/// Merged grants: both persistent and overlay credentials are visible; grants with the same id
/// take the union of actions per DUT; any matching disabled credential fails closed with `None`.
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

    // Persistent credential.
    assert!(rt.grants_for("cred-persist").is_some());
    rt.authorize(&agent("cred-persist"), "dut-1", DutAction::Read)
        .unwrap();
    assert!(
        rt.authorize(&agent("cred-persist"), "dut-2", DutAction::Read)
            .is_err()
    );

    // Overlay credential (visible even while persistent auth is off).
    assert!(rt.grants_for("cred-session").is_some());
    rt.authorize(&agent("cred-session"), "dut-2", DutAction::Read)
        .unwrap();
    assert!(
        rt.authorize(&agent("cred-session"), "dut-1", DutAction::Read)
            .is_err()
    );

    // Same-id merge: dut-1 read (persistent) + dut-2 serialWrite (overlay).
    let merged = rt.grants_for("cred-dup").unwrap();
    assert_eq!(merged.len(), 2, "{merged:?}");
    rt.authorize(&agent("cred-dup"), "dut-1", DutAction::Read)
        .unwrap();
    rt.authorize(&agent("cred-dup"), "dut-2", DutAction::SerialWrite)
        .unwrap();

    // Disabled or unknown -> None; LocalAdmin is unrestricted.
    assert_eq!(rt.grants_for("cred-off"), None);
    assert_eq!(rt.grants_for("cred-unknown"), None);
    rt.authorize(&Principal::LocalAdmin, "dut-9", DutAction::Flash)
        .unwrap();
}

/// `update(settings)` rebuilds only the persistent table: overlay listeners/credentials remain and
/// their tokens stay valid.
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

    assert!(
        rt.is_runtime_listener("session-a"),
        "update must not clear overlay"
    );
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
        "settings() must never expose session tokens"
    );
}

/// When `update` introduces a persistent listener with the same id, the overlay stays intact but
/// the persistent table wins the HTTP auth path so temporary tokens cannot open main listeners.
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
        "update must preserve the overlay"
    );
    assert!(
        !rt.is_runtime_listener("lab"),
        "persistent listener must reclaim the HTTP auth path"
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

/// The four rejection rules enforced by `set_runtime_listener`.
#[test]
fn runtime_listener_registration_rejects_four_invalid_shapes() {
    let token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());

    // Rule 1: listener id is empty or whitespace-only.
    for id in ["", "   "] {
        let error = rt
            .set_runtime_listener(
                listener(id),
                new_credential("cred-x", Some(id), &token, true, vec![]),
            )
            .unwrap_err();
        assert!(matches!(error, AuthError::InvalidListener(_)), "{error:?}");
    }

    // Rule 2: credential id or token is empty.
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

    // Rule 3: `credential.listener_id` does not match `listener.id`.
    for listener_id in [None, Some("session-other")] {
        let error = rt
            .set_runtime_listener(
                listener("session-x"),
                new_credential("cred-x", listener_id, &token, true, vec![]),
            )
            .unwrap_err();
        assert!(matches!(error, AuthError::InvalidListener(_)), "{error:?}");
    }

    // Rule 4: id collides with a persistent listener (prevents hijacking a main listener).
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

    // Rule 4 (continued): duplicate registration of the same runtime listener id.
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

/// Removing a runtime listener is idempotent, immediately revokes its token, and allows
/// re-registration.
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
    // Idempotent: repeated removals do not error.
    rt.remove_runtime_listener("session-a").unwrap();
    rt.remove_runtime_listener("never-registered").unwrap();
    // Re-registration succeeds after revocation.
    let (session, credential) = registration("session-a", &token, vec![]);
    rt.set_runtime_listener(session, credential).unwrap();
    assert!(rt.authenticate_internal("session-a", &token).is_ok());
}

/// `AgentCredential` `Debug` output must never include the plaintext token.
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
    assert!(!debug.contains(&token), "Debug leaked token: {debug}");
    assert!(debug.contains("[REDACTED]"), "{debug}");
    assert!(debug.contains("cred-a"), "{debug}");

    let settings = AgentAccessSettings {
        enabled: true,
        agents: vec![credential.clone()],
        ..Default::default()
    };
    let debug = format!("{settings:?}");
    assert!(
        !debug.contains(&token),
        "nested Debug leaked token: {debug}"
    );
}

/// Session tokens are never written to `settings.json`: saving persistent settings omits overlay
/// entries from disk.
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
    assert!(
        !raw.contains(&session_token),
        "session token must never touch disk: {raw}"
    );
    assert!(
        !raw.contains("cred-session-a"),
        "overlay credential must never touch disk: {raw}"
    );
    assert!(
        !raw.contains("session-a"),
        "overlay listener must never touch disk: {raw}"
    );

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

/// Lease credential revalidation (`lease_credential_enabled`) also merges the overlay: session
/// credentials stay enabled, disabled/unknown credentials are rejected, and `local-admin` keeps
/// its existing semantics.
#[test]
fn lease_credential_enabled_merges_overlay() {
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    assert!(
        rt.lease_credential_enabled("local-admin"),
        "local-admin stays valid when auth is off and no persistent listeners exist"
    );

    let token = generate_token();
    let (session, credential) = registration("session-a", &token, vec![]);
    rt.set_runtime_listener(session.clone(), credential)
        .unwrap();
    assert!(
        rt.lease_credential_enabled("cred-session-a"),
        "overlay credential must not be treated as disabled"
    );
    assert!(!rt.lease_credential_enabled("cred-unknown"));

    // Persistent credential enable/disable keeps existing semantics through the merged path.
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
        "placeholder local-admin credential must be rejected once auth is enabled"
    );
    settings.agents[0].enabled = false;
    rt.update(settings);
    assert!(!rt.lease_credential_enabled("cred-p"));

    // Disabled overlay credential: fail closed.
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
