//! Per-DUT A2A loopback endpoint (`duthub-a2a`, W3 minimal surface, ADR-2/8/9).
//!
//! - Each listener serves a single DUT bound to `127.0.0.1` (non-loopback returns
//!   `NeedsProductSignoff`).
//! - The JSON-RPC surface (`SendMessage` / `GetTask` / `CancelTask`) is Bearer fail-closed;
//!   all other methods return `-32601` (`task::StrictRequestHandler`).
//! - The executor maps `DataPart` `{skill, dutId?, args}` to the W1 evidence MCP service
//!   (`McpServerCore::for_evidence` over an in-process duplex transport), allowing only the
//!   8 evidence tools.
//! - Task state is in-memory only (ADR-9); `messageId` is deduplicated in-process with no
//!   `taskId` reuse guarantee.
//! - The Agent Card is minimal (ADR-8): no DUT name, COM port, path, hostname, or token.

#![forbid(unsafe_code)]

pub mod auth;
pub mod card;
pub mod evidence;
pub mod executor;
pub mod listener;
pub mod ports;
pub mod task;
