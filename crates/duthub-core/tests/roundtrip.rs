//! v1 schema 往返测试：examples 里的真实样例必须能无损地 类型化 → 再序列化 → 语义相等。

use duthub_core::{DutsFile, SettingsFile};
use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    // 运行时读取（cargo 注入）：env!() 在编译期烘焙绝对路径，
    // 项目目录改名后旧二进制仍指向旧路径（cargo 指纹不含绝对路径）。
    std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn examples_dir() -> PathBuf {
    manifest_dir().join("../../docs/examples")
}

fn assert_roundtrip<T: serde::Serialize + serde::de::DeserializeOwned>(raw: &str, name: &str) {
    let parsed: T =
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("{name} 反序列化失败: {e}"));
    let re = serde_json::to_value(&parsed).expect("序列化失败");
    let original: serde_json::Value = serde_json::from_str(raw).expect("原始 JSON 应有效");
    assert_eq!(original, re, "{name} 往返后语义不相等");
}

#[test]
fn settings_example_roundtrip() {
    let raw = std::fs::read_to_string(examples_dir().join("settings.json")).expect("读文件");
    assert_roundtrip::<SettingsFile>(&raw, "settings.json");
}

#[test]
fn duts_example_roundtrip() {
    let raw = std::fs::read_to_string(examples_dir().join("duts.json")).expect("读文件");
    assert_roundtrip::<DutsFile>(&raw, "duts.json");
}

#[test]
fn relay_variants_parse() {
    // disabled / snmp_pdu 不在 examples 里，用 DATA_MODEL.md 的形状补测
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
        other => panic!("期望 SnmpPdu，得到 {other:?}"),
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
        other => panic!("期望 Stdio，得到 {other:?}"),
    }
}

#[test]
fn unknown_fields_tolerated() {
    // 向前兼容：v2 新字段出现在 v1 文件里不应报错
    let raw = r#"{"version":1,"appearance":{"theme":"dark","language":"zh-CN","platform":"windows","futureField":1}}"#;
    let parsed: SettingsFile = serde_json::from_str(raw).expect("未知字段应被容忍");
    assert_eq!(parsed.appearance.theme, "dark");
}

#[test]
fn duts_store_roundtrip() {
    use duthub_core::store;

    let dir = manifest_dir().join("target/test-tmp/duts-store");
    let _ = std::fs::remove_dir_all(&dir);
    // 缺失 → None
    assert!(store::load_duts(&dir).unwrap().is_none());
    // 保存 → 加载等价
    let raw = std::fs::read_to_string(examples_dir().join("duts.json")).unwrap();
    let duts: duthub_core::DutsFile = serde_json::from_str(&raw).unwrap();
    store::save_duts_atomic(&dir, &duts).unwrap();
    let loaded = store::load_duts(&dir).unwrap().unwrap();
    assert_eq!(loaded, duts);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a2a_section_tolerated_and_defaulted() {
    let parsed: duthub_core::SettingsFile = serde_json::from_str(
        r#"{"version":1,"appearance":{"theme":"dark","language":"zh-CN","platform":"windows"},
            "a2a":{"agentName":"Lab Agent","port":9000,"host":"127.0.0.1","peers":[{"id":"p1","name":"Peer","cardUrl":"http://127.0.0.1:9001/.well-known/agent-card.json"}]}}"#,
    )
    .unwrap();
    let a2a = parsed.a2a.unwrap();
    assert_eq!(a2a.agent_name, "Lab Agent");
    assert_eq!(a2a.peers.len(), 1);
}
