//! duthub-core：DUT 状态、配置 schema、typed command/event 与后端 trait。
//!
//! 所有 UI（GPUI desktop / Ratatui tui）与协议服务（MCP / A2A）只消费本 crate
//! 暴露的接口，不复制业务逻辑（docs/ARCHITECTURE.md）。

pub mod agent_access;
pub mod approval;
pub mod audit;
pub mod backend;
pub mod dut_artifacts;
pub mod dut_quota;
pub mod error;
pub mod event;
pub mod heartbeat;
pub mod ipc;
pub mod keys;
pub mod lease;
pub mod llm;
pub mod log_archive;
pub mod model;
pub mod process;
pub mod store;

pub use agent_access::{
    AgentAccessRuntime, AgentAccessSettings, AgentAccessStore, AgentCredential, AuthError,
    DutAction, DutGrant, JsonAgentAccessStore, Principal,
};
pub use approval::{ApprovalError, ApprovalManager, ApprovalToken};
pub use audit::{AuditEvent, AuditOutcome, AuditSink};
pub use backend::{
    AdbBackend, AdbDevice, ControlBackend, ControlTarget, DutId, FlashExecutor, ProcessResult,
    SerialBackend, SerialPortInfo,
};
pub use error::{DeviceError, DeviceResult};
pub use event::AppEvent;
pub use heartbeat::{HeartbeatAlert, HeartbeatConfig, HeartbeatState, SysInfo};
pub use ipc::{AppCommand, AppService, ChunkStyle, EventEnvelope, UiEvent};
pub use keys::{delete_key, get_key, set_key};
pub use lease::{
    ControlAdmission, ControlLeaseManager, LeaseAuthority, LeaseError, LeaseGrant, LeaseHolder,
    LeaseId, LeasePhase, LeaseStatus, WriteCtx, WriteIdentity,
};
pub use llm::{
    Provider, TestRequest, TestResponse, model_provider_credential_id, test_model_api,
    test_model_connection, validate_model_base,
};
pub use model::{A2aConfig, A2aPeer, DutsFile, SettingsFile};
