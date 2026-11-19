//! Windows backend: the only place in this workspace where a sandbox boundary
//! is established with Win32 calls.
//!
//! Layering (each layer only talks to the one below it):
//!
//! - [`identity`]: the dedicated local account, its DPAPI-sealed password and
//!   the one-time elevated provisioning step.
//! - [`dpapi`]: `CryptProtectData`/`CryptUnprotectData`, fail closed.
//! - [`token`]: per-cell capability SIDs (stored outside the writable cell) and
//!   the `WRITE_RESTRICTED` tokens, built from the sandbox account when it is
//!   provisioned and from the host token otherwise.
//! - [`no_reparse_dir`]: handle-based directory opens with `OBJ_DONT_REPARSE`.
//! - [`acl`]: per-path ACEs (grant / deny-read / protected DACL) with a read-back
//!   after every write, plus live probes.
//! - [`deny_read_acl`]: planning, materialisation and rollback of deny-read ACEs.
//! - [`container`]: per-cell AppContainer profiles (zero capabilities).
//! - [`process`]: Job Object limits, pipes, `CreateProcess*` and the
//!   suspended-start → token read-back → resume sequence.
//!
//! `unsafe` appears only inside this module tree. Every block carries a
//! `SAFETY:` note; `tests/source_contract.rs` fails the build if that changes.
//!
//! The identity/ACL layer follows openai/codex `codex-rs/windows-sandbox-rs`
//! (Apache-2.0); see `NOTICE` and ADR-12.

mod acl;
mod container;
mod deny_read_acl;
pub(crate) mod dir_acl;
mod dpapi;
mod identity;
mod no_reparse_dir;
pub(crate) mod process;
mod token;

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{CreateWellKnownSid, FreeSid, PSID};

use crate::backend::{PreparedCell, SandboxBackend};
use crate::policy::{
    CellSpec, ExecRequest, ExecResult, IsolationMode, SandboxError, SandboxPolicy, SpawnRequest,
};
use crate::status::{Capability, SandboxStatus};
use crate::stream::SandboxedProcess;

static WINDOWS: WindowsBackend = WindowsBackend;

pub(crate) fn backend() -> &'static dyn SandboxBackend {
    &WINDOWS
}

struct WindowsBackend;

impl SandboxBackend for WindowsBackend {
    fn platform(&self) -> &'static str {
        "windows"
    }

    fn status(&self) -> SandboxStatus {
        let restricted_token = token::probe_restricted();
        let app_container = container::probe();
        let job_limits = process::probe_job_limits();
        // `Verified` only when the credential loads **and** this process holds
        // the privileges `CreateProcessAsUser` needs (Win32 1314 otherwise).
        let (identity, identity_note) = identity::availability()
            .map(|availability| (availability.state, availability.note))
            .unwrap_or_else(|error| (Capability::Unverified, error.to_string()));
        // Read isolation is only claimable when the child runs as a *different*
        // principal than the host user; without a usable identity the ACE probe
        // would prove the API works, not that reads are separated.
        let read_deny = if identity.is_verified() {
            acl::probe_deny_read()
        } else {
            Capability::Unverified
        };
        let detail = format!(
            "受限令牌: {} · AppContainer: {} · Job 限额: {} · 专用沙箱身份: {}（{}）· 读拒绝 ACE: {}（\
             读隔离需要专用身份 + 提权创建进程；不满足时脚本仍受网络/写入/进程树限制，但读取沿用宿主用户身份）",
            restricted_token.as_str(),
            app_container.as_str(),
            job_limits.as_str(),
            identity.as_str(),
            identity_note,
            read_deny.as_str()
        );
        SandboxStatus {
            platform: "windows",
            restricted_token,
            app_container,
            job_limits,
            read_deny,
            identity,
            detail,
        }
    }

    fn prepare(
        &self,
        cell: &CellSpec,
        policy: &SandboxPolicy,
    ) -> Result<PreparedCell, SandboxError> {
        let (prepared, _) = prepare_inner(cell, policy)?;
        Ok(prepared)
    }

    fn launch(
        &self,
        cell: &CellSpec,
        policy: &SandboxPolicy,
        request: &ExecRequest,
    ) -> Result<ExecResult, SandboxError> {
        let (_, identity) = prepare_inner(cell, policy)?;
        grant_program_read(&identity, &request.program)?;
        match &identity {
            CellIdentity::Restricted { .. } => {
                process::launch_restricted(cell, policy, request, &identity)
            }
            CellIdentity::Container { .. } => {
                process::launch_container(cell, policy, request, &identity)
            }
        }
    }

    fn spawn(
        &self,
        cell: &CellSpec,
        policy: &SandboxPolicy,
        request: &SpawnRequest,
    ) -> Result<SandboxedProcess, SandboxError> {
        let (_, identity) = prepare_inner(cell, policy)?;
        grant_program_read(&identity, &request.program)?;
        match &identity {
            CellIdentity::Restricted { .. } => {
                process::spawn_streaming_restricted(cell, policy, request, &identity)
            }
            CellIdentity::Container { .. } => {
                process::spawn_streaming_container(cell, policy, request, &identity)
            }
        }
    }
}

