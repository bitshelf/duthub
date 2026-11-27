//! 桌面端 A2A 运行时包装（W4a）。
//!
//! - 把 `duthub_a2a::A2aRuntime` 的 async API 驱动在专属 tokio 运行时上，
//!   GUI 线程全部走同步入口（`block_on`）；监听任务常驻 worker 线程。
//! - 启动时把 MCP 侧端口（`agentAccess.httpPort` + `listeners[].port`）以
//!   External 身份预登记进 PortRegistry，A2A 绝不抢占；每 DUT 自身的
//!   `a2a.port` 由运行时自行 reserve（预登记反而会让自己的 bind 被拒）。
//! - `reserved_ports`（MCP + 全部已启用 DUT A2A 端口的合集）用于保存/启用
//!   时的冲突检测（`agent_ui::validate_a2a_port`）。
//! - 任何失败（驱动构建失败/启动失败）都降级为状态可见，绝不 panic——
//!   Workbench 必须能在 A2A 不可用时照常启动。

use std::sync::{Arc, Mutex};

use duthub_a2a::listener::{A2aDutConfig, A2aStatus};
use duthub_core::SettingsFile;
use duthub_mcp::state::McpState;

use crate::data::DutsState;
use duthub_desktop::agent_ui;

/// 已启用 A2A 的 DUT → 监听器配置（顺序与 duts.json 一致）。
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

/// MCP 侧端口（交给 `A2aRuntime` 预登记为 External：A2A 不绑定这些端口）。
/// 注意：已启用 DUT 自身的 a2a 端口不在此列——PortRegistry 会对任何已有
/// 持有者拒绝 reserve，预登记会让运行时拒绝绑定自己的端口。
fn reserved_mcp_ports(settings: &SettingsFile) -> Vec<u16> {
    let mut reserved = vec![settings.agent_access.http_port];
    reserved.extend(settings.agent_access.listeners.iter().map(|l| l.port));
    reserved
}

/// 冲突检测合集（W4a）：MCP HTTP 端口 + listeners[].port + 全部已启用
/// DUT 的 a2a.port（去重保序）。保存/启用 A2A 端口前经
/// `agent_ui::validate_a2a_port` 校验。
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
