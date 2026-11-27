//! Duthub desktop library target (available since W2).
//!
//! - [`agent_host`] provides the goose ACP client and per-DUT session host. Use the tool downloader
//!   to install goose. At runtime prefer the configured external path, then the downloaded
//!   executable, then PATH. [`agent_host::goose_install_state`] reports when none is available.
//! - [`agent_ui`] provides testable Agent logic for A2A credential checks and port validation, DUT
//!   deletion and cleanup persistence, and isolation badge labels.
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
//! The MCP HTTP startup stack remains in the `bin_target` directory, within the file
//! `mcp_runtime.rs`. The source code contract for `plan_badges.rs` defines `spawn_mcp_runtime` and
//! `McpRuntimeHandles`, which are located in this file. The UI-related components, on the other
//! hand, are still located in the `src/main.rs` file within the `bin_target` directory.

pub mod agent_host;
pub mod agent_ui;
pub mod capture_backend;
pub mod capture_ui;
pub mod tool_resolver;
pub mod tool_ui;
pub mod tray;
pub mod tray_lifecycle;
pub mod tray_ui;
