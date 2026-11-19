//! OS-enforced sandbox for commands an Agent asks the host to run (ADR-12).
//!
//! A cwd, a path prefix check or a command allow-list is not a security
//! boundary: a child process can open any absolute path with the rights of its
//! token. This crate makes the boundary the kernel's decision — **which token
//! the child runs under** — and keeps every other layer (policy, status,
//! diagnostics) free of unsafe code.
//!
//! # Layers
//!
//! - [`policy`]: pure logic. [`SandboxPolicy`], [`CellSpec`], script limits
//!   and the PowerShell request (`-EncodedCommand`, allow-listed environment).
//!   No OS calls, unit-testable everywhere.
//! - [`backend`]: [`SandboxBackend`], the only OS entry point. All platform code
//!   lives behind it.
//! - `windows::{token, acl, container, process}`: the Windows implementation.
//!   `unsafe` is confined to these modules; each block carries a `SAFETY:`
//!   note. `tests/source_contract.rs` pins that arrangement.
//! - [`status`]: capability probing and an honest enum. There is deliberately
//!   no `os_sandboxed: bool`: a caller has to name the capability it relies on.
//!
//! # Two identities, because two callers need different things
//!
//! - [`IsolationMode::RestrictedToken`] — the Agent host (`goose`, `claude`). A
//!   `WRITE_RESTRICTED` token whose restricting SID is a per-DUT random
//!   capability SID (modeled on OpenAI Codex `codex-rs/windows-sandbox-rs`):
//!   writes land only where that SID was granted, while keeping the host's
//!   network stack so the Agent can reach loopback MCP listeners. Read
//!   separation requires the dedicated sandbox account + elevated launcher.
//! - [`IsolationMode::AppContainer`] — scripts. A per-DUT AppContainer with
//!   **zero capabilities**: no network at all (loopback included), no read of
//!   anything not explicitly granted, a Job Object for process/memory limits
//!   and tree kill. This is the surface untrusted model output touches.
//!
//! # Fail-closed
//!
//! If a token, profile, ACL or Job cannot be established, or the child's token
//! cannot be verified before it is resumed, the call returns an error and the
//! child is terminated while still suspended. There is no unsandboxed fallback
//! and no "best effort" mode.
//!
//! On non-Windows every entry point returns an explicit
//! [`SandboxError::Unsupported`]: the platform has no backend, and the same
//! assertions run there (see `tests/security_invariants.rs`) so a cfg cannot
//! make a security test disappear.

mod backend;
pub mod os_acl;
pub mod policy;
mod status;
mod stream;
#[cfg(not(windows))]
mod unsupported;
#[cfg(windows)]
mod windows;

use std::ffi::OsString;
use std::path::Path;

pub use backend::{PreparedCell, SandboxBackend};
pub use os_acl::{
    AclError, OsIsolationStatus, PrivateDirAcl, harden_private_dir, inspect_private_dir_acl,
    probe_os_isolation, probe_restricted_token,
};
pub use policy::{
    CellSpec, DEFAULT_MEMORY_LIMIT_BYTES, DEFAULT_OUTPUT_BYTES, DEFAULT_PROCESS_LIMIT,
    DEFAULT_TIMEOUT, ExecRequest, ExecResult, IsolationMode, MAX_MEMORY_LIMIT_BYTES,
    MAX_OUTPUT_BYTES, MAX_PROCESS_LIMIT, MAX_SCRIPT_CHARS, MAX_TIMEOUT, MIN_MEMORY_LIMIT_BYTES,
    NetworkPolicy, SandboxError, SandboxPolicy, ScratchLayout, SpawnRequest, encode_script,
    powershell_environment, powershell_request, script_denial_reason, validate_policy,
    validate_scope, windows_powershell,
};
pub use status::{Capability, IdentityReport, SandboxStatus};
pub use stream::{SandboxLimits, SandboxedProcess};

/// The single OS backend of this build.
pub fn backend() -> &'static dyn SandboxBackend {
    #[cfg(windows)]
    {
        windows::backend()
    }
    #[cfg(not(windows))]
    {
        unsupported::backend()
    }
}

