//! goose ACP 宿主（W2）+ 通用 ACP Agent 注册表。
//!
//! 分层：
//! - [`bundle`]：goose 许可/发布事实（兼容面）与版本探测；goose 不再内嵌，
//!   由「工具下载」页下载安装（2026-11-24）；
//! - [`registry`]：`agents.json` 定义 → 启动规格（外部路径/哈希/版本门槛），
//!   以及未安装状态 [`registry::goose_install_state`]；
//! - [`acp`]：按行 JSON-RPC ACP 客户端（stdout 只走协议）；
//! - [`job`]：Windows Job Object（挂起启动 → 加入 Job → resume；限制与杀树）；
//! - [`model_env`]：模型 provider → goose 环境变量与 managed config；
//! - [`sandbox`]：每 DUT 的 sandbox / GOOSE_PATH_ROOT / transcript 布局；
//! - [`listener`]：每会话 loopback MCP listener 与只读 Token；
//! - [`session`]：会话状态、内存/文件 transcript 上限；
//! - [`transcript`]：async transcript writer thread (keeps disk I/O off the ACP loop);
//! - [`host`]：每 DUT 一个 Agent 进程的 HostManager（默认 goose）；
//! - [`cli`]：CLI profiles (W2c). The first implemented profile is Claude Code:
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
