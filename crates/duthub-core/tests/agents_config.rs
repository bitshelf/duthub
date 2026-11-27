//! `agents.json` / `agent-bindings.json` 持久化回归。
//!
//! 覆盖：缺失生成默认 goose、往返、损坏备份 + fail closed、只存 env 键名、
//! 每 DUT 绑定默认 goose 与损坏 fail closed。

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
    let goose = file.goose().expect("默认必须有 goose");
    assert_eq!(goose.kind, AgentKind::Acp);
    assert_eq!(goose.program, None, "program=null 表示内嵌");
    assert!(goose.enabled);
    assert!(
        agents_path(&dir).is_file(),
        "缺失时必须生成 {AGENTS_FILE_NAME}"
    );

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(agents_path(&dir)).unwrap()).unwrap();
    assert_eq!(raw["version"], 1);
    assert_eq!(raw["agents"][0]["id"], DEFAULT_AGENT_ID);
    assert!(
        raw["agents"][0]["program"].is_null(),
        "内嵌 goose 必须写 null"
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

    // 缺失值 fail closed，不把空串当已配置。
    assert!(load_agent_env_with(&agent, |_| Ok(None)).is_err());
    // 非法键名在持久化层就拒绝。
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
    assert!(!agents_path(&dir).exists(), "损坏文件必须被移走");
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
    assert_eq!(backups, 1, "必须保留唯一损坏备份");
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
        "语义无效的文件不删除，便于用户修复"
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

    // 删除 DUT 只移除自己的绑定。
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
