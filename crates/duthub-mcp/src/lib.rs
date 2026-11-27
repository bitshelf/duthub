//! duthub-mcp：MCP Server（官方 rmcp SDK）。
//!
//! - 工具集与 `docs/API_CONTRACT.md` 对齐（只读 + 受租约/审批双重保护的写操作）。
//! - 两种状态源：
//!   - `RuntimeMcpState`（desktop 进程内桥接 `DeviceService` 等，HTTP 端点使用）
//!   - `SnapshotState`（独立 stdio 进程读工作目录 `runtime/mcp-state.json` 快照，只读；
//!     目录发现与桌面端同源解析，见 bin 入口 `discover_data_dir`）
//! - 传输：stdio（`serve_stdio`）与 Streamable HTTP（`serve_http_blocking`；默认 loopback，
//!   显式局域网模式绑 `0.0.0.0` 且强制 Bearer 认证层）。
//! - 授权链（方案 §3）：[A] HTTP Bearer `认证（auth_http`）→ [B] Grant 授权 +
//!   [C] 租约归属/代数复核（server.rs）→ [D]/[E] device 写入通道双点复核 → 执行 → 审计。
//! - `notify_user`（`notify`）：宿主的用户通知桥。它不触设备、无审批/租约，但能唤起
//!   用户界面；只挂在注入了 `UserNotifier` 的用户配置监听器上（桌面 Agent 会话没有）。

#![forbid(unsafe_code)]

pub mod auth_http;
pub mod notify;
pub mod probe;
pub mod server;
pub mod skill;
pub mod snapshot;
pub mod state;
pub mod transport;

pub use auth_http::{AccessContext, CurrentPrincipal};
pub use notify::{
    MESSAGE_MAX_CHARS, NotifyLevel, NotifyOutcome, NotifyRequest, TITLE_MAX_CHARS, ThrottleReason,
    UserNotifier,
};
pub use probe::{ProbeError, list_tool_names};
pub use server::{EVIDENCE_TOOL_NAMES, McpServerCore, PLAN_TOOL_NAMES};
pub use state::{ApprovalIssueResult, ControlStatusDto, LeaseGrantDto, McpState};
pub use transport::serve_stdio;
