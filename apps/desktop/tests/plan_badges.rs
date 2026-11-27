//! W3 acceptance: three-line boundary badges on the Agent page and retirement of the legacy
//! single-line "Plan · Read-only" copy.
//!
//! UI lives in the bin target (workspace/pages are bin modules), so this test verifies the
//! **render contract** against source files and locale resources (paths read at runtime):
//! - Both locales carry all three badge strings and no longer contain `agentPlanMode`;
//! - The demo panel keeps the three-line badges, Agent settings hide static notes, and Inspector
//!   shows actual session isolation status;
//! - Agent settings mount `agent_runtime_panel`;
//! - Neither the zh-CN nor en-US legacy single-line "Plan · Read-only" copy may reappear in any
//!   source file.

use std::path::PathBuf;

use serde_json::Value;

const BADGES: [&str; 3] = [
    "agentToolSurface",
    "agentLocalTools",
    "agentBindingBoundary",
];

fn manifest_dir() -> PathBuf {
    // Read at runtime instead of env!.
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
}

fn locale(name: &str) -> Value {
    let path = manifest_dir().join("locales").join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn source(name: &str) -> String {
    let src_root = manifest_dir().join("src");
    let path = src_root.join(name);
    if path.is_file() {
        return std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    }
    if let Some(stem) = name.strip_suffix(".rs") {
        let dir = src_root.join(stem);
        if dir.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "rs"))
                .collect();
            files.sort_by(|a, b| {
                let a_mod = a.file_name().is_some_and(|n| n == "mod.rs");
                let b_mod = b.file_name().is_some_and(|n| n == "mod.rs");
                b_mod.cmp(&a_mod).then_with(|| a.cmp(b))
            });
            return files
                .into_iter()
                .map(|p| {
                    std::fs::read_to_string(&p)
                        .unwrap_or_else(|error| panic!("read {}: {error}", p.display()))
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    panic!("missing source {name}");
}

fn text_at<'a>(locale: &'a Value, key: &str) -> &'a str {
    locale
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("locale missing non-empty text for {key}"))
}

#[test]
fn both_locales_carry_the_three_badges_and_retire_plan_readonly() {
    for name in ["zh-CN.json", "en-US.json"] {
        let locale = locale(name);
        let object = locale
            .as_object()
            .unwrap_or_else(|| panic!("{name} top-level JSON must be an object"));
        assert!(
            !object.contains_key("agentPlanMode"),
            "{name} must not retain legacy agentPlanMode single-line copy"
        );
        for key in BADGES {
            let value = text_at(&locale, key);
            assert!(!value.trim().is_empty(), "{name}: {key}");
        }
    }

    let zh = locale("zh-CN.json");
    assert!(
        text_at(&zh, "agentToolSurface").contains("13 个 Plan 工具（只读）"),
        "zh-CN tool-surface badge must state 13 Plan tools (read-only)"
    );
    assert!(
        text_at(&zh, "agentLocalTools").contains("Goose 扩展已禁用")
            && text_at(&zh, "agentLocalTools").contains("OS 沙箱（MXC）已启用"),
        "zh-CN local-tools badge must state Goose extensions disabled and MXC OS sandbox enabled"
    );
    assert!(
        text_at(&zh, "agentBindingBoundary").contains("不是安全边界"),
        "zh-CN binding badge must state it is not a security boundary"
    );

    let en = locale("en-US.json");
    assert!(
        text_at(&en, "agentToolSurface").contains("13 Plan tools"),
        "English tool-surface badge must name the 13 Plan tools"
    );
    assert!(
        text_at(&en, "agentLocalTools").contains("Goose extensions disabled")
            && text_at(&en, "agentLocalTools").contains("OS sandbox (MXC) enabled"),
        "English local-tools badge must state extensions disabled and MXC OS sandbox enabled"
    );
    assert!(
        text_at(&en, "agentBindingBoundary").contains("not a security boundary"),
        "English binding badge must state it is not a security boundary"
    );
}

#[test]
fn demo_keeps_badges_and_agent_settings_keep_runtime_status() {
    let provider_models = source("provider_models.rs");
    assert!(
        provider_models.contains("agent_runtime_panel"),
        "Agent settings page must mount agent_runtime_panel (goose runtime)"
    );
    let pages = source("pages.rs");
    let runtime = pages
        .split_once("fn agent_runtime_panel")
        .expect("runtime panel")
        .1
        .split_once("fn agent_registry_section")
        .expect("registry section")
        .0;
    for key in BADGES {
        assert!(
            !runtime.contains(key),
            "runtime settings hide static badge {key}"
        );
    }
    assert!(runtime.contains("builtin_goose_status_line(&goose_state)"));
    let inspector = pages
        .split_once("fn agent_inspector_panel")
        .expect("session inspector")
        .1;
    assert!(inspector.contains("format_sandbox_status(&status)"));
}

#[test]
fn plan_readonly_single_line_must_not_come_back() {
    for file in ["agent.rs", "pages.rs", "provider_models.rs", "workspace.rs"] {
        let text = source(file);
        assert!(
            !text.contains("agentPlanMode"),
            "{file} must not reference legacy agentPlanMode copy"
        );
        assert!(
            !text.contains("Plan · 只读") && !text.contains("Plan · Read-only"),
            "{file} must not reintroduce legacy Plan read-only single-line copy"
        );
    }
}

/// Wiring evidence for F1/T16/F12 (bin modules cannot be instantiated in lib tests; asserted via
/// source contract).
#[test]
fn agent_runtime_lifecycle_is_wired_into_the_workbench() {
    let workspace = source("workspace.rs");
    // Non-blocking stop: definition + close tab + delete DUT all send stop requests only.
    assert!(
        workspace.matches("stop_agent_session").count() >= 3,
        "stop_agent_session must cover definition, close tab, and delete DUT paths"
    );
    // Drop hook: stop all agent sessions and close MCP listener handles.
    assert!(workspace.contains("impl Drop for Workbench"));
    assert!(workspace.contains("mcp_runtime.shutdown()"));
    // --self-test: host is not constructed.
    assert!(workspace.contains("agent_self_test: self_test"));
    assert!(workspace.contains("agent_host: None"));
    // F1/T16: spawn_mcp_runtime return value is stored on Workbench (closed on Drop).
    assert!(
        workspace.contains("wb.mcp_runtime = crate::mcp_runtime::spawn_mcp_runtime("),
        "listener handles from spawn_mcp_runtime must be stored"
    );

    let runtime = source("mcp_runtime.rs");
    assert!(runtime.contains("pub fn spawn_mcp_runtime("));
    assert!(runtime.contains("-> McpRuntimeHandles"));
    assert!(
        runtime.contains("duthub_mcp::transport::spawn_http_listener_on("),
        "main and per-DUT listeners must obtain handles via spawn_http_listener_on"
    );
    assert!(runtime.contains("shutdown_and_join_with_timeout"));
}
