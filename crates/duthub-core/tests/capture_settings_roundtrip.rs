//! serde contract for `tools.capture.idleSecs` and the forward-compatible
//! `DutAction` surface (lane A of `docs/design/CAPTURE_STREAM_BACKENDS.md` §3/§7.1).
//!
//! The existing `roundtrip.rs` / `tools_download_roundtrip.rs` files stay
//! untouched; this file pins the new key, its omission rules, the downgrade
//! behavior of grant actions, and the save→load path.

use duthub_core::SettingsFile;
use duthub_core::agent_access::DutAction;
use duthub_core::model::{CaptureSettings, ToolsSettings};
use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    // Runtime read: `env!` bakes an absolute path that goes stale when the
    // directory is renamed (cargo fingerprints do not include it).
    std::env::var("CARGO_MANIFEST_DIR")
        .map_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
}

/// Minimal settings.json in the same shape as `docs/examples/settings.json`
/// (`tools.adbExecutable` spelled out); `extra` keys are merged into `tools`.
fn settings_json_with_tools(extra: serde_json::Value) -> String {
    let mut root = serde_json::json!({
        "version": 1,
        "appearance": { "theme": "dark", "language": "zh-CN", "platform": "windows" },
        "tools": { "adbExecutable": "adb" }
    });
    if let serde_json::Value::Object(extra) = extra {
        for (key, value) in extra {
            root["tools"][&key] = value;
        }
    }
    serde_json::to_string(&root).unwrap()
}

fn parse(raw: &str) -> SettingsFile {
    serde_json::from_str(raw).expect("settings.json must be valid JSON")
}

#[test]
fn default_capture_settings_have_no_idle() {
    assert_eq!(CaptureSettings::default().idle_secs, None);
    // `store::default_settings()` fills the section with `Default::default()`,
    // so a fresh config directory carries no idleSecs key either.
    assert_eq!(
        duthub_core::store::default_settings().tools.capture,
        CaptureSettings::default()
    );
}

#[test]
fn default_tools_settings_serialize_without_capture_section() {
    // `{}` defaults every field: capture stays empty and the whole section is
    // skipped, so the pre-existing shape is byte-for-byte unchanged.
    let tools: ToolsSettings = serde_json::from_str("{}").unwrap();
    assert_eq!(
        serde_json::to_value(&tools).unwrap(),
        serde_json::json!({ "adbExecutable": "adb" }),
        "default ToolsSettings must not serialize idleSecs or capture keys"
    );
}

#[test]
fn idle_secs_roundtrips_as_camel_case() {
    // Pitfall #15: the key must be camelCase, snake_case silently falls back.
    let raw = settings_json_with_tools(serde_json::json!({
        "capture": { "idleSecs": 90 }
    }));
    let settings = parse(&raw);
    assert_eq!(settings.tools.capture.idle_secs, Some(90));

    let re = serde_json::to_value(&settings).unwrap();
    assert_eq!(re["tools"]["capture"]["idleSecs"], serde_json::json!(90));
    assert!(
        re["tools"]["capture"].get("idle_secs").is_none(),
        "snake_case key must not be serialized: {re}"
    );

    // Typed -> serialize -> parse keeps the same value (the section also carries
    // the pre-existing, always-serialized ffmpeg/ffplay/listenIp keys).
    let again = parse(&serde_json::to_string(&settings).unwrap());
    assert_eq!(again.tools.capture, settings.tools.capture);
}

#[test]
fn idle_secs_null_equals_missing_key() {
    let with_null = parse(&settings_json_with_tools(serde_json::json!({
        "capture": { "idleSecs": null }
    })));
    let without = parse(&settings_json_with_tools(serde_json::json!({})));
    assert_eq!(with_null.tools.capture.idle_secs, None);
    assert_eq!(
        with_null.tools.capture, without.tools.capture,
        "null and missing key must produce the same default"
    );
    let re = serde_json::to_value(&with_null).unwrap();
    assert!(
        re["tools"].get("capture").is_none(),
        "None must not serialize idleSecs or an empty capture section: {re}"
    );
}

#[test]
fn example_settings_have_default_capture() {
    let raw = std::fs::read_to_string(manifest_dir().join("../../docs/examples/settings.json"))
        .expect("read examples/settings.json");
    let settings = parse(&raw);
    assert_eq!(settings.tools.capture, CaptureSettings::default());
}

#[test]
fn save_load_keeps_idle_secs() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = duthub_core::store::load_settings(dir.path()).unwrap();
    settings.tools.capture.idle_secs = Some(90);
    duthub_core::store::save_settings_atomic(dir.path(), &settings).unwrap();

    let loaded = duthub_core::store::load_settings(dir.path()).unwrap();
    assert_eq!(loaded.tools.capture.idle_secs, Some(90));
    assert_eq!(loaded, settings);
}

#[test]
fn newer_action_grants_load_and_are_not_written_back() {
    // `capture` is known to this build; `futureAction` stands in for an action
    // a newer build might add. The latter must not fail the parse and must not
    // survive into the re-serialized document.
    let raw = serde_json::json!({
        "version": 1,
        "appearance": { "theme": "dark", "language": "zh-CN", "platform": "windows" },
        "tools": { "adbExecutable": "adb" },
        "agentAccess": {
            "enabled": true,
            "agents": [{
                "id": "cred-downgrade",
                "agentName": "Agent",
                "token": "dth-abcdef",
                "grants": [{ "dutId": "dut-1", "actions": ["read", "capture", "futureAction"] }]
            }]
        }
    });
    let settings: SettingsFile = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(
        settings.agent_access.agents[0].grants[0].actions,
        vec![DutAction::Read, DutAction::Capture]
    );

    let re = serde_json::to_value(&settings).unwrap();
    assert_eq!(
        re["agentAccess"]["agents"][0]["grants"][0]["actions"],
        serde_json::json!(["read", "capture"]),
        "unknown actions must be filtered out on load and never written back"
    );
    assert!(
        !serde_json::to_string(&settings)
            .unwrap()
            .contains("futureAction"),
        "serialized output must not contain unknown actions"
    );

    // The load path (not just serde) must accept the file instead of treating
    // it as corrupt and renaming it aside (pitfall: a downgrade would lose the
    // whole configuration).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), raw.to_string()).unwrap();
    let loaded =
        duthub_core::store::load_settings(dir.path()).expect("newer configuration must load");
    assert_eq!(
        loaded.agent_access.agents[0].grants[0].actions,
        vec![DutAction::Read, DutAction::Capture]
    );
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("corrupt"))
        .collect();
    assert!(
        backups.is_empty(),
        "newer configuration must not be backed up or discarded: {backups:?}"
    );
}

#[test]
fn capture_action_wire_name_is_camel_case() {
    assert_eq!(
        serde_json::to_value(DutAction::Capture).unwrap(),
        serde_json::json!("capture")
    );
    assert_eq!(DutAction::Capture.as_str(), "capture");
    assert_eq!(DutAction::Unknown.as_str(), "unknown");
    assert_eq!(
        serde_json::from_str::<DutAction>("\"futureAction\"").unwrap(),
        DutAction::Unknown
    );
}
