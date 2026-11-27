//! MCP stdio entry point for a subprocess launched by an Agent.
//!
//! Read the desktop's persisted state snapshot from the work directory.
//! Use the explicit work directory, then the saved workspace directory,
//! then the default beside the executable.
//! Without a configured directory, also look for existing legacy snapshots.
//! This supports clients that launch the executable from another directory.
//! Reserve standard output for MCP messages and send diagnostics to standard error.

use std::path::{Path, PathBuf};

/// Read the saved workspace directory if the bootstrap settings exist and are valid.
fn load_workspace_setting(candidates: &[PathBuf]) -> Option<String> {
    candidates.iter().find_map(|dir| {
        let raw = std::fs::read_to_string(dir.join("settings.json")).ok()?;
        let settings: duthub_core::SettingsFile = serde_json::from_str(&raw).ok()?;
        settings.workspace_dir.filter(|s| !s.trim().is_empty())
    })
}

/// Locate the read-only snapshot using the desktop's work directory precedence.
/// Resolve an explicit override or the saved setting first.
/// Relative paths use the executable directory as their base.
/// Without an override, prefer the current snapshot location and recognize legacy files.
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
