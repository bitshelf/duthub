//! v1 schema round-trip tests: real examples must deserialize, re-serialize, and stay semantically
//! equal without loss.

use duthub_core::{DutsFile, SettingsFile};
use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    // Read at runtime (injected by cargo): `env!()` bakes an absolute path at compile time,
    // which goes stale if the project directory is renamed (cargo fingerprints exclude paths).
    std::env::var("CARGO_MANIFEST_DIR")
        .map_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
}

fn examples_dir() -> PathBuf {
    manifest_dir().join("../../docs/examples")
}

fn assert_roundtrip<T: serde::Serialize + serde::de::DeserializeOwned>(raw: &str, name: &str) {
    let parsed: T =
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("Failed to deserialize {name}: {e}"));
    let re = serde_json::to_value(&parsed).expect("serialization failed");
    let original: serde_json::Value =
        serde_json::from_str(raw).expect("original JSON should be valid");
    assert_eq!(
        original, re,
        "{name} was not semantically equal after round-trip"
    );
}

#[test]
fn settings_example_roundtrip() {
    let raw =
        std::fs::read_to_string(examples_dir().join("settings.json")).expect("read settings.json");
    assert_roundtrip::<SettingsFile>(&raw, "settings.json");
}

#[test]
fn duts_example_roundtrip() {
    let raw = std::fs::read_to_string(examples_dir().join("duts.json")).expect("read duts.json");
    assert_roundtrip::<DutsFile>(&raw, "duts.json");
}

#[test]
fn relay_variants_parse() {
    // `disabled` and `snmp_pdu` are not in examples; test their `DATA_MODEL.md` shapes directly.
    let disabled: duthub_core::model::DutConfig = serde_json::from_str(
        r#"{"id":"d2","name":"X","serial":{"host":"local","port":"COM3","baud":115200,"format":"8N1"},"relay":{"mode":"disabled"}}"#,
    )
    .expect("disabled relay");
    assert_eq!(disabled.relay, duthub_core::model::RelayConfig::Disabled);

    let pdu: duthub_core::model::DutConfig = serde_json::from_str(
        r#"{"id":"d3","name":"Y","serial":{"host":"local","port":"COM3","baud":115200,"format":"8N1"},
            "relay":{"mode":"snmp_pdu","snmp":{"address":"10.0.0.9","version":"3","credentialRef":"win-cred: pdu-9"},"outlets":{"power":2}}}"#,
    )
    .expect("snmp_pdu relay");
    match &pdu.relay {
        duthub_core::model::RelayConfig::SnmpPdu { snmp, outlets } => {
            assert_eq!(snmp.credential_ref, "win-cred: pdu-9");
            assert_eq!(outlets.power, 2);
        }
        other => panic!("Expected SnmpPdu, got {other:?}"),
    }
}

#[test]
fn mcp_stdio_variant_parse() {
    let dut: duthub_core::model::DutConfig = serde_json::from_str(
        r#"{"id":"d4","name":"Z","serial":{"host":"local","port":"COM3","baud":115200,"format":"8N1"},
            "mcp":{"transport":"stdio","command":"duthub-mcp","args":["--stdio"],"cwd":"","env":{"RUST_LOG":"warn"}}}"#,
    )
    .expect("stdio transport");
    match &dut.mcp {
        duthub_core::model::McpConfig::Stdio { command, env, .. } => {
            assert_eq!(command, "duthub-mcp");
            assert_eq!(env.get("RUST_LOG").map(String::as_str), Some("warn"));
        }
        other @ duthub_core::model::McpConfig::StreamableHttp { .. } => {
            panic!("Expected Stdio, got {other:?}")
        }
    }
}

#[test]
fn unknown_fields_tolerated() {
    // Forward compatibility: v2 fields in a v1 file must not fail deserialization.
    let raw = r#"{"version":1,"appearance":{"theme":"dark","language":"zh-CN","platform":"windows","futureField":1}}"#;
    let parsed: SettingsFile =
        serde_json::from_str(raw).expect("unknown fields should be tolerated");
    assert_eq!(parsed.appearance.theme, "dark");
}

#[test]
fn duts_store_roundtrip() {
    use duthub_core::store;

    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    // Missing -> None
    assert!(store::load_duts(dir).unwrap().is_none());
    // Save -> load equivalence
    let raw = std::fs::read_to_string(examples_dir().join("duts.json")).unwrap();
    let duts: duthub_core::DutsFile = serde_json::from_str(&raw).unwrap();
    store::save_duts_atomic(dir, &duts).unwrap();
    let loaded = store::load_duts(dir).unwrap().unwrap();
    assert_eq!(loaded, duts);
}

#[test]
fn a2a_section_tolerated_and_defaulted() {
    let parsed: duthub_core::SettingsFile = serde_json::from_str(
        r#"{"version":1,"appearance":{"theme":"dark","language":"zh-CN","platform":"windows"},
            "a2a":{"agentName":"Lab Agent","port":9000,"peers":[{"id":"p1","name":"Peer","cardUrl":"http://127.0.0.1:9001/.well-known/agent-card.json"}]}}"#,
    )
    .unwrap();
    let a2a = parsed.a2a.unwrap();
    assert_eq!(a2a.agent_name, "Lab Agent");
    assert_eq!(a2a.peers.len(), 1);
}
