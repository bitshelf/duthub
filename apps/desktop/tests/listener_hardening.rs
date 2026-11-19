//! Per-session listener hardening (W1).
//!
//! `listener::issue` must register a session credential through
//! `AgentAccessRuntime::set_session_listener`, which enforces a single-DUT,
//! read-only grant. The listener exposes its runtime credential id so the audit
//! inspector can filter rows for this session; revocation (explicit or on drop)
//! must make both the listener and the credential disappear from the runtime.

use std::sync::Arc;

use duthub_core::agent_access::{
    AgentAccessRuntime, AgentAccessSettings, AgentCredential, AgentListener, AuthError, DutAction,
    DutGrant,
};
use duthub_core::audit::AuditSink;
use duthub_core::lease::ControlLeaseManager;
use duthub_desktop::agent_host::listener;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::snapshot::SnapshotState;

fn runtime() -> Arc<AgentAccessRuntime> {
    AgentAccessRuntime::from_settings(AgentAccessSettings::default())
}

fn access(auth: Arc<AgentAccessRuntime>) -> Arc<AccessContext> {
    Arc::new(AccessContext::new(
        auth,
        Arc::new(ControlLeaseManager::new(None)),
        AuditSink::memory(),
    ))
}

fn grant(dut_id: &str, actions: Vec<DutAction>) -> DutGrant {
    DutGrant {
        dut_id: dut_id.into(),
        actions,
    }
}

fn session_listener(dut_id: Option<&str>) -> AgentListener {
    AgentListener {
        id: "session-x".into(),
        port: 0,
        dut_id: dut_id.map(str::to_owned),
    }
}

fn session_credential(grants: Vec<DutGrant>) -> AgentCredential {
    AgentCredential {
        id: "cred-x".into(),
        listener_id: Some("session-x".into()),
        agent_name: "session-agent".into(),
        token: duthub_core::agent_access::generate_token(),
        enabled: true,
        grants,
    }
}

#[test]
fn issue_registers_a_read_only_credential_and_exposes_its_id() {
    let auth = runtime();
    let issued = listener::issue(
        Arc::new(SnapshotState::empty()),
        access(auth.clone()),
        auth.clone(),
        "dut-a",
    )
    .expect("session listener issue");

    assert!(issued.id.starts_with("session-"));
    assert!(issued.port > 0);
    assert!(
        issued.credential_id.starts_with("cred-session-"),
        "{}",
        issued.credential_id
    );
    assert!(auth.is_runtime_listener(&issued.id));
    assert!(auth.credential_enabled(&issued.credential_id));
    assert_eq!(
        auth.grants_for(&issued.credential_id),
        Some(vec![grant("dut-a", vec![DutAction::Read])]),
        "session credentials are read-only and bound to one DUT"
    );
    assert!(
        auth.authenticate_internal(&issued.id, &issued.token)
            .is_ok()
    );

    let debug = format!("{issued:?}");
    assert!(debug.contains(&issued.credential_id), "{debug}");
    assert!(debug.contains("[REDACTED]"), "{debug}");
    assert!(!debug.contains(&issued.token), "Debug leaked the token");

    let id = issued.id.clone();
    let credential_id = issued.credential_id.clone();
    listener::revoke(issued, &auth);
    assert!(!auth.is_runtime_listener(&id));
    assert!(!auth.credential_enabled(&credential_id));
}

#[test]
fn drop_also_revokes_the_runtime_credential() {
    let auth = runtime();
    let issued = listener::issue(
        Arc::new(SnapshotState::empty()),
        access(auth.clone()),
        auth.clone(),
        "dut-a",
    )
    .expect("session listener issue");
    let credential_id = issued.credential_id.clone();
    assert!(auth.credential_enabled(&credential_id));
    drop(issued);
    assert!(!auth.credential_enabled(&credential_id));
}

#[test]
fn issue_rejects_an_empty_dut_id() {
    let auth = runtime();
    let error = listener::issue(
        Arc::new(SnapshotState::empty()),
        access(auth.clone()),
        auth.clone(),
        "   ",
    )
    .expect_err("empty DUT id must be rejected");
    assert!(
        matches!(error, listener::ListenerError::Invalid(_)),
        "{error:?}"
    );
}

#[test]
fn set_session_listener_enforces_the_single_dut_read_contract() {
    let auth = runtime();

    // No DUT binding.
    let error = auth
        .set_session_listener(
            session_listener(None),
            session_credential(vec![grant("dut-a", vec![DutAction::Read])]),
        )
        .expect_err("unbound listener must be rejected");
    assert!(matches!(error, AuthError::InvalidListener(_)), "{error:?}");

    // Empty grants.
    auth.set_session_listener(
        session_listener(Some("dut-a")),
        session_credential(Vec::new()),
    )
    .expect_err("empty grants must be rejected");

    // Cross-DUT grants.
    auth.set_session_listener(
        session_listener(Some("dut-a")),
        session_credential(vec![
            grant("dut-a", vec![DutAction::Read]),
            grant("dut-b", vec![DutAction::Read]),
        ]),
    )
    .expect_err("cross-DUT grants must be rejected");

    // Write escalation.
    auth.set_session_listener(
        session_listener(Some("dut-a")),
        session_credential(vec![grant(
            "dut-a",
            vec![DutAction::Read, DutAction::SerialWrite],
        )]),
    )
    .expect_err("write grants must be rejected");

    // Exactly one read grant succeeds and becomes visible to the inspector.
    auth.set_session_listener(
        session_listener(Some("dut-a")),
        session_credential(vec![grant("dut-a", vec![DutAction::Read])]),
    )
    .expect("read-only single-DUT credential is valid");
    assert!(auth.credential_enabled("cred-x"));
    assert!(!auth.credential_enabled("cred-unknown"));
}

#[test]
fn credential_enabled_tracks_persistent_settings_updates() {
    let token = duthub_core::agent_access::generate_token();
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: true,
        agents: vec![AgentCredential {
            id: "cred-p".into(),
            listener_id: None,
            agent_name: "persistent".into(),
            token,
            enabled: true,
            grants: vec![grant("dut-a", vec![DutAction::Read])],
        }],
        ..Default::default()
    });
    assert!(auth.credential_enabled("cred-p"));
    let mut settings = auth.settings();
    settings.agents[0].enabled = false;
    auth.update(settings);
    assert!(!auth.credential_enabled("cred-p"));
}