/// Who the cell's children run as.
///
/// `identity` is `None` when the machine has not run the one-time provisioning:
/// the cell still gets network/write/process isolation, but *reads* stay with
/// the host user, which is exactly what [`SandboxPolicy::deny_read`] cannot
/// tolerate — a policy that asks for read isolation is refused instead of
/// silently weakening it.
pub(crate) enum CellIdentity {
    Container {
        container: OwnedSid,
        account: Option<OwnedSid>,
        identity: Option<identity::SandboxIdentity>,
    },
    Restricted {
        capability: OwnedSid,
        account: Option<OwnedSid>,
        identity: Option<identity::SandboxIdentity>,
    },
}

impl CellIdentity {
    /// The SID whose grant on the cell is checked for writes (capability) or for
    /// the lowbox restricted check (container).
    pub(crate) fn primary(&self) -> &OwnedSid {
        match self {
            CellIdentity::Container { container, .. } => container,
            CellIdentity::Restricted { capability, .. } => capability,
        }
    }

    /// The sandbox account's SID, when the dedicated identity is in use.
    pub(crate) fn account(&self) -> Option<&OwnedSid> {
        match self {
            CellIdentity::Container { account, .. } | CellIdentity::Restricted { account, .. } => {
                account.as_ref()
            }
        }
    }

    pub(crate) fn identity(&self) -> Option<&identity::SandboxIdentity> {
        match self {
            CellIdentity::Container { identity, .. }
            | CellIdentity::Restricted { identity, .. } => identity.as_ref(),
        }
    }
}