/// One-time, administrator-only provisioning of the dedicated sandbox identity
/// (ADR-12).
///
/// Creates the local group `DuthubSandboxUsers` and account `DuthubSandbox`,
/// stores a random password DPAPI-sealed under `%LOCALAPPDATA%\duthub\sandbox`,
/// and verifies that the account logs on with a non-administrator token.
///
/// **Provisioning alone is not enough to use the identity.** Starting a process
/// as another account needs `SeAssignPrimaryTokenPrivilege` and
/// `SeIncreaseQuotaPrivilege` in the *calling* process, which an ordinary
/// (UAC-filtered) desktop process never holds — `CreateProcessAsUser` then fails
/// with Win32 1314. `status().identity` verifies both conditions, and reports
/// `Unverified` with the exact reason when either is missing. Until both hold,
/// scripts still get network/write/process isolation but **reads are not
/// separated** and any policy with `deny_read` is refused (ADR-12 §4.1.3).
///
/// Fails with a clear error when called without elevation; no account is ever
/// created implicitly by [`run`], [`run_script`] or [`prepare`].
pub fn provision_identity() -> Result<IdentityReport, SandboxError> {
    #[cfg(windows)]
    {
        windows::provision_identity()
    }
    #[cfg(not(windows))]
    {
        Err(SandboxError::Unsupported(
            "当前平台没有沙箱身份后端（仅 Windows 提供）",
        ))
    }
}

/// Provision the sandbox identity with the credential stored under `root`.
///
/// Same contract as [`provision_identity`], but the DPAPI-sealed credential and
/// the account name live in `root` — for hosts whose data must stay inside a
/// project directory. `root` also becomes this process's credential directory
/// ([`set_identity_root`]), so provisioning and later use agree on the path.
pub fn provision_identity_in(root: &std::path::Path) -> Result<IdentityReport, SandboxError> {
    #[cfg(windows)]
    {
        windows::provision_identity_in(root)
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        Err(SandboxError::Unsupported(
            "当前平台没有沙箱身份后端（仅 Windows 提供）",
        ))
    }
}

/// Override the directory that holds the sandbox credential.
///
/// Precedence: this override > `DUTHUB_SANDBOX_IDENTITY_ROOT` >
/// `%LOCALAPPDATA%\duthub\sandbox`. `None` clears the override. The path must
/// be absolute. **Known deviation**: with the default, the credential (a DPAPI
/// blob, never plaintext) is written to the user profile rather than the project
/// directory — see ADR-12 §4.1.4.
pub fn set_identity_root(root: Option<std::path::PathBuf>) -> Result<(), SandboxError> {
    #[cfg(windows)]
    {
        windows::set_identity_root(root)
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        Err(SandboxError::Unsupported(
            "当前平台没有沙箱身份后端（仅 Windows 提供）",
        ))
    }
}

/// The directory the sandbox credential is read from (see [`set_identity_root`]).
pub fn identity_root() -> Result<std::path::PathBuf, SandboxError> {
    #[cfg(windows)]
    {
        windows::identity_root()
    }
    #[cfg(not(windows))]
    {
        Err(SandboxError::Unsupported(
            "当前平台没有沙箱身份后端（仅 Windows 提供）",
        ))
    }
}

/// Whether the current process runs with an enabled Administrators group.
///
/// Callers (CLI, UI) use it to decide whether [`provision_identity`] can be
/// attempted; the function itself also refuses without elevation.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        windows::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// What this host can *actually* enforce. Callers must gate UI claims on the
/// individual capabilities, never on a single "sandboxed" flag.
pub fn status() -> SandboxStatus {
    backend().status()
}

/// Establish (or re-verify) the cell's kernel state: directories, capability
/// SID, container profile and ACLs. Idempotent; repeated launches reuse the
/// same identities instead of stacking ACEs.
pub fn prepare(cell: &CellSpec, policy: &SandboxPolicy) -> Result<PreparedCell, SandboxError> {
    validate_scope(cell.scope())?;
    validate_policy(policy, cell.root())?;
    backend().prepare(cell, policy)
}

