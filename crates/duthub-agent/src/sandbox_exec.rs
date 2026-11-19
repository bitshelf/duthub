//! Unified sandboxed PowerShell executor combining the Codex-style AST policy
//! gate ([`crate::ps_ast`]) with the OS-enforced kernel write sandbox
//! ([`duthub_sandbox`]).
//!
//! Two-layer defense in depth (Windows non-elevated desktop, Option A):
//! 1. **AST policy gate**: inspects the raw script with PowerShell's native
//!    parser before spawning any worker, rejecting `Forbidden` scripts and
//!    requiring explicit approval for `Prompt` scripts.
//! 2. **Kernel write boundary**: launches PowerShell inside `duthub_sandbox`
//!    (`IsolationMode::RestrictedToken` + Job Object) with `active_dut_dir` as
//!    the sole writable cell, so even if an approved script mutates paths
//!    dynamically, writes outside `active_dut_dir` are denied by the Windows
//!    kernel.

use std::fmt;
use std::time::Duration;

use duthub_sandbox::{
    CellSpec, DEFAULT_TIMEOUT, ExecResult, SandboxError, SandboxPolicy, powershell_request, run,
};

use crate::policy::{ApprovalPolicy, SandboxMode, Verdict, WorkspaceScope};
use crate::ps_ast;

/// Execution parameters for [`execute_powershell`].
#[derive(Debug, Clone)]
pub struct PowerShellExecOptions {
    /// Logical scope name (used to derive the per-DUT sandbox cell identity).
    pub cell_scope: String,
    /// Filesystem workspace scope (active DUT RW, sibling DUTs RO, protected deny).
    pub workspace: WorkspaceScope,
    /// Isolation mode.
    pub mode: SandboxMode,
    /// Approval policy for [`Verdict::Prompt`] decisions.
    pub approval: ApprovalPolicy,
    /// Whether the operator explicitly approved this exact script invocation.
    pub approved: bool,
    /// Wall-clock timeout enforced by the sandbox runner.
    pub timeout: Duration,
}

impl PowerShellExecOptions {
    pub fn for_dut(cell_scope: impl Into<String>, workspace: WorkspaceScope) -> Self {
        Self {
            cell_scope: cell_scope.into(),
            workspace,
            mode: SandboxMode::DutWorkspaceWrite,
            approval: ApprovalPolicy::OnRequest,
            approved: false,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Failure modes of [`execute_powershell`].
#[derive(Debug)]
pub enum PowerShellExecError {
    /// Rejected by the AST/path policy gate before any process was started.
    Forbidden(String),
    /// Requires interactive operator approval before execution.
    ApprovalRequired(String),
    /// Rejected or failed inside the OS sandbox layer.
    Sandbox(SandboxError),
}

impl fmt::Display for PowerShellExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forbidden(reason) => write!(f, "PowerShell execution forbidden: {reason}"),
            Self::ApprovalRequired(reason) => {
                write!(f, "PowerShell execution requires approval: {reason}")
            }
            Self::Sandbox(error) => write!(f, "sandbox execution failed: {error}"),
        }
    }
}

impl std::error::Error for PowerShellExecError {}

impl From<SandboxError> for PowerShellExecError {
    fn from(error: SandboxError) -> Self {
        Self::Sandbox(error)
    }
}

/// Evaluate `script` through the AST policy gate and execute it inside the
/// kernel-enforced `duthub_sandbox` cell for the active DUT.
pub fn execute_powershell(
    options: &PowerShellExecOptions,
    script: &str,
) -> Result<ExecResult, PowerShellExecError> {
    let verdict = ps_ast::analyze_script(script, &options.workspace, options.mode);
    match verdict {
        Verdict::Allow => {}
        Verdict::Forbidden { reason } => return Err(PowerShellExecError::Forbidden(reason)),
        Verdict::Prompt { reason } => {
            let permitted = options.approved && options.approval != ApprovalPolicy::Never;
            if !permitted {
                return Err(PowerShellExecError::ApprovalRequired(reason));
            }
        }
    }

    let cell = CellSpec::new(&options.workspace.active_dut_dir, &options.cell_scope)?;
    let layout = cell.layout();
    let request = powershell_request(script, cell.root(), &layout)?;
    let mut policy = SandboxPolicy::agent_cell(&cell);
    policy.timeout = options.timeout;
    run(&cell, &policy, &request).map_err(PowerShellExecError::Sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn executes_allowed_script_and_blocks_sibling_write_before_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let dut_a = tmp
            .path()
            .join("agent")
            .join("dut")
            .join("dut-a")
            .join("sandbox");
        let dut_b = tmp
            .path()
            .join("agent")
            .join("dut")
            .join("dut-b")
            .join("sandbox");
        std::fs::create_dir_all(&dut_a).unwrap();
        std::fs::create_dir_all(&dut_b).unwrap();
        std::fs::write(dut_a.join("boot.log"), "hello-dut-a\n").unwrap();

        let scope = WorkspaceScope::new(&dut_a).with_sibling_dut(&dut_b);
        let opts = PowerShellExecOptions::for_dut("dut-a", scope);

        // Allowed read inside active DUT runs inside the RestrictedToken sandbox.
        let res = execute_powershell(&opts, "Get-Content -Path boot.log").unwrap();
        assert_eq!(res.exit_code, Some(0));
        assert!(String::from_utf8_lossy(&res.stdout).contains("hello-dut-a"));

        // Forbidden write to sibling DUT is rejected before spawn.
        let err = execute_powershell(
            &opts,
            &format!(
                "Set-Content -Path '{}' -Value 'bad'",
                dut_b.join("x.txt").display()
            ),
        )
        .unwrap_err();
        assert!(matches!(err, PowerShellExecError::Forbidden(_)));
        assert!(!dut_b.join("x.txt").exists());
    }
}
