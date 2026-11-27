//! Desktop A2A runtime packaging (W4a).
//!
//! The desktop calls the asynchronous `duthub_a2a::A2aRuntime` through synchronous `block_on` entry
//! points. Listeners run on a dedicated Tokio runtime.
//! Register MCP ports from `agentAccess.httpPort` and `listeners[].port` as external reservations
//! before startup. A2A reserves each DUT's `a2a.port` itself when it binds.
//! Use `reserved_ports` and `agent_ui::validate_a2a_port` to check for conflicts before saving or
//! enabling a listener.
//! Report runtime construction or listener startup errors as status so the workbench can still
//! start.

use std::sync::{Arc, Mutex};

use duthub_a2a::listener::{A2aDutConfig, A2aStatus};
use duthub_core::SettingsFile;
use duthub_mcp::state::McpState;

use crate::data::DutsState;
use duthub_desktop::agent_ui;

/// Build listener configurations for A2A-enabled DUTs in the order they appear in `duts.json`.
fn start_configs(duts: &DutsState) -> Vec<A2aDutConfig> {
    duts.file
        .duts
        .iter()
        .filter(|dut| dut.a2a.enabled)
        .map(|dut| A2aDutConfig {
            dut_id: dut.id.clone(),
            port: dut.a2a.port,
            bind_addr: dut.a2a.bind_addr.clone(),
            agent_name: dut.a2a.agent_name.clone(),
        })
        .collect()
}

/// MCP side ports are managed by `A2aRuntime`. These ports are registered as "External": A2A does
/// not bind these ports.
/// Note: Ports that have been configured for use by the DUT itself are not included in this list.
/// `PortRegistry` will refuse to reserve such ports, and registration will prevent the runtime from
/// binding these ports to itself.
fn reserved_mcp_ports(settings: &SettingsFile) -> Vec<u16> {
    let mut reserved = vec![settings.agent_access.http_port];
    reserved.extend(settings.agent_access.listeners.iter().map(|l| l.port));
    reserved
}

/// Collect the MCP HTTP port, all `listeners[].port` values, and enabled DUT `a2a.port` values.
/// Deduplicate them in order. Validate proposed A2A ports with `agent_ui::validate_a2a_port`.
pub fn combined_reserved_ports(settings: &SettingsFile, duts: &DutsState) -> Vec<u16> {
    agent_ui::reserved_ports_from(
        settings,
        duts.file
            .duts
            .iter()
            .filter(|dut| dut.a2a.enabled)
            .map(|dut| dut.a2a.port),
    )
}

/// Desktop-held A2A runtime (`Workbench::a2a`), driven on the shared desktop
/// Tokio runtime handle so it neither spawns a redundant runtime nor blocks the
/// GPUI UI thread on listener start/stop.
pub struct A2aRuntime {
    inner: Arc<duthub_a2a::listener::A2aRuntime>,
    /// Shared Tokio handle from `main.rs`; `None` only after `shutdown()`.
    handle: Option<tokio::runtime::Handle>,
    /// DUT IDs configured during this session (scope for `any_running`).
    started: Mutex<Vec<String>>,
    /// MCP ports reserved so later `start_duts` calls never collide with active MCP listeners.
    reserved_mcp: Vec<u16>,
}

impl Drop for A2aRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl A2aRuntime {
    /// Start A2A listeners for all enabled DUTs on the shared Tokio `handle`
    /// without blocking the GPUI UI thread.
    pub fn start_with_handle(
        duts: &DutsState,
        settings: &SettingsFile,
        state: Arc<dyn McpState>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        let inner = Arc::new(duthub_a2a::listener::A2aRuntime::new());
        let configs = start_configs(duts);
        let reserved_mcp = reserved_mcp_ports(settings);
        let started_ids = configs.iter().map(|config| config.dut_id.clone()).collect();
        let inner_bg = inner.clone();
        let reserved_bg = reserved_mcp.clone();
        handle.spawn(async move {
            let outcomes = inner_bg.start(configs, state, reserved_bg).await;
            for (dut_id, status) in outcomes {
                if let A2aStatus::Failed { reason } | A2aStatus::NeedsProductSignoff { reason } =
                    &status
                {
                    eprintln!("[a2a] DUT {dut_id} listener start failed: {reason}");
                }
            }
        });
        Self {
            inner,
            handle: Some(handle),
            started: Mutex::new(started_ids),
            reserved_mcp,
        }
    }

    /// Current status of one DUT (unstarted DUT = Disabled).
    pub fn status(&self, dut_id: &str) -> A2aStatus {
        if self.handle.is_none() {
            return A2aStatus::Failed {
                reason: "A2A runtime unavailable".into(),
            };
        }
        self.inner.status(dut_id)
    }

    /// Whether any configured A2A listener is currently running.
    pub fn any_running(&self) -> bool {
        let started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        started
            .iter()
            .any(|dut_id| matches!(self.inner.status(dut_id), A2aStatus::Running { .. }))
    }

    /// Rotate the bearer token in keyring and hot-swap any running listener.
    pub fn set_token(&self, dut_id: &str, token: &str) -> Result<(), String> {
        let Some(handle) = self.handle.as_ref() else {
            return Err("A2A runtime unavailable".into());
        };
        handle.block_on(self.inner.set_token(dut_id, token))
    }

    /// Reconcile and start newly enabled DUT listeners asynchronously on the
    /// shared Tokio runtime so the UI thread never blocks on socket binds.
    pub fn start_duts(&self, duts: &DutsState, state: Arc<dyn McpState>) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let configs = start_configs(duts);
        {
            let mut started = self
                .started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for config in &configs {
                if !started.contains(&config.dut_id) {
                    started.push(config.dut_id.clone());
                }
            }
        }
        let inner = self.inner.clone();
        let reserved = self.reserved_mcp.clone();
        handle.spawn(async move {
            let _ = inner.start(configs, state, reserved).await;
        });
    }

    /// Stop one DUT's listener asynchronously and release its port.
    pub fn stop_dut(&self, dut_id: &str) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let inner = self.inner.clone();
        let dut = dut_id.to_owned();
        handle.spawn(async move {
            inner.stop_dut(&dut).await;
        });
    }

    /// Stop all listeners synchronously before application exit (idempotent).
    pub fn shutdown(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        handle.block_on(self.inner.shutdown());
    }
}
