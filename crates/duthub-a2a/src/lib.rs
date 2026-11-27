//! duthub-a2a：每 DUT A2A 回环端点（W3 最小面，ADR-2/8/9）。
//!
//! - 一个监听器只服务一个 DUT，绑定 `127.0.0.1`（非回环一律 `NeedsProductSignoff`）。
//! - JSON-RPC 面（SendMessage/GetTask/CancelTask）Bearer fail-closed；其余方法
//!   一律 `-32601`（`task::StrictRequestHandler`）。
//! - 执行体把 `DataPart` `{skill, dutId?, args}` 映射到 W1 证据 MCP 服务
//!   （`McpServerCore::for_evidence`，进程内 duplex 传输），白名单 8 个证据工具。
//! - 任务状态仅内存（ADR-9）；messageId 进程内去重，不承诺 taskId 复用。
//! - Agent Card 内容最小化（ADR-8）：无 DUT 名/COM/路径/hostname/token。

#![forbid(unsafe_code)]

pub mod auth;
pub mod card;
pub mod evidence;
pub mod executor;
pub mod listener;
pub mod ports;
pub mod task;
