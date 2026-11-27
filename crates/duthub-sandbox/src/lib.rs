//! Cross-platform OS-enforced sandbox powered by Microsoft Execution Containers
//! (`mxc-sdk`).
//!
//! This crate is 100% safe Rust (`#![forbid(unsafe_code)]`) and delegates all
//! OS-level containment to `mxc_sdk::v1`:
//! - **Windows 11 (native PSEC 1.1+)**: `ProcessContainer` with kernel-enforced
//!   filesystem grants/denials (`readwrite`, `readonly`, `denied_paths`,
//!   `enumerate_paths`), directional network policy (`egress`, `host_loopback`,
//!   optional proxy), UI/clipboard/input-injection isolation, tree-kill Job
//!   Objects, and optional Learning Mode denial capture.
//! - **Linux**: `Bubblewrap`.
//! - **macOS**: `Seatbelt`.
//!
//! # Fail-closed
//!
//! Every entry point validates scope, paths, and policy bounds before invoking
//! `mxc-sdk`. If the host does not provide an enforced MXC backend or policy
//! validation fails, execution is refused with [`SandboxError`] — there is no
//! unsandboxed fallback.

#![forbid(unsafe_code)]

pub mod policy;
mod runner;
mod status;

pub use policy::{
    CellSpec, DEFAULT_OUTPUT_BYTES, DEFAULT_TIMEOUT, DenialCaptureConfig, ExecRequest, ExecResult,
    MAX_OUTPUT_BYTES, MAX_SCRIPT_CHARS, MAX_TIMEOUT, NetworkPolicy, SandboxError, SandboxPolicy,
    ScratchLayout, SpawnRequest, UiPolicy, encode_script, format_command_line,
    powershell_environment, powershell_request, script_denial_reason, validate_policy,
    validate_scope, windows_powershell,
};
pub use runner::{
    DenialCaptureSummary, SandboxedProcess, StreamCloser, prepare_cell, probe_request, run,
    run_agent, run_script, spawn_streaming,
};
pub use status::{SandboxBackendKind, SandboxStatus, status};
