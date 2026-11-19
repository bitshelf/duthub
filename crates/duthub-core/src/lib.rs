//! duthub-core: DUT state, configuration schema, typed events, and shared contracts.
//!
//! All UI layers and protocol servers consume this crate without duplicating
//! domain rules. `duthub-core` contains zero `unsafe` code and no OS FFI or HTTP
//! runtime dependencies.

#![forbid(unsafe_code)]

pub mod agent_access;
pub mod agent_config;
pub mod approval;
pub mod audit;
pub mod backend;
pub mod dut_artifacts;
pub mod dut_quota;
pub mod error;
pub mod heartbeat;
pub mod ipc;
pub mod keys;
pub mod lease;
pub mod llm;
pub mod log_archive;
pub mod media_op;
pub mod model;
pub mod path_guard;
pub mod process;
pub mod restart;
pub mod store;
pub mod util;

pub use agent_access::{
    AgentAccessRuntime, AgentAccessSettings, AgentCredential, AgentListener, AuthError, DutAction,
    DutGrant, Principal, RuntimeListener,
};
pub use agent_config::{
    AGENT_BINDINGS_FILE_NAME, AGENT_BINDINGS_FILE_VERSION, AGENTS_FILE_NAME, AGENTS_FILE_VERSION,
    AgentBindingsFile, AgentConfigError, AgentDefinition, AgentKind, AgentsFile, DEFAULT_AGENT_ID,
    agent_bindings_path, agent_env_entry, agents_path, default_agents, load_agent_bindings,
    load_agent_env, load_agent_env_with, load_agents, save_agent_bindings_atomic,
    save_agents_atomic, validate_agent_id, validate_env_key,
};
pub use approval::{ApprovalError, ApprovalManager, ApprovalToken};
pub use audit::{AuditEvent, AuditOutcome, AuditSink};
pub use backend::{AdbDevice, ControlBackend, ControlTarget, DutId, ProcessResult, SerialPortInfo};
pub use error::{DeviceError, DeviceResult};
pub use heartbeat::{HeartbeatAlert, HeartbeatConfig, HeartbeatState, SysInfo};
pub use ipc::{ChunkStyle, EventEnvelope, UiEvent};
pub use keys::{
    delete_a2a_token, delete_key, delete_mcp_token, get_a2a_token, get_key, get_mcp_token,
    set_a2a_token, set_key, set_mcp_token,
};
pub use lease::{
    ControlAdmission, ControlLeaseManager, LeaseAuthority, LeaseError, LeaseGrant, LeaseHolder,
    LeaseId, LeasePhase, LeaseStatus, WriteCtx, WriteIdentity,
};
pub use llm::{Provider, model_provider_credential_id};
pub use model::{A2aConfig, A2aPeer, DutsFile, SettingsFile};
pub use restart::{
    INITIAL_RESTART_DELAY, MAX_RESTART_DELAY, MAX_RESTARTS_PER_WINDOW, RESTART_WINDOW,
    RestartBudget,
};