/// Establish the cell and return the identities the child must run under.
pub(crate) fn prepare_inner(
    cell: &CellSpec,
    policy: &SandboxPolicy,
) -> Result<(PreparedCell, CellIdentity), SandboxError> {
    crate::backend::ensure_cell_dirs(cell)?;
    // A damaged credential file is a setup error, never a silent downgrade.
    // Never "try the identity and fall back": the availability check decides
    // *before* anything is created whether the child can run as the sandbox
    // account. When it cannot (no SeAssignPrimaryTokenPrivilege /
    // SeIncreaseQuotaPrivilege, i.e. Win32 1314 on the CreateProcessAsUser call),
    // the cell uses the host token — the shape that is fully tested.
    let availability = identity::availability()?;
    let identity = availability.credentials;
    let account = availability.account;

    match policy.mode {
        IsolationMode::RestrictedToken => {
            // Read isolation in this mode comes from running as a *different*
            // principal. A deny ACE is not usable here (a write-restricted token
            // only consults its restricting SIDs for writes), which is why the
            // policy layer refuses `deny_read` for this mode.
            if !policy.deny_read.is_empty() {
                return Err(SandboxError::PolicyRejected(
                    "受限令牌模式无法用 deny_read 表达读隔离，请改用 AppContainer 模式".to_owned(),
                ));
            }
            let capability = token::capability_sid_for(cell)?;
            for root in policy.writable.iter().chain(&policy.readable) {
                let mask = if policy.writable.contains(root) {
                    acl::FILE_ALL
                } else {
                    acl::FILE_READ_EXEC
                };
                acl::grant(
                    root,
                    capability.as_ptr(),
                    mask,
                    acl::INHERIT_CONTAINER_AND_OBJECT,
                )?;
                if let Some(account) = &account {
                    // The write check needs the *normal* SID list to grant too.
                    acl::grant(
                        root,
                        account.as_ptr(),
                        mask,
                        acl::INHERIT_CONTAINER_AND_OBJECT,
                    )?;
                }
            }
            if !acl::has_allow_ace(cell.root(), capability.as_ptr(), acl::FILE_ALL)? {
                return Err(SandboxError::Setup(format!(
                    "沙箱单元授权回读失败: {}",
                    cell.root().display()
                )));
            }
            let mut notes = vec![
                "WRITE_RESTRICTED 令牌：写操作只允许落在已授予单元的 capability SID 上".to_owned(),
            ];
            match (&identity, &account) {
                (Some(credentials), Some(account)) => notes.push(format!(
                    "专用沙箱身份 {}（{}）：读隔离来自账户身份隔离 + 针对该 SID 的受保护路径",
                    credentials.username,
                    account.as_string()
                )),
                _ => notes.push(
                    "未初始化专用沙箱身份：仅写隔离生效，读操作仍以宿主用户身份进行（读隔离不可用）"
                        .to_owned(),
                ),
            }
            let prepared = PreparedCell {
                scope: cell.scope().to_owned(),
                root: cell.root().to_path_buf(),
                mode: policy.mode,
                capability_sid: Some(capability.as_string()),
                container_sid: None,
                protected_paths: Vec::new(),
                sandbox_identity_sid: account.as_ref().map(OwnedSid::as_string),
                notes,
            };
            Ok((
                prepared,
                CellIdentity::Restricted {
                    capability,
                    account,
                    identity,
                },
            ))
        }
        IsolationMode::AppContainer => {
            if !policy.deny_read.is_empty() && identity.is_none() {
                return Err(SandboxError::Setup(format!(
                    "读隔离需要可用的专用沙箱身份，当前不可用: {}。                     未满足时脚本仍受网络/写入/进程树限制，但读取沿用宿主用户身份",
                    availability.note
                )));
            }
            let container = container::ensure_profile(cell.scope())?;
            for root in &policy.writable {
                acl::grant(
                    root,
                    container.as_ptr(),
                    acl::FILE_ALL,
                    acl::INHERIT_CONTAINER_AND_OBJECT,
                )?;
                if let Some(account) = &account {
                    // Lowbox tokens are checked twice (normal SIDs, then the
                    // container SIDs); the account ACE satisfies the first half.
                    acl::grant(
                        root,
                        account.as_ptr(),
                        acl::FILE_ALL,
                        acl::INHERIT_CONTAINER_AND_OBJECT,
                    )?;
                }
            }
            for root in &policy.readable {
                acl::grant(
                    root,
                    container.as_ptr(),
                    acl::FILE_READ_EXEC,
                    acl::INHERIT_CONTAINER_AND_OBJECT,
                )?;
                if let Some(account) = &account {
                    acl::grant(
                        root,
                        account.as_ptr(),
                        acl::FILE_READ_EXEC,
                        acl::INHERIT_CONTAINER_AND_OBJECT,
                    )?;
                }
            }
            if !acl::has_allow_ace(cell.root(), container.as_ptr(), acl::FILE_ALL)? {
                return Err(SandboxError::Setup(format!(
                    "AppContainer 单元授权回读失败: {}",
                    cell.root().display()
                )));
            }
            // Protected paths are denied to the sandbox account. A deny ACE for
            // the container SID would be dead weight: the lowbox check only
            // grants what this crate granted explicitly, and nothing outside the
            // cell does.
            let mut protected = Vec::new();
            if !policy.deny_read.is_empty()
                && let Some(account) = &account
            {
                protected = deny_read_acl::apply(&policy.deny_read, account.as_ptr())?;
            }
            let mut notes = vec![
                "零 capability AppContainer：无网络（含回环）".to_owned(),
                "Job Object：ActiveProcessLimit + ProcessMemoryLimit + KILL_ON_JOB_CLOSE"
                    .to_owned(),
            ];
            if identity.is_some() {
                notes.push(
                    "专用沙箱身份：容器子进程的用户是沙箱账户，受保护路径对该 SID 写 deny-read ACE"
                        .to_owned(),
                );
            } else {
                notes.push(
                    "未初始化专用沙箱身份：容器子进程仍以宿主用户身份读取（读隔离不可用）"
                        .to_owned(),
                );
            }
            let prepared = PreparedCell {
                scope: cell.scope().to_owned(),
                root: cell.root().to_path_buf(),
                mode: policy.mode,
                capability_sid: None,
                container_sid: Some(container.as_string()),
                protected_paths: protected,
                sandbox_identity_sid: account.as_ref().map(OwnedSid::as_string),
                notes,
            };
            Ok((
                prepared,
                CellIdentity::Container {
                    container,
                    account,
                    identity,
                },
            ))
        }
    }
}

