//! W3 验收：Agent 页三行边界徽标，以及旧「Plan · 只读」单行的退役。
//!
//! UI 不在 lib target 内（workspace/pages 是 bin 模块），因此本测试按源码与
//! locale 资源做**渲染契约**断言（坑 #4：路径运行期读取）：
//! - 两份 locale 都带三行徽标文案，且不再保留 `agentPlanMode`；
//! - 演示面板保留三行徽标，Agent 设置页不显示静态说明，Inspector 显示会话实际隔离状态；
//! - Agent 页确实挂载 `agent_runtime_panel`；
//! - 「Plan · 只读 / Plan · Read-only」字样不得回到任何源文件。

use std::path::PathBuf;

use serde_json::Value;

const BADGES: [&str; 3] = [
    "agentToolSurface",
    "agentLocalTools",
    "agentBindingBoundary",
];

fn manifest_dir() -> PathBuf {
    // 坑 #4：运行期读取，不能 env!。
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
        .unwrap_or_else(|| panic!("locale 缺少非空文案 {key}"))
}

#[test]
fn both_locales_carry_the_three_badges_and_retire_plan_readonly() {
    for name in ["zh-CN.json", "en-US.json"] {
        let locale = locale(name);
        let object = locale
            .as_object()
            .unwrap_or_else(|| panic!("{name} 顶层必须是对象"));
        assert!(
            !object.contains_key("agentPlanMode"),
            "{name} 不得再保留旧「Plan · 只读」单行文案"
        );
        for key in BADGES {
            let value = text_at(&locale, key);
            assert!(!value.trim().is_empty(), "{name}: {key}");
        }
    }

    let zh = locale("zh-CN.json");
    assert!(
        text_at(&zh, "agentToolSurface").contains("13 个 Plan 工具（只读）"),
        "中文工具面徽标必须写明 13 个 Plan 工具（只读）"
    );
    assert!(
        text_at(&zh, "agentLocalTools").contains("Goose 扩展已禁用")
            && text_at(&zh, "agentLocalTools").contains("OS 沙箱（MXC）已启用"),
        "中文本机工具徽标必须写明扩展禁用与 MXC OS 沙箱已启用"
    );
    assert!(
        text_at(&zh, "agentBindingBoundary").contains("不是安全边界"),
        "中文绑定徽标必须写明不是安全边界"
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
        "Agent 设置页必须挂载 agent_runtime_panel（goose 运行时）"
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
            "{file} 不得再引用旧 agentPlanMode 文案"
        );
        assert!(
            !text.contains("Plan · 只读") && !text.contains("Plan · Read-only"),
            "{file} 不得再出现「Plan · 只读」单行"
        );
    }
}

/// F1/T16/F12 的接线证据（bin 模块无法在 lib test 里实例化，按源码契约断言）。
#[test]
fn agent_runtime_lifecycle_is_wired_into_the_workbench() {
    let workspace = source("workspace.rs");
    // 非阻塞停止：定义 + 关 tab + 删除 DUT 三个入口都只发停止请求。
    assert!(
        workspace.matches("stop_agent_session").count() >= 3,
        "stop_agent_session 必须覆盖定义、关 tab、删除 DUT 三条路径"
    );
    // 退出钩子：停全部 Agent 会话 + 关闭 MCP 监听器句柄。
    assert!(workspace.contains("impl Drop for Workbench"));
    assert!(workspace.contains("mcp_runtime.shutdown()"));
    // --self-test：宿主不构造。
    assert!(workspace.contains("agent_self_test: self_test"));
    assert!(workspace.contains("agent_host: None"));
    // F1/T16：spawn_mcp_runtime 的返回值保存在 Workbench（Drop 时关闭）。
    assert!(
        workspace.contains("wb.mcp_runtime = crate::mcp_runtime::spawn_mcp_runtime("),
        "spawn_mcp_runtime 的 listener 句柄必须保存"
    );

    let runtime = source("mcp_runtime.rs");
    assert!(runtime.contains("pub fn spawn_mcp_runtime("));
    assert!(runtime.contains("-> McpRuntimeHandles"));
    assert!(
        runtime.contains("duthub_mcp::transport::spawn_http_listener_on("),
        "主/每 DUT listener 必须经 spawn_http_listener_on 取得句柄"
    );
    assert!(runtime.contains("shutdown_and_join_with_timeout"));
}
