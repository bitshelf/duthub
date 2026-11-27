//! serde contract tests for the `tools.download` section (separate file; the
//! existing roundtrip.rs tests stay untouched).
//!
//! Three things are locked down: (1) an old settings.json without the section
//! deserializes to the default (env proxy); (2) default-valued keys/sections
//! are omitted on serialize (pits #19/#24; `adbExecutable` is a pre-existing
//! exception — no skip, always serialized, docs/examples/settings.json keeps
//! writing it); (3) keys are camelCase only (pit #15: `snake_case` keys silently
//! fall back to defaults, so this file uses camelCase exclusively).

use duthub_core::SettingsFile;
use duthub_core::model::{
    DownloadProxyMode, DownloadProxySettings, ToolDownloadSettings, ToolsSettings,
};

/// Minimal settings.json with `"adbExecutable":"adb"` spelled out in the tools
/// section (matching the existing shape of docs/examples/settings.json);
/// `extra` keys are merged into tools one by one.
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

#[test]
fn missing_section_defaults_to_env_proxy() {
    let settings: SettingsFile =
        serde_json::from_str(&settings_json_with_tools(serde_json::json!({}))).unwrap();
    assert_eq!(settings.tools.download, ToolDownloadSettings::default());
    assert_eq!(settings.tools.download.proxy.mode, DownloadProxyMode::Env);
    assert_eq!(settings.tools.download.proxy.url, "");
}

#[test]
fn default_tools_settings_serialize_without_download_section() {
    // `{}` deserializes to all-default serde fields (adb="adb", hdc="hdc",
    // capture/download defaulted) → re-serializing may only emit the
    // pre-existing `adbExecutable` key; download/hdc/capture stay omitted.
    let tools: ToolsSettings = serde_json::from_str("{}").unwrap();
    let value = serde_json::to_value(&tools).unwrap();
    assert_eq!(
        value,
        serde_json::json!({ "adbExecutable": "adb" }),
        "default ToolsSettings must not serialize any download, hdc, or capture keys"
    );
}

#[test]
fn custom_proxy_roundtrips_losslessly() {
    let raw = settings_json_with_tools(serde_json::json!({
        "download": { "proxy": { "mode": "custom", "url": "socks5://127.0.0.1:1080" } }
    }));
    let settings: SettingsFile = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        settings.tools.download.proxy.mode,
        DownloadProxyMode::Custom
    );
    assert_eq!(settings.tools.download.proxy.url, "socks5://127.0.0.1:1080");
    // Typed -> re-serialized -> semantically equal. Pre-existing sections
    // (agent/commands/flashTools/terminal) have no section-level skip and are
    // always emitted (roundtrip.rs compares whole documents built from
    // examples/settings.json), so this file scopes its check to the tools
    // subtree owned by this feature.
    let re = serde_json::to_value(&settings).unwrap();
    let original: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        original["tools"], re["tools"],
        "custom proxy section must round-trip losslessly"
    );
}

#[test]
fn off_mode_roundtrips_losslessly() {
    let raw = settings_json_with_tools(serde_json::json!({
        "download": { "proxy": { "mode": "off" } }
    }));
    let settings: SettingsFile = serde_json::from_str(&raw).unwrap();
    assert_eq!(settings.tools.download.proxy.mode, DownloadProxyMode::Off);
    assert_eq!(settings.tools.download.proxy.url, "");
    let re = serde_json::to_value(&settings).unwrap();
    let original: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        original["tools"], re["tools"],
        "non-default mode=off must round-trip losslessly with empty url omitted"
    );
}

#[test]
fn partial_proxy_defaults_mode_and_omits_it_on_serialize() {
    let raw = settings_json_with_tools(serde_json::json!({
        "download": { "proxy": { "url": "http://10.0.0.2:8080" } }
    }));
    let settings: SettingsFile = serde_json::from_str(&raw).unwrap();
    assert_eq!(settings.tools.download.proxy.mode, DownloadProxyMode::Env);
    assert_eq!(settings.tools.download.proxy.url, "http://10.0.0.2:8080");
    let re = serde_json::to_value(&settings).unwrap();
    assert_eq!(
        re["tools"]["download"],
        serde_json::json!({ "proxy": { "url": "http://10.0.0.2:8080" } }),
        "default mode=env must be omitted, leaving only url"
    );
}

#[test]
fn default_proxy_settings_are_fully_skipped() {
    let value = serde_json::to_value(DownloadProxySettings::default()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({}),
        "default proxy section must be omitted completely"
    );
}
