//! duthub-mcp stdio 入口：由 Code Agent 作为子进程启动（MCP stdio 传输）。
//!
//! 状态源：读取工作目录 `runtime/mcp-state.json` 快照（desktop 落盘，只读）。
//! 工作目录解析与桌面端同序：CLI `--workdir`、`.duthub/settings.json` 的 `workspaceDir`、
//! exe 同级 `duthub-data/`。
//! 两者都未配置时保留旧发现链（exe 同级存在 → 进程 cwd 存在 → 相对路径，
//! 兼容外部 MCP 客户端以任意 cwd 启动）。
//! stdout 只输出 MCP 协议数据（rmcp 管理）；日志走 stderr。

use std::path::{Path, PathBuf};

/// 从候选 bootstrap 目录读取 settings.json 的 `workspaceDir`（缺失/损坏按未配置处理）。
fn load_workspace_setting(candidates: &[PathBuf]) -> Option<String> {
    candidates.iter().find_map(|dir| {
        let raw = std::fs::read_to_string(dir.join("settings.json")).ok()?;
        let settings: duthub_core::SettingsFile = serde_json::from_str(&raw).ok()?;
        settings.workspace_dir.filter(|s| !s.trim().is_empty())
    })
}

/// 工作目录发现（快照只读，与桌面端写盘侧同源解析）：
/// 1) CLI `--workdir` 或 settings.json `workspaceDir` 配置 → `resolve_work_dir_in`
///    （相对路径锚定本进程 exe 目录）；
/// 2) 均未配置时优先选择新快照，兼容旧目录中的快照。
fn discover_data_dir() -> PathBuf {
    use duthub_core::store::{config_dir_in, default_work_dir_in, resolve_portable_work_dir_in};

    let anchor = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    match duthub_core::store::explicit_workspace(&anchor, std::env::args_os().skip(1)) {
        Ok(Some(root)) => return configured_data_dir(root),
        Ok(None) => {}
        Err(error) => {
            eprintln!("[duthub-mcp] {error}");
            std::process::exit(2);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| anchor.clone());
    let settings_workspace = load_workspace_setting(&[config_dir_in(&anchor), config_dir_in(&cwd)]);

    if let Some(configured) = settings_workspace.as_deref() {
        let root = resolve_portable_work_dir_in(&anchor, None, Some(configured));
        return configured_data_dir(root);
    }
    [default_work_dir_in(&anchor), default_work_dir_in(&cwd)]
        .into_iter()
        .find(|dir| dir.join("runtime/mcp-state.json").is_file())
        .unwrap_or_else(|| default_work_dir_in(&anchor))
}

fn configured_data_dir(root: PathBuf) -> PathBuf {
    let desktop_state = duthub_core::store::default_work_dir_in(&root);
    if desktop_state.join("runtime/mcp-state.json").is_file()
        || (desktop_state.is_dir() && !root.join("runtime/mcp-state.json").is_file())
    {
        desktop_state
    } else {
        root
    }
}

use anyhow::Context;

fn run() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .context("failed to build Tokio runtime")?;

    let args: Vec<_> = std::env::args().collect();
    if let Some(index) = args.iter().position(|arg| arg == "--bridge") {
        let Some(endpoint) = args.get(index + 1) else {
            eprintln!("[duthub-mcp] --bridge requires an MCP endpoint");
            std::process::exit(2);
        };
        let Ok(token) = std::env::var("DUTHUB_MCP_TOKEN") else {
            eprintln!("[duthub-mcp] DUTHUB_MCP_TOKEN is required in bridge mode");
            std::process::exit(2);
        };
        let result = rt.block_on(duthub_mcp::transport::bridge_stdio(endpoint, token));
        // On Windows a pending blocking stdin operation cannot be cancelled by Tokio.
        rt.shutdown_background();
        result.map_err(anyhow::Error::msg)?;
        return Ok(());
    }

    rt.block_on(async move {
        let snapshot_path = discover_data_dir().join("runtime").join("mcp-state.json");
        if !snapshot_path.exists() {
            eprintln!(
                "[duthub-mcp] snapshot missing ({}): tools will return empty data until duthub desktop is started.",
                snapshot_path.display()
            );
        }
        // Long-lived stdio process: reload the snapshot before each tool call
        // (the desktop writes every 2s) so data never goes stale.
        let state = duthub_mcp::snapshot::reloading_snapshot_state(&snapshot_path);
        duthub_mcp::transport::serve_stdio(state)
            .await
            .map_err(anyhow::Error::msg)
            .context("stdio server error")
    })
}

fn main() {
    if let Err(err) = run() {
        eprintln!("[duthub-mcp] {err:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_data_dir_prefers_the_desktop_snapshot_over_a_legacy_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let desktop_state = root.join("duthub-data");
        std::fs::create_dir(&desktop_state).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::write(root.join("runtime/mcp-state.json"), "{}").unwrap();
        assert_eq!(configured_data_dir(root.to_path_buf()), root);

        std::fs::create_dir(desktop_state.join("runtime")).unwrap();
        std::fs::write(desktop_state.join("runtime/mcp-state.json"), "{}").unwrap();
        assert_eq!(configured_data_dir(root.to_path_buf()), desktop_state);
    }
}
