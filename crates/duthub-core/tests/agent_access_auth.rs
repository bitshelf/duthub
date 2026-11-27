//! Agent access model integration tests:
//! legacy `settings.json` compatibility (missing `agentAccess` section -> default auth off),
//! `camelCase` round-trip, default-section serialization omission, and deny-by-default
//! authentication/authorization matrix.

use duthub_core::agent_access::{AgentAccessSettings, DutAction, DutGrant, generate_token};
use duthub_core::{AgentAccessRuntime, Principal, SettingsFile};

fn settings_with(agents: Vec<duthub_core::AgentCredential>, enabled: bool) -> SettingsFile {
    let mut settings = duthub_core::store::default_settings();
    settings.agent_access = AgentAccessSettings {
        enabled,
        agents,
        ..Default::default()
    };
    settings
}

/// Legacy `settings.json` without `agentAccess` loads as the default section (auth off, no
/// credentials) and omits the whole section on serialize without changing the legacy file shape.
#[test]
fn legacy_settings_file_defaults_to_auth_off_and_omits_section() {
    let legacy = serde_json::json!({
        "version": 1,
        "appearance": { "theme": "dark", "language": "zh-CN", "platform": "windows" }
    });
    let parsed: SettingsFile = serde_json::from_value(legacy).unwrap();
    assert_eq!(
        parsed.agent_access,
        AgentAccessSettings::default(),
        "legacy file missing section must deserialize to default"
    );
    assert!(!parsed.agent_access.enabled, "auth must default to off");
    assert_eq!(parsed.agent_access.http_port, 8797);
    assert_eq!(parsed.agent_access.lease_ttl_secs, 120);
    // Default section is omitted on serialize (required for lossless round-trips).
    let back = serde_json::to_value(&parsed).unwrap();
    assert!(
        back.get("agentAccess").is_none(),
        "default agentAccess section must be skipped during serialization: {back}"
    );
}

/// Configured `settings.json` with credentials: `camelCase` keys round-trip and non-default
/// section is preserved.
#[test]
fn agent_access_section_roundtrips_camel_case() {
    let token = generate_token();
    let settings = settings_with(
        vec![duthub_core::AgentCredential {
            listener_id: None,
            id: "cred-claude".into(),
            agent_name: "Claude".into(),
            token: token.clone(),
            enabled: true,
            grants: vec![
                DutGrant {
                    dut_id: "dut-1".into(),
                    actions: vec![DutAction::Read, DutAction::SerialWrite],
                },
                DutGrant {
                    dut_id: "dut-2".into(),
                    actions: vec![DutAction::Read, DutAction::Relay],
                },
            ],
        }],
        true,
    );
    let value = serde_json::to_value(&settings).unwrap();
    let section = value
        .get("agentAccess")
        .expect("non-default section must be serialized");
    assert_eq!(section["enabled"], serde_json::json!(true));
    assert_eq!(section["httpPort"], serde_json::json!(8797));
    assert_eq!(section["leaseTtlSecs"], serde_json::json!(120));
    assert_eq!(
        section["agents"][0]["agentName"],
        serde_json::json!("Claude")
    );
    assert_eq!(
        section["agents"][0]["grants"][0]["dutId"],
        serde_json::json!("dut-1")
    );
    assert_eq!(
        section["agents"][0]["grants"][0]["actions"],
        serde_json::json!(["read", "serialWrite"])
    );
    // Typed -> serialized -> deserialized must remain equal (same criterion as `roundtrip.rs`).
    let re: SettingsFile = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(re, settings);
    assert_eq!(
        re.agent_access.agents[0].token, token,
        "token must be stored verbatim"
    );
}

/// Store-level write/read round-trip with credentials: default section stays omitted and
/// credential section round-trips losslessly.
#[test]
fn settings_file_store_roundtrip_with_agent_access() {
    let dir = std::env::temp_dir().join(format!(
        "duthub-auth-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let token = generate_token();
    let settings = settings_with(
        vec![duthub_core::AgentCredential {
            listener_id: None,
            id: "cred-x".into(),
            agent_name: "X".into(),
            token,
            enabled: false,
            grants: Vec::new(),
        }],
        false,
    );
    duthub_core::store::save_settings_atomic(&dir, &settings).unwrap();
    let loaded = duthub_core::store::load_settings(&dir).unwrap();
    assert_eq!(loaded, settings);
    std::fs::remove_dir_all(&dir).ok();
}

/// End-to-end runtime authentication/authorization matrix: toggle semantics + deny by default.
#[test]
fn runtime_auth_matrix() {
    let token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: false,
        agents: vec![duthub_core::AgentCredential {
            listener_id: None,
            id: "cred-a".into(),
            agent_name: "A".into(),
            token: token.clone(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-1".into(),
                actions: vec![DutAction::Read],
            }],
        }],
        ..Default::default()
    });
    // Auth off: any Bearer token (including invalid ones) resolves to `LocalAdmin`.
    assert_eq!(rt.authenticate(None).unwrap(), Principal::LocalAdmin);
    assert_eq!(
        rt.authenticate(Some("junk")).unwrap(),
        Principal::LocalAdmin
    );
    // Auth on: valid token -> Agent; invalid token -> Unauthorized.
    rt.update(AgentAccessSettings {
        enabled: true,
        agents: rt.settings().agents.clone(),
        ..Default::default()
    });
    let p = rt.authenticate(Some(&token)).unwrap();
    assert!(p.audit_identity().is_some());
    assert!(rt.authenticate(Some(&generate_token())).is_err());
    // Authorization: read-only grant allows `read`, denies `serialWrite` and other DUTs.
    rt.authorize(&p, "dut-1", DutAction::Read).unwrap();
    assert!(rt.authorize(&p, "dut-1", DutAction::SerialWrite).is_err());
    assert!(rt.authorize(&p, "dut-2", DutAction::Read).is_err());
    // `LocalAdmin` is unrestricted.
    rt.authorize(&Principal::LocalAdmin, "dut-9", DutAction::Flash)
        .unwrap();
    // Hot-updating grants takes effect immediately (`AuthSnapshot` swap).
    let mut next = rt.settings();
    next.agents[0].grants.push(DutGrant {
        dut_id: "dut-2".into(),
        actions: vec![DutAction::Read],
    });
    rt.update(next);
    rt.authorize(&p, "dut-2", DutAction::Read).unwrap();
}