/// Grant the sandbox account read access to the program it is asked to run.
///
/// Without it an identity-isolated child cannot map its own executable
/// (`goose` lives under `.duthub/tools`, which is a protected path). To avoid
/// exposing unrelated siblings (e.g. a binary placed inside `Downloads` or
/// `.duthub/`), only the executable file itself is granted a non-inherited
/// read/execute ACE; sibling files in the parent directory remain untouched.
fn grant_program_read(identity: &CellIdentity, program: &Path) -> Result<(), SandboxError> {
    let Some(account) = identity.account() else {
        return Ok(());
    };
    let Some(directory) = program.parent() else {
        return Ok(());
    };
    if !program.is_file() || !grantable_directory(directory) {
        // System/Program Files trees already grant `Users` read (the sandbox
        // account is a member) and rewriting their DACL is not our business —
        // doing it would also fail for a non-administrator caller.
        return Ok(());
    }
    acl::grant(program, account.as_ptr(), acl::FILE_READ_EXEC, 0)?;
    Ok(())
}

/// Whether this crate may rewrite the DACL of `directory` to grant the sandbox
/// account read access.
///
/// Only user-owned locations are considered: the Windows and Program Files trees
/// already grant the built-in `Users` group read access (the sandbox account is a
/// member), and rewriting their DACL is neither needed nor possible for a
/// non-administrator caller.
fn grantable_directory(directory: &Path) -> bool {
    let protected = [
        std::env::var_os("SystemRoot"),
        std::env::var_os("ProgramFiles"),
        std::env::var_os("ProgramFiles(x86)"),
        std::env::var_os("ProgramW6432"),
    ];
    for root in protected.into_iter().flatten() {
        let root = Path::new(&root);
        if directory.starts_with(root) {
            return false;
        }
    }
    true
}

pub(crate) fn provision_identity() -> Result<crate::status::IdentityReport, SandboxError> {
    identity::provision()
}

pub(crate) fn provision_identity_in(
    root: &Path,
) -> Result<crate::status::IdentityReport, SandboxError> {
    identity::provision_in(root)
}

pub(crate) fn set_identity_root(root: Option<std::path::PathBuf>) -> Result<(), SandboxError> {
    identity::set_root(root)
}

pub(crate) fn identity_root() -> Result<std::path::PathBuf, SandboxError> {
    identity::state_dir()
}

pub(crate) fn is_elevated() -> bool {
    identity::is_elevated()
}

/// A SID allocation with the deleter the API that produced it requires.
pub(crate) struct OwnedSid {
    sid: PSID,
    dealloc: SidDealloc,
}

enum SidDealloc {
    /// `ConvertStringSidToSidW` allocates with `LocalAlloc`.
    LocalFree,
    /// AppContainer APIs allocate with `FreeSid`.
    FreeSid,
}

