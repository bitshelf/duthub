//! MCP server implemented with the rmcp SDK.
//!
//! The API contract describes read-only tools and writes guarded by leases and approvals.
//! HTTP and stdio use different state sources.
//! HTTP reads live device state inside the desktop process.
//! Stdio reads the desktop's persisted snapshot.
//! Both use the same work directory resolution.
//! The server supports stdio and Streamable HTTP.
//! HTTP binds to loopback by default. LAN access requires explicit configuration and Bearer authentication.
//! HTTP authenticates the request before checking grants and lease ownership and generation.
//! The device checks writes again at admission and execution, then records audit events.
//! The optional notifier forwards requests to the host's user interface.
//! Only user-configured listeners receive it. Desktop-initiated Agent sessions do not.

#![forbid(unsafe_code)]

pub mod auth_http;
pub mod http_transfer;
pub mod i18n;
pub mod keys;
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
