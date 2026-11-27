//! Goose ACP Host (W2) + Universal ACP Agent Registry.
//!
//! Layers:
//! - [`bundle`]: Goose licensing/release details (compatible with various platforms) and version
//!   detection mechanisms. Goose is no longer embedded in the application; it is downloaded and
//!   installed via the "Tool Download" page.
//! - [`registry`]: Definitions for `agents.json` are used to determine startup specifications
//!   (external paths/hashes/version thresholds). Additionally, there's a status indicator
//!   [`registry::goose_install_state`].
//! - [`acp`]: JSON-RPC ACP client implemented per line; stdout follows the protocol only.
//! - [`job`]: self-containment helper (`contain_current_process`) for test stubs;
//! - [`model_env`]: model provider -> goose environment and managed config;
//! - [`sandbox`]: per-DUT sandbox / `GOOSE_PATH_ROOT` / transcript layout and unified MXC policy preparation;
//! - [`listener`]: Loopback MCP listener for each session, along with read-only tokens.
//! - [`session`]: Session state, maximum number of transcripts stored in memory/files.
//! - [`transcript`]: async transcript writer thread (keeps disk I/O off the ACP loop);
//! - [`host`]: Each DUT is assigned a `HostManager` process for the Agent (default: goose);
//! - [`cli`]: CLI profiles (W2c). The first implemented profile is Claude Code:
//!   argv/NDJSON parsing/resume, spawned once per turn.

pub mod acp;
pub mod bundle;
pub mod cli;
pub mod host;
pub mod job;
pub mod listener;
pub mod model_env;
pub mod registry;
pub mod sandbox;
pub mod session;
pub mod transcript;

pub use acp::{AcpClient, AcpEvent, InitializeInfo, McpServerRef, SessionInfo};
pub use bundle::{BundleError, GOOSE_VERSION};
pub use cli::{
    CLAUDE_CODE_PROFILE, CliEvent, CliPlan, claude_code_plan, claude_code_plan_with_resume,
    ensure_cli_launchable, parse_claude_event,
};
pub use host::{BindingSource, HostError, HostEvents, HostManager, StopJoin};
pub use listener::{ListenerError, SessionListener};
pub use model_env::GooseBinding;
pub use registry::{
    AgentLaunchSpec, GOOSE_AGENT_ID, GOOSE_TOOL_ID, GooseInstallState, MIN_GOOSE_VERSION,
    ProgramProbe, RegistryError, downloaded_goose, ensure_goose_version, ensure_launchable,
    goose_install_state, goose_install_state_from, goose_install_state_in, is_absolute_program,
    parse_version, probe_program, resolve, resolve_builtin_from, version_at_least,
};
pub use sandbox::{AgentPaths, SandboxError};
pub use session::{SessionStatus, SessionView, TranscriptEntry, TranscriptKind};
pub use transcript::TranscriptWriter;
