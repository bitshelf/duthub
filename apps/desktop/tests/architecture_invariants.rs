//! Workspace-wide architecture, safety, and global i18n source-contract invariants.
//!
//! Guards key structural decisions from Waves 0–6:
//! 1. `duthub-core` and `duthub-agent` forbid `unsafe` code (`#![forbid(unsafe_code)]`).
//! 2. `duthub-core` stays pure (no `windows-sys` or `reqwest` dependencies).
//! 3. `PLAN_TOOL_NAMES` has a single source of truth shared between `duthub-mcp` and `cli.rs`.
//! 4. Desktop UI rendering files contain zero hardcoded Chinese UI string literals
//!    outside comments, `#[cfg(test)]`, and serial-driver protocol markers.
//! 5. Both locale catalogs (`zh-CN.json` and `en-US.json`) have identical keys and no empty values.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir()
        .parent()
        .and_then(Path::parent)
        .expect("apps/desktop sits two levels below repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn core_and_agent_crates_forbid_unsafe_and_keep_core_pure() {
    let root = repo_root();
    let core_lib = read(&root.join("crates/duthub-core/src/lib.rs"));
    assert!(
        core_lib.contains("#![forbid(unsafe_code)]"),
        "duthub-core must declare #![forbid(unsafe_code)]"
    );
    let agent_lib = read(&root.join("crates/duthub-agent/src/lib.rs"));
    assert!(
        agent_lib.contains("#![forbid(unsafe_code)]"),
        "duthub-agent must declare #![forbid(unsafe_code)]"
    );
    let sandbox_lib = read(&root.join("crates/duthub-sandbox/src/lib.rs"));
    assert!(
        sandbox_lib.contains("#![forbid(unsafe_code)]"),
        "duthub-sandbox must declare #![forbid(unsafe_code)]"
    );

    let core_toml = read(&root.join("crates/duthub-core/Cargo.toml"));
    assert!(
        !core_toml.contains("windows-sys"),
        "duthub-core must not depend on windows-sys"
    );
    assert!(
        !core_toml.contains("reqwest"),
        "duthub-core must not depend on reqwest (LLM HTTP probes live in duthub-agent)"
    );
    let sandbox_toml = read(&root.join("crates/duthub-sandbox/Cargo.toml"));
    assert!(
        !sandbox_toml.contains("windows-sys"),
        "duthub-sandbox must use mxc-sdk instead of raw windows-sys FFI"
    );
    assert!(
        sandbox_toml.contains("mxc-sdk"),
        "duthub-sandbox must depend on mxc-sdk"
    );
}

#[test]
fn plan_tool_names_have_a_single_source_of_truth() {
    let cli = read(&manifest_dir().join("src/agent_host/cli.rs"));
    assert!(
        cli.contains("duthub_mcp::PLAN_TOOL_NAMES"),
        "cli.rs must alias PLAN_TOOLS to duthub_mcp::PLAN_TOOL_NAMES"
    );
    assert_eq!(
        duthub_desktop::agent_host::cli::PLAN_TOOLS,
        duthub_mcp::PLAN_TOOL_NAMES
    );
    assert_eq!(duthub_mcp::PLAN_TOOL_NAMES.len(), 11);
    assert!(!duthub_mcp::PLAN_TOOL_NAMES.contains(&"notify_user"));
    assert!(!duthub_mcp::PLAN_TOOL_NAMES.contains(&"dut_control"));
    assert!(!duthub_mcp::PLAN_TOOL_NAMES.contains(&"serial_exec"));
}

#[test]
fn locales_have_identical_non_empty_key_sets() {
    let locales_dir = manifest_dir().join("locales");
    let zh: serde_json::Value =
        serde_json::from_str(&read(&locales_dir.join("zh-CN.json"))).expect("parse zh-CN.json");
    let en: serde_json::Value =
        serde_json::from_str(&read(&locales_dir.join("en-US.json"))).expect("parse en-US.json");
    let zh_map = zh.as_object().expect("zh-CN root object");
    let en_map = en.as_object().expect("en-US root object");
    let zh_keys: BTreeSet<&str> = zh_map.keys().map(String::as_str).collect();
    let en_keys: BTreeSet<&str> = en_map.keys().map(String::as_str).collect();
    assert_eq!(
        zh_keys, en_keys,
        "zh-CN.json and en-US.json must stay in 1:1 key parity"
    );
    for (lang, map) in [("zh-CN", zh_map), ("en-US", en_map)] {
        for (key, value) in map {
            let text = value
                .as_str()
                .unwrap_or_else(|| panic!("{lang}: {key} must be a string"));
            assert!(
                !text.trim().is_empty(),
                "{lang}: locale key `{key}` must not be empty"
            );
        }
    }
}

/// Returns `true` if `ch` is a CJK Unified Ideograph.
fn is_cjk(ch: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&ch)
}

/// Scan Rust source lines prior to `#[cfg(test)]` for quoted string literals
/// containing CJK characters, ignoring comments and explicitly allowlisted
/// serial-driver protocol markers.
fn find_hardcoded_cjk_literals(source: &str) -> Vec<(usize, String)> {
    const ALLOWED_PROTOCOL_MARKERS: &[&str] = &[" 已打开", "重连"];
    let mut hits = Vec::new();
    let mut in_block_comment = false;

    for (idx, raw_line) in source.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("#[cfg(test)]") {
            break;
        }
        // Strip single-line comments and skip lines inside block comments.
        let mut line = raw_line;
        if in_block_comment {
            if let Some((_, after)) = line.split_once("*/") {
                in_block_comment = false;
                line = after;
            } else {
                continue;
            }
        }
        if let Some((before, _)) = line.split_once("//") {
            line = before;
        }
        if let Some((before, after)) = line.split_once("/*") {
            if !after.contains("*/") {
                in_block_comment = true;
            }
            line = before;
        }

        // Extract double-quoted string literals on this line.
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '"' {
                continue;
            }
            let mut literal = String::new();
            let mut escaped = false;
            for next in chars.by_ref() {
                if escaped {
                    literal.push(next);
                    escaped = false;
                    continue;
                }
                if next == '\\' {
                    escaped = true;
                    continue;
                }
                if next == '"' {
                    break;
                }
                literal.push(next);
            }
            if literal.chars().any(is_cjk) && !ALLOWED_PROTOCOL_MARKERS.contains(&literal.as_str())
            {
                hits.push((idx + 1, literal));
            }
        }
    }
    hits
}

#[test]
fn desktop_ui_modules_contain_zero_hardcoded_chinese_literals() {
    let src = manifest_dir().join("src");
    let mut files = vec![
        src.join("components.rs"),
        src.join("media.rs"),
        src.join("media_page.rs"),
        src.join("monitor_window.rs"),
        src.join("provider_models.rs"),
        src.join("side_panel.rs"),
        src.join("tool_ui.rs"),
        src.join("tray_ui.rs"),
    ];
    for subdir in ["pages", "workspace"] {
        let dir = src.join(subdir);
        for entry in std::fs::read_dir(&dir).expect("read ui submodule dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }

    let mut offenders = Vec::new();
    for path in files {
        let content = read(&path);
        for (line_no, literal) in find_hardcoded_cjk_literals(&content) {
            offenders.push(format!(
                "{}:{line_no}: {literal:?}",
                path.strip_prefix(&src).unwrap_or(&path).display()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "Hardcoded Chinese UI string literals found (use crate::i18n::label/message instead):\n{}",
        offenders.join("\n")
    );
}