impl OwnedSid {
    pub(crate) fn from_string(text: &str) -> Result<Self, SandboxError> {
        let wide = to_wide(text);
        let mut sid = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string alive for the call;
        // on success the API hands back a LocalAlloc'd SID in `sid`.
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
        if ok == 0 || sid.is_null() {
            return Err(SandboxError::Setup(format!("SID 字符串无效: {text:?}")));
        }
        Ok(Self {
            sid,
            dealloc: SidDealloc::LocalFree,
        })
    }

    pub(crate) fn well_known(kind: i32) -> Result<Self, SandboxError> {
        let mut size = 0u32;
        // SAFETY: a size query with a null SID buffer is the documented shape.
        unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
        if size == 0 {
            return Err(SandboxError::Setup(format!(
                "无法查询 well-known SID {kind}"
            )));
        }
        let mut buffer = vec![0u8; size as usize];
        // SAFETY: `buffer` is at least `size` bytes and `size` is the value the
        // API just asked for.
        let ok = unsafe {
            CreateWellKnownSid(
                kind,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if ok == 0 {
            return Err(SandboxError::Setup(format!(
                "无法创建 well-known SID {kind}"
            )));
        }
        let text = sid_to_string(buffer.as_mut_ptr().cast())?;
        Self::from_string(&text)
    }

    /// Take ownership of a SID returned by an AppContainer API (`FreeSid`).
    pub(crate) fn from_container_api(sid: PSID) -> Self {
        Self {
            sid,
            dealloc: SidDealloc::FreeSid,
        }
    }

    pub(crate) fn as_ptr(&self) -> PSID {
        self.sid
    }

    pub(crate) fn as_string(&self) -> String {
        sid_to_string(self.sid).unwrap_or_else(|_| "<invalid sid>".to_owned())
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        if self.sid.is_null() {
            return;
        }
        match self.dealloc {
            // SAFETY: the SID was allocated by ConvertStringSidToSidW (or a
            // well-known SID round-tripped through it), so LocalFree is the
            // matching deleter.
            SidDealloc::LocalFree => unsafe { LocalFree(self.sid as HLOCAL) },
            // SAFETY: the SID came from CreateAppContainerProfile or
            // DeriveAppContainerSidFromAppContainerName, whose contract is FreeSid.
            SidDealloc::FreeSid => unsafe { FreeSid(self.sid) },
        };
    }
}

/// A Win32 handle closed on drop.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

impl OwnedHandle {
    /// Give up ownership; the caller becomes responsible for closing it.
    pub(crate) fn into_raw(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle is owned by this value and closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// UTF-16 with a trailing NUL.
pub(crate) fn to_wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

pub(crate) fn path_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

pub(crate) fn sid_to_string(sid: PSID) -> Result<String, SandboxError> {
    let mut text = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID owned by the caller for the duration of the
    // call; the API returns a LocalAlloc'd string in `text`.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut text) };
    if ok == 0 || text.is_null() {
        return Err(SandboxError::Setup("SID 无法转换为字符串".to_owned()));
    }
    let mut length = 0usize;
    // SAFETY: `text` is NUL-terminated by contract; the scan stops at the NUL.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the range [0, length) is inside the allocation the API returned.
    let value = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })
        .map_err(|_| SandboxError::Setup("SID 字符串不是合法 UTF-16".to_owned()));
    // SAFETY: `text` was allocated by ConvertStringSidToSidW.
    unsafe { LocalFree(text as HLOCAL) };
    value
}

/// Windows error text for a `GetLastError` value.
pub(crate) fn error_text(code: u32) -> String {
    let message = std::io::Error::from_raw_os_error(code as i32).to_string();
    format!("{message} (Win32 {code})")
}

/// The thread's last-error value (each call site is a fresh Win32 failure).
pub(crate) fn last_error() -> u32 {
    // SAFETY: reading the thread-local last-error value.
    unsafe { GetLastError() }
}