/// Run one trusted, host-built request. The policy is validated before any OS
/// state is touched, and `launch` prepares the cell itself, so a caller cannot
/// forget to establish the boundary.
pub fn run(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &ExecRequest,
) -> Result<ExecResult, SandboxError> {
    validate_scope(cell.scope())?;
    validate_policy(policy, &request.cwd)?;
    backend().launch(cell, policy, request)
}

/// Start a **long-lived** sandboxed process (an ACP session host).
///
/// Unlike [`run`], the pipes are handed to the caller: stdin stays writable,
/// stdout/stderr are readable while the process runs, and there is no wall
/// clock limit. The process and memory limits of `policy` still apply, and
/// ending the session is the caller's decision ([`SandboxedProcess::terminate`]
/// or `Drop`); `KILL_ON_JOB_CLOSE` is the last-resort tree kill.
///
/// The token read-back is identical to [`run`]'s: if the child cannot be proven
/// to run under the cell's sandbox token, it is terminated while suspended and
/// no handle is returned.
pub fn spawn_streaming(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &SpawnRequest,
) -> Result<SandboxedProcess, SandboxError> {
    validate_scope(cell.scope())?;
    if !request.program.is_absolute() || !request.program.is_file() {
        return Err(SandboxError::PolicyRejected(format!(
            "程序必须是存在的绝对路径: {}",
            request.program.display()
        )));
    }
    validate_policy(policy, &request.cwd)?;
    backend().spawn(cell, policy, request)
}

/// Run `script` in PowerShell inside the cell's AppContainer.
///
/// The script is the only untrusted input: program, arguments, cwd and
/// environment are built here. Script length, timeout and the hostile-command
/// list are checked before anything is spawned; they are defence in depth on
/// top of the kernel boundary, not a replacement for it.
pub fn run_script(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    script: &str,
) -> Result<ExecResult, SandboxError> {
    if policy.mode != IsolationMode::AppContainer {
        return Err(SandboxError::PolicyRejected(
            "脚本只能在 AppContainer 模式下执行".to_owned(),
        ));
    }
    let layout = cell.layout();
    let request = powershell_request(script, cell.root(), &layout)?;
    run(cell, policy, &request)
}

/// Run `program` under the cell's restricted token (the Agent host path).
///
/// `extra_environment` is appended to the allow-listed environment; it carries
/// things like `GOOSE_PATH_ROOT` and the MCP endpoint. Nothing is inherited
/// from the host process.
pub fn run_agent(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    program: &Path,
    args: &[OsString],
    extra_environment: &[(OsString, OsString)],
) -> Result<ExecResult, SandboxError> {
    if policy.mode != IsolationMode::RestrictedToken {
        return Err(SandboxError::PolicyRejected(
            "Agent 主程序只能在受限令牌模式下执行".to_owned(),
        ));
    }
    if !program.is_absolute() || !program.is_file() {
        return Err(SandboxError::PolicyRejected(format!(
            "程序必须是存在的绝对路径: {}",
            program.display()
        )));
    }
    let layout = cell.layout();
    let home = layout.home();
    let temp = layout.temp();
    let mut environment = powershell_environment(&home, &temp);
    environment.extend(
        extra_environment
            .iter()
            .filter(|(key, _)| valid_environment_key(key))
            .cloned(),
    );
    let request = ExecRequest {
        program: program.to_path_buf(),
        args: args.to_vec(),
        cwd: cell.root().to_path_buf(),
        environment,
        stdin: Vec::new(),
    };
    run(cell, policy, &request)
}

/// Environment variable names from an untrusted caller must not smuggle
/// separators (`A=B`), NULs or empty names into the block.
fn valid_environment_key(key: &std::ffi::OsStr) -> bool {
    let Some(text) = key.to_str() else {
        return false;
    };
    !text.is_empty() && !text.contains(['=', '\0'])
}
