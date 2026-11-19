//! duthub 桌面库目标（W2 起）。
//!
//! - [`agent_host`]：goose ACP 客户端与每 DUT 会话宿主。goose 不再内嵌：
//!   由「工具下载」页下载安装（`duthub-tooldown`），运行时按「外部路径 >
//!   已下载安装 > PATH」解析，未安装经 [`agent_host::goose_install_state`] 上报。
//! - [`agent_ui`]：Agent 相关 UI 的纯逻辑支撑（W4a）：A2A 导出秘密扫描、
//!   DUT 删除清理序、pending-cleanup 落盘、A2A 端口校验、OS 隔离徽标文案。
//!   不依赖 gpui/workspace/alerts，集成测试可独立导入。
//! - [`capture_backend`]: capture-demand ledger, idle clamping, ffmpeg
//!   resolution, artifact parsing and the `MEDIA_*` error codes
//!   (`docs/design/CAPTURE_STREAM_BACKENDS.md` §3.4/§4.2/§5.4/§5.7). No GPUI types.
//! - [`capture_ui`]: the HDMI monitor-window state machine (§6.2). No GPUI
//!   types; window lifecycle stays in the bin target.
//! - [`tray_ui`]: pure tray/alert decisions (close plan, presentation, alert
//!   router). No GPUI types and no `windows` crate, so it is unit-testable
//!   everywhere.
//! - [`tray_lifecycle`]: pure window-lifecycle coordinator (close/hide/show/
//!   quit/taskbar/alert events → ordered actions). The desktop workbench is the
//!   executor.
//! - [`tray`]: the notification-area host and the only module that touches the
//!   `windows` crate (`docs/design/TRAY_ALERT_DESIGN.md` §4/§8/§12). It exposes the
//!   `TrayBackend` port; non-Windows targets get an inert implementation.
//!
//! MCP HTTP 启动栈留在 bin target 的 `mcp_runtime.rs`（`plan_badges.rs` 的
//! 源码契约把 `spawn_mcp_runtime`/`McpRuntimeHandles` 钉在该文件）；UI 仍在
//! `src/main.rs` 的 bin target。

pub mod agent_host;
pub mod agent_ui;
pub mod capture_backend;
pub mod capture_ui;
pub mod tool_resolver;
pub mod tool_ui;
pub mod tray;
pub mod tray_lifecycle;
pub mod tray_ui;
