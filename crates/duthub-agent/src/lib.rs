//! Agent runtime support: LLM provider probes, Codex-style workspace policy,
//! PowerShell AST classification, and sandboxed execution orchestration.

#![forbid(unsafe_code)]

pub mod llm;
pub mod policy;
pub mod ps_ast;
pub mod sandbox_exec;

pub use llm::{
    TestRequest, TestResponse, test_model_api, test_model_connection, validate_model_base,
};
pub use policy::{AccessKind, ApprovalPolicy, SandboxMode, Verdict, WorkspaceScope};
pub use sandbox_exec::{PowerShellExecError, PowerShellExecOptions, execute_powershell};
