//! Persistence regression tests for `agents.json` and `agent-bindings.json`.
//!
//! Covers: generating default `goose` when missing, round-trip, corrupt backup + fail-closed,
//! storing env key names only, per-DUT default `goose` binding, and corrupt bindings fail-closed.

use std::path::PathBuf;

use duthub_core::agent_config::{
    AGENT_BINDINGS_FILE_NAME, AGENTS_FILE_NAME, AgentBindingsFile, AgentConfigError,
    AgentDefinition, AgentKind, AgentsFile, DEFAULT_AGENT_ID, agent_bindings_path, agent_env_entry,
    agents_path, default_agents, load_agent_bindings, load_agent_env_with, load_agents,
    save_agent_bindings_atomic, save_agents_atomic,
};

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("duthub-agents-test-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn missing_file_generates_versioned_default_goose() {
    let dir = temp_dir("default");
    assert!(!agents_path(&dir).exists());
    let file = load_agents(&dir).unwrap();
    assert_eq!(file, default_agents());
    assert_eq!(file.version, 1);
    let goose = file.goose().expect("default registry must contain goose");
    assert_eq!(goose.kind, AgentKind::Acp);
    assert_eq!(goose.program, None, "program=null indicates embedded goose");
    assert!(goose.enabled);
    assert!(
        agents_path(&dir).is_file(),
        "{AGENTS_FILE_NAME} must be created when missing"
    );

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(agents_path(&dir)).unwrap()).unwrap();
    assert_eq!(raw["version"], 1);
    assert_eq!(raw["agents"][0]["id"], DEFAULT_AGENT_ID);
    assert!(
        raw["agents"][0]["program"].is_null(),
        "embedded goose must serialize program as null"
    );
    assert!(raw["agents"][0]["expectedSha256"].is_null());
    assert_eq!(raw["agents"][0]["kind"], "acp");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn roundtrip_keeps_schema_and_env_keys_only_hold_names() {
    let dir = temp_dir("roundtrip");
    let mut file = default_agents();
    file.agents.push(AgentDefinition {
        id: "acme".into(),
        name: "Acme ACP".into(),
        kind: AgentKind::Acp,
        program: Some("C:/tools/acme.exe".into()),
        args: vec!["--acp".into()],
        env_keys: vec!["ACME_API_KEY".into()],
        cli_profile: None,
        enabled: true,
        auth_mode: "inherit".into(),
        trust_level: "mcp_only".into(),
        expected_sha256: Some("b".repeat(64)),
    });
    file.agents.push(AgentDefinition {
        id: "claude".into(),
        name: "Claude Code".into(),
        kind: AgentKind::Cli,
        program: None,
        args: Vec::new(),
        env_keys: Vec::new(),
        cli_profile: Some("claude-code".into()),
        enabled: false,
        auth_mode: "api_key".into(),
        trust_level: "mcp_only".into(),
        expected_sha256: None,
    });
    save_agents_atomic(&dir, &file).unwrap();

    let raw = std::fs::read_to_string(agents_path(&dir)).unwrap();
    assert!(raw.contains("\"envKeys\""), "{raw}");
    assert!(raw.contains("\"cliProfile\""), "{raw}");
    assert!(raw.contains("\"expectedSha256\""), "{raw}");
    assert!(!raw.contains("sk-should-not-be-stored"), "{raw}");
    assert!(!raw.contains("\"token\""), "{raw}");
    assert!(!raw.contains("\"apiKey\""), "{raw}");
    assert_eq!(load_agents(&dir).unwrap(), file);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn env_values_are_read_from_keyring_entries_not_the_file() {
    let mut agent = AgentDefinition::goose();
    agent.env_keys = vec!["ACME_API_KEY".into()];
    assert_eq!(
        agent_env_entry(&agent.id, "ACME_API_KEY"),
        "acp/goose/env/ACME_API_KEY"
    );
    let values = load_agent_env_with(&agent, |entry| {
        assert_eq!(entry, "acp/goose/env/ACME_API_KEY");
        Ok(Some("value-from-keyring".into()))
    })
    .unwrap();
    assert_eq!(
        values,
        [("ACME_API_KEY".to_owned(), "value-from-keyring".into())]
    );

    // Missing values fail closed rather than treating empty strings as configured.
    assert!(load_agent_env_with(&agent, |_| Ok(None)).is_err());
    // Invalid key names are rejected at the persistence layer.
    let mut bad = AgentDefinition::goose();
    bad.env_keys = vec!["NOT-VALID".into()];
    let bad_dir = temp_dir("bad-key");
    assert!(
        save_agents_atomic(
            &bad_dir,
            &AgentsFile {
                version: 1,
                agents: vec![bad],
            }
        )
        .is_err()
    );
    let _ = std::fs::remove_dir_all(&bad_dir);
}

#[test]
fn corrupt_file_is_backed_up_and_load_fails_closed() {
    let dir = temp_dir("corrupt");
    std::fs::write(agents_path(&dir), "{ definitely not json").unwrap();
    let error = load_agents(&dir).unwrap_err();
    assert!(matches!(error, AgentConfigError::Parse(_)), "{error}");
    assert!(
        !agents_path(&dir).exists(),
        "corrupt file must be moved aside"
    );
    let backups = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("agents.corrupt-")
        })
        .count();
    assert_eq!(backups, 1, "exactly one corrupt backup must be kept");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_schema_fails_closed_without_silent_default() {
    let dir = temp_dir("invalid");
    let raw = serde_json::json!({
        "version": 99,
        "agents": [{ "id": "goose", "name": "goose", "kind": "acp" }]
    });
    std::fs::write(agents_path(&dir), raw.to_string()).unwrap();
    let error = load_agents(&dir).unwrap_err();
    assert!(matches!(error, AgentConfigError::Invalid(_)), "{error}");
    assert!(
        agents_path(&dir).exists(),
        "semantically invalid files must stay on disk so the user can fix them"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_dut_bindings_default_to_goose_and_roundtrip() {
    let dir = temp_dir("bindings");
    let mut bindings = load_agent_bindings(&dir).unwrap();
    assert_eq!(bindings.agent_id_for("dut-a"), DEFAULT_AGENT_ID);
    bindings.set_agent_id("dut-a", "acme");
    save_agent_bindings_atomic(&dir, &bindings).unwrap();
    assert!(agent_bindings_path(&dir).is_file());
    assert!(
        agent_bindings_path(&dir)
            .file_name()
            .is_some_and(|name| name == AGENT_BINDINGS_FILE_NAME)
    );

    let loaded = load_agent_bindings(&dir).unwrap();
    assert_eq!(loaded.agent_id_for("dut-a"), "acme");
    assert_eq!(loaded.agent_id_for("dut-b"), DEFAULT_AGENT_ID);
    assert_eq!(loaded, bindings);

    // Removing a DUT removes only its own binding.
    let mut pruned = loaded;
    pruned.remove_dut("dut-a");
    assert_eq!(pruned, AgentBindingsFile::default());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_bindings_fail_closed_with_backup() {
    let dir = temp_dir("bindings-corrupt");
    std::fs::write(agent_bindings_path(&dir), "not json").unwrap();
    assert!(load_agent_bindings(&dir).is_err());
    let backups = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("agent-bindings.corrupt-")
        })
        .count();
    assert_eq!(backups, 1);
    let _ = std::fs::remove_dir_all(&dir);
}
