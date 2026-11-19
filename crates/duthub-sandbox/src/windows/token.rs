//! Kernel identities: the dedicated sandbox account's logon + restricted token,
//! the per-cell capability SID store, and the probes behind `status()`.
//!
//! Derived from openai/codex `codex-rs/windows-sandbox-rs` (Apache-2.0):
//! `token.rs` (`create_readonly_token_with_caps_and_user_from`, the
//! restricting-SID order, `set_default_dacl`, `SeChangeNotifyPrivilege`) and
//! `cap.rs` (per-workspace capability SIDs).
//!
//! # Two token shapes
//!
//! - [`restricted_for_current_user`] — the historical shape: a
//!   `WRITE_RESTRICTED` copy of the *host* token. Writes are limited to the
//!   cell's capability SID; reads keep the host user's access, so this token
//!   **cannot** separate DUTs.
//! - [`restricted_for_identity`] — a `WRITE_RESTRICTED` copy of the *sandbox
//!   account's* logon token. The child's user SID is the sandbox account, which
//!   has no access to the host's profile, `.duthub`, `duthub-data`, other DUTs or
//!   `%USERPROFILE%\.ssh`; reads are therefore separated by the kernel without
//!   touching the host user's own ACLs.
//!
//! Restricting-SID order (matters for audit readability, see Codex): per-cell
//! capability SIDs first, then the token user, the logon SID and `Everyone`.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_SUCCESS, GENERIC_ALL, GetLastError, HANDLE, HLOCAL, LUID, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, AdjustTokenPrivileges, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, EqualSid,
    GetTokenInformation, LUA_TOKEN, LookupPrivilegeValueW, SID_AND_ATTRIBUTES,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenDefaultDacl,
    TokenGroups, TokenRestrictedSids, TokenUser, WRITE_RESTRICTED,
};
use windows_sys::Win32::Security::{
    LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT, LUID_AND_ATTRIBUTES, LogonUserW,
    SE_PRIVILEGE_REMOVED, TokenPrivileges,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::acl::{self, PinnedPath};
use super::identity::SandboxIdentity;
use super::{OwnedHandle, OwnedSid, error_text};
use crate::policy::{CellSpec, SandboxError};
use crate::status::Capability;

/// `S-1-5-32-544`, the built-in Administrators alias.
const BUILTIN_ADMINISTRATORS: &str = "S-1-5-32-544";
/// `SE_GROUP_ENABLED`: the group takes part in access checks.
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
/// `SE_GROUP_USE_FOR_DENY_ONLY`: the group is only consulted for deny ACEs.
const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x0000_0010;
/// Header of a per-cell capability file (see [`capability_sid_for`]).
const CAPABILITY_HEADER: &str = "duthub-sandbox-cell";
const CAPABILITY_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Capability SIDs
// ---------------------------------------------------------------------------

/// Directory that owns the per-cell capability SIDs.
///
/// It lives under `<work_dir>/duthub-data/sandbox/`, i.e. **outside** the cell:
/// the cell is the only tree a sandboxed Agent may write, so an identity file
/// stored there would let the Agent mint a capability SID of its own choosing
/// (and with it a grant ACE on any path it can reach).
fn capability_dir(cell: &CellSpec) -> PathBuf {
    cell.work_dir()
        .join("duthub-data")
        .join("sandbox")
        .join("capabilities")
}

fn capability_file(cell: &CellSpec) -> PathBuf {
    capability_dir(cell).join(format!("{}.cell", cell.dut_key()))
}

/// The cell's capability SID, created on first use and verified on every use.
///
/// Validation on load (fail closed, never "mint a second identity"):
/// - the recorded root must be *this* cell's canonical root;
/// - the SID must be one this crate generates (`S-1-5-21-a-b-c-d`, last
///   sub-authority ≥ 1000) and must not be a well-known SID;
/// - the file must not be writable by `Everyone`/`Users`.
pub(crate) fn capability_sid_for(cell: &CellSpec) -> Result<OwnedSid, SandboxError> {
    let path = capability_file(cell);
    let expected_root = path_key(cell.root());
    match std::fs::read_to_string(&path) {
        Ok(text) => load_capability(&path, &text, &expected_root),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let sid = random_capability_sid()?;
            write_capability(cell, &path, &sid, &expected_root)?;
            Ok(sid)
        }
        Err(error) => Err(SandboxError::Setup(format!(
            "无法读取沙箱 capability SID 文件 {}: {error}",
            path.display()
        ))),
    }
}

fn load_capability(path: &Path, text: &str, expected_root: &str) -> Result<OwnedSid, SandboxError> {
    let mut lines = text.lines();
    if lines.next() != Some(CAPABILITY_HEADER) {
        return Err(SandboxError::Setup(format!(
            "沙箱 capability 文件格式不符: {}",
            path.display()
        )));
    }
    let (mut version, mut root, mut sid) = (None, None, None);
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(SandboxError::Setup(format!(
                "沙箱 capability 文件含无法解析的行: {}",
                path.display()
            )));
        };
        match key {
            "version" => {
                version = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| SandboxError::Setup("capability 版本号无效".to_owned()))?,
                )
            }
            "root" => root = Some(value.to_owned()),
            "sid" => sid = Some(value.to_owned()),
            other => {
                return Err(SandboxError::Setup(format!(
                    "沙箱 capability 文件含未知字段 {other:?}"
                )));
            }
        }
    }
    if version != Some(CAPABILITY_VERSION) {
        return Err(SandboxError::Setup(format!(
            "沙箱 capability 文件版本不匹配（期望 {CAPABILITY_VERSION}）"
        )));
    }
    if root.as_deref() != Some(expected_root) {
        return Err(SandboxError::Setup(format!(
            "记录在案的 capability SID 属于另一个沙箱单元（记录 {} ≠ 本单元 {expected_root}），拒绝复用",
            root.as_deref().unwrap_or("<缺失>")
        )));
    }
    let sid = sid.ok_or_else(|| SandboxError::Setup("capability 文件缺少 sid".to_owned()))?;
    if !is_generated_capability(&sid) {
        return Err(SandboxError::Setup(format!(
            "capability SID 不是本 crate 生成的形状（拒绝使用被篡改的身份）: {sid}"
        )));
    }
    verify_capability_file_dacl(path)?;
    OwnedSid::from_string(&sid)
}

fn verify_capability_file_dacl(path: &Path) -> Result<(), SandboxError> {
    let pinned = PinnedPath::open(path)?;
    let public = [
        OwnedSid::well_known(windows_sys::Win32::Security::WinWorldSid)?,
        OwnedSid::well_known(windows_sys::Win32::Security::WinBuiltinUsersSid)?,
    ];
    if acl::has_any_allow_ace(
        &pinned,
        &public.iter().collect::<Vec<_>>(),
        acl::write_rights(),
    )? {
        return Err(SandboxError::Setup(format!(
            "capability 文件可被 Everyone/Users 写入，拒绝使用: {}",
            path.display()
        )));
    }
    Ok(())
}

fn write_capability(
    cell: &CellSpec,
    path: &Path,
    sid: &OwnedSid,
    root: &str,
) -> Result<(), SandboxError> {
    let dir = capability_dir(cell);
    let pinned_dir = PinnedPath::create_directory(&dir)?;
    protect_state_dir(&pinned_dir)?;
    let text = format!(
        "{CAPABILITY_HEADER}\nversion={CAPABILITY_VERSION}\nroot={root}\nsid={}\n",
        sid.as_string()
    );
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text).map_err(|error| {
        SandboxError::Setup(format!(
            "无法写入 capability 文件 {}: {error}",
            temporary.display()
        ))
    })?;
    let pinned_file = PinnedPath::open(&temporary)?;
    protect_state_dir(&pinned_file)?;
    std::fs::rename(&temporary, path).map_err(|error| {
        SandboxError::Setup(format!(
            "无法启用 capability 文件 {}: {error}",
            path.display()
        ))
    })?;
    verify_capability_file_dacl(path)
}

fn protect_state_dir(pinned: &PinnedPath) -> Result<(), SandboxError> {
    let current = current_user_sid()?;
    let system = OwnedSid::well_known(windows_sys::Win32::Security::WinLocalSystemSid)?;
    let admins = OwnedSid::well_known(windows_sys::Win32::Security::WinBuiltinAdministratorsSid)?;
    acl::set_protected_dacl(
        pinned,
        &[
            (current.as_ptr(), acl::FILE_ALL),
            (system.as_ptr(), acl::FILE_ALL),
            (admins.as_ptr(), acl::FILE_ALL),
        ],
        acl::INHERIT_CONTAINER_AND_OBJECT,
    )
}

/// Normalised path key: canonical form without the `\\?\` prefix, `\`
/// separators, no trailing separator, lowercase (NTFS compares case-folded).
fn path_key(path: &Path) -> String {
    let text = std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// `S-1-5-21-<a>-<b>-<c>-<d>` with a non-well-known last sub-authority.
fn is_generated_capability(sid: &str) -> bool {
    let mut parts = sid.split('-');
    let mut prefix = Vec::new();
    for _ in 0..4 {
        match parts.next() {
            Some(part) => prefix.push(part),
            None => return false,
        }
    }
    if prefix != ["S", "1", "5", "21"] {
        return false;
    }
    let mut sub_authorities = Vec::new();
    for part in parts {
        let Ok(value) = part.parse::<u32>() else {
            return false;
        };
        sub_authorities.push(value);
    }
    sub_authorities.len() == 4 && sub_authorities[3] >= 1000
}

fn random_capability_sid() -> Result<OwnedSid, SandboxError> {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let word = |offset: usize| {
        u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    };
    OwnedSid::from_string(&format!(
        "S-1-5-21-{}-{}-{}-{}",
        word(0),
        word(4),
        word(8),
        // Keep the last sub-authority away from the well-known RID ranges.
        1000 + word(12) % 1_000_000
    ))
}

// ---------------------------------------------------------------------------
// Logons
// ---------------------------------------------------------------------------

/// Log the sandbox account on and return `(token, account SID)`.
///
/// `LOGON32_LOGON_INTERACTIVE` is what a local account can use without a
/// password-expiry or service-account special case; the token is a primary
/// token, so it can be used for `CreateProcessAsUserW` directly.
pub(crate) fn logon_identity(
    identity: &SandboxIdentity,
) -> Result<(OwnedHandle, OwnedSid), SandboxError> {
    let username = super::to_wide(&identity.username);
    let domain = super::to_wide(".");
    let password = super::to_wide(&identity.password);
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: all three strings are NUL-terminated and alive for the call; the
    // API writes an owned primary token handle into `token`.
    let ok = unsafe {
        LogonUserW(
            username.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if ok == 0 || token.is_null() {
        return Err(SandboxError::Setup(format!(
            "无法以沙箱账户 {} 登录（账户被禁用、口令过期或凭据不符）: {}",
            identity.username,
            error_text(last_error())
        )));
    }
    let token = OwnedHandle(token);
    let user = user_sid_of(token.0)?;
    if user.as_string() == current_user_sid()?.as_string() {
        return Err(SandboxError::Setup(
            "沙箱账户的 SID 与当前用户相同，身份隔离不成立".to_owned(),
        ));
    }
    if groups_enabled(token.0, BUILTIN_ADMINISTRATORS)? {
        return Err(SandboxError::Setup(
            "沙箱账户是启用的管理员，拒绝作为受限身份使用".to_owned(),
        ));
    }
    Ok((token, user))
}

/// Build the sandbox identity's `WRITE_RESTRICTED` token for one cell.
pub(crate) fn restricted_for_identity(
    identity: &SandboxIdentity,
    capability: &OwnedSid,
) -> Result<(OwnedHandle, OwnedSid), SandboxError> {
    let (base, account) = logon_identity(identity)?;
    let logon = logon_sid_of(base.0)?;
    // The token user is a restricting SID so that write access granted to the
    // sandbox account itself (the cell's non-inherited owner ACE) also satisfies
    // the second half of the write check.
    let token = create_restricted(base.0, capability, &[account.as_ptr()], logon.as_ptr())?;
    Ok((token, account))
}

/// Build a `WRITE_RESTRICTED` token from the *host* process token.
///
/// This is the fallback when no sandbox account is provisioned: write isolation
/// only, no read separation (documented in ADR-12).
pub(crate) fn restricted_for_current_user(
    capability: &OwnedSid,
) -> Result<OwnedHandle, SandboxError> {
    // The new token handle inherits the access rights of `base`, and
    // `AdjustTokenPrivileges` below needs TOKEN_ADJUST_PRIVILEGES on it.
    let base = open_current_token(
        TOKEN_DUPLICATE
            | TOKEN_QUERY
            | TOKEN_ASSIGN_PRIMARY
            | TOKEN_ADJUST_DEFAULT
            | TOKEN_ADJUST_PRIVILEGES,
    )?;
    let logon = logon_sid_of(base.0)?;
    create_restricted(base.0, capability, &[], logon.as_ptr())
}

/// `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED)`
/// plus the two follow-up calls the kernel needs before the token is usable.
///
/// Mirrors OpenAI Codex `codex-rs/windows-sandbox-rs`: `Everyone` (`WinWorldSid`)
/// is required in `TokenRestrictedSids` so `.NET`/PowerShell/system DLLs can open
/// standard kernel objects during process initialization.
fn create_restricted(
    base: HANDLE,
    capability: &OwnedSid,
    additional_restricting: &[windows_sys::Win32::Security::PSID],
    logon: windows_sys::Win32::Security::PSID,
) -> Result<OwnedHandle, SandboxError> {
    let everyone = OwnedSid::well_known(windows_sys::Win32::Security::WinWorldSid)?;
    let mut entries: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    // Exact order: capabilities..., extra restricting SIDs, logon SID, Everyone.
    for sid in std::iter::once(capability.as_ptr()).chain(additional_restricting.iter().copied()) {
        entries.push(SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: 0,
        });
    }
    entries.push(SID_AND_ATTRIBUTES {
        Sid: logon,
        Attributes: 0,
    });
    entries.push(SID_AND_ATTRIBUTES {
        Sid: everyone.as_ptr(),
        Attributes: 0,
    });
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `base` is a primary token handle with TOKEN_DUPLICATE; `entries`
    // is a live array whose SIDs outlive the call; `token` receives an owned
    // handle wrapped by `OwnedHandle` below.
    let ok = unsafe {
        CreateRestrictedToken(
            base,
            DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            entries.len() as u32,
            entries.as_mut_ptr(),
            &mut token,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Setup(format!(
            "CreateRestrictedToken 失败: {}",
            error_text(last_error())
        )));
    }
    let token = OwnedHandle(token);
    set_default_dacl(token.0, logon)?;
    enable_privilege(token.0, "SeChangeNotifyPrivilege")?;
    Ok(token)
}

/// Replace the token's default DACL with one ACE for the logon SID.
///
/// Without it a restricted child dies during DLL initialisation
/// (`STATUS_DLL_INIT_FAILED`) because the objects it creates would grant the
/// *host* user access; verified 2026-11-25 (ADR-12 §5).
fn set_default_dacl(
    token: HANDLE,
    logon: windows_sys::Win32::Security::PSID,
) -> Result<(), SandboxError> {
    /// `TokenDefaultDacl` payload: a pointer to the ACL to install.
    #[repr(C)]
    struct DefaultDaclInfo {
        default_dacl: *mut ACL,
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_ALL,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: 0,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: logon.cast::<u16>(),
        },
    };
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: one explicit entry with a live trustee SID, no old ACL, result
    // stored in `dacl`.
    let code = unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &mut dacl) };
    if code != ERROR_SUCCESS || dacl.is_null() {
        return Err(SandboxError::Setup(format!(
            "无法构造令牌默认 DACL: {}",
            error_text(code)
        )));
    }
    let mut info = DefaultDaclInfo { default_dacl: dacl };
    // SAFETY: the payload is a `DefaultDaclInfo` of exactly the size passed; the
    // ACL outlives the call and the kernel copies it into the token.
    let ok = unsafe {
        windows_sys::Win32::Security::SetTokenInformation(
            token,
            TokenDefaultDacl,
            (&raw mut info).cast(),
            std::mem::size_of::<DefaultDaclInfo>() as u32,
        )
    };
    // SAFETY: `dacl` was allocated by `SetEntriesInAclW` (LocalAlloc).
    unsafe { LocalFree(dacl as HLOCAL) };
    if ok == 0 {
        return Err(SandboxError::Setup(format!(
            "SetTokenInformation(TokenDefaultDacl) 失败: {}",
            error_text(last_error())
        )));
    }
    Ok(())
}

/// `DISABLE_MAX_PRIVILEGE` keeps `SeChangeNotifyPrivilege` but disabled; the
/// child needs it to traverse the (host-owned) parents of its cell.
fn enable_privilege(token: HANDLE, name: &str) -> Result<(), SandboxError> {
    let wide = super::to_wide(name);
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    // SAFETY: `wide` is NUL-terminated and alive for the call; `luid` receives
    // the privilege identifier.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), wide.as_ptr(), &mut luid) } == 0 {
        return Err(SandboxError::Setup(format!(
            "LookupPrivilegeValueW({name}) 失败: {}",
            error_text(last_error())
        )));
    }
    let mut privileges: TOKEN_PRIVILEGES = unsafe {
        // SAFETY: plain-old-data; zeroed before the single entry is filled.
        std::mem::zeroed()
    };
    privileges.PrivilegeCount = 1;
    privileges.Privileges[0].Luid = luid;
    privileges.Privileges[0].Attributes = 0x0000_0002; // SE_PRIVILEGE_ENABLED
    // SAFETY: the structure is fully initialised and lives for the call.
    let ok = unsafe {
        AdjustTokenPrivileges(
            token,
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(SandboxError::Setup(format!(
            "AdjustTokenPrivileges({name}) 失败: {}",
            error_text(last_error())
        )));
    }
    let leftover = last_error();
    if leftover != 0 {
        return Err(SandboxError::Setup(format!(
            "AdjustTokenPrivileges({name}) 未生效: {}",
            error_text(leftover)
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Token inspection
// ---------------------------------------------------------------------------

pub(crate) fn open_current_token(access: u32) -> Result<OwnedHandle, SandboxError> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle valid in this process;
    // `token` receives an owned handle.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) };
    if ok == 0 || token.is_null() {
        return Err(SandboxError::Setup(format!(
            "OpenProcessToken 失败: {}",
            error_text(last_error())
        )));
    }
    Ok(OwnedHandle(token))
}

/// Raw `GetTokenInformation` into a `Vec<u8>` (size query then read).
pub(crate) fn token_information(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<Vec<u8>, SandboxError> {
    let mut needed = 0u32;
    // SAFETY: a size query with a null buffer is the documented shape.
    unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(SandboxError::Setup(format!(
            "令牌信息查询 {class} 无返回长度: {}",
            error_text(last_error())
        )));
    }
    let mut buffer = vec![0u8; needed as usize];
    let mut returned = 0u32;
    // SAFETY: the buffer has `needed` bytes, the size the previous call asked for.
    let ok = unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut returned,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Setup(format!(
            "令牌信息查询 {class} 失败: {}",
            error_text(last_error())
        )));
    }
    buffer.truncate(returned as usize);
    Ok(buffer)
}

fn read_u32(buffer: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    if let Some(slice) = buffer.get(offset..offset + 4) {
        bytes.copy_from_slice(slice);
    }
    u32::from_ne_bytes(bytes)
}

/// Every `(SID, attributes)` pair of a `TOKEN_GROUPS`-shaped buffer.
fn token_group_entries(buffer: &[u8]) -> Vec<(windows_sys::Win32::Security::PSID, u32)> {
    let count = read_u32(buffer, 0) as usize;
    let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
    let stride = std::mem::size_of::<SID_AND_ATTRIBUTES>();
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let start = offset + index * stride;
        if start + stride > buffer.len() {
            break;
        }
        // SAFETY: the bounds check above guarantees a full SID_AND_ATTRIBUTES
        // record inside the buffer; the struct is plain-old-data and only read.
        let entry: SID_AND_ATTRIBUTES =
            unsafe { std::ptr::read_unaligned(buffer.as_ptr().add(start).cast()) };
        entries.push((entry.Sid, entry.Attributes));
    }
    entries
}

/// The interactive logon SID (`S-1-5-5-x-y`) of `token`.
pub(crate) fn logon_sid_of(token: HANDLE) -> Result<OwnedSid, SandboxError> {
    let groups = token_information(token, TokenGroups)?;
    for (sid, attributes) in token_group_entries(&groups) {
        if attributes & 0xC000_0000 == 0xC000_0000 {
            return OwnedSid::from_string(&super::sid_to_string(sid)?);
        }
    }
    Err(SandboxError::Setup(
        "令牌没有交互登录 SID，无法建立沙箱".to_owned(),
    ))
}

/// The token's user SID.
pub(crate) fn user_sid_of(token: HANDLE) -> Result<OwnedSid, SandboxError> {
    let buffer = token_information(token, TokenUser)?;
    // `TOKEN_USER` is a `SID_AND_ATTRIBUTES` at offset 0.
    if buffer.len() < std::mem::size_of::<SID_AND_ATTRIBUTES>() {
        return Err(SandboxError::Setup("令牌用户信息不完整".to_owned()));
    }
    // SAFETY: the length check above guarantees a full SID_AND_ATTRIBUTES record
    // inside the buffer returned by GetTokenInformation; read-only POD access.
    let entry: SID_AND_ATTRIBUTES = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
    OwnedSid::from_string(&super::sid_to_string(entry.Sid)?)
}

pub(crate) fn current_user_sid() -> Result<OwnedSid, SandboxError> {
    let token = open_current_token(TOKEN_QUERY)?;
    user_sid_of(token.0)
}

/// Whether `sid` is an enabled (not deny-only) group of `token`.
pub(crate) fn groups_enabled(token: HANDLE, wanted: &str) -> Result<bool, SandboxError> {
    if let Ok(sid) = OwnedSid::from_string(wanted) {
        let groups = token_information(token, TokenGroups)?;
        for (entry, attributes) in token_group_entries(&groups) {
            // SAFETY: both SIDs are valid for the comparison; EqualSid only reads.
            if unsafe { EqualSid(entry, sid.as_ptr()) } != 0 {
                let enabled = attributes & SE_GROUP_ENABLED != 0
                    && attributes & SE_GROUP_USE_FOR_DENY_ONLY == 0;
                return Ok(enabled);
            }
        }
    }
    Ok(false)
}

/// Whether the current process token has an enabled Administrators group.
pub(crate) fn is_elevated() -> bool {
    open_current_token(TOKEN_QUERY)
        .and_then(|token| groups_enabled(token.0, BUILTIN_ADMINISTRATORS))
        .unwrap_or(false)
}

/// Whether the token carries `sid` in its restricted-SID list.
pub(crate) fn restricted_sids_contain(token: HANDLE, sid: &OwnedSid) -> Result<bool, SandboxError> {
    let buffer = token_information(token, TokenRestrictedSids)?;
    for (entry, _) in token_group_entries(&buffer) {
        // SAFETY: both SIDs are valid for the duration of the comparison.
        if unsafe { EqualSid(entry, sid.as_ptr()) } != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

/// Return caller rights missing from the process token for a cross-account launch.
///
/// Windows can enable a held but disabled privilege during CreateProcessAsUser.
/// Reading the token leaves its privileges unchanged.
pub(crate) fn missing_create_process_privileges() -> Result<Vec<&'static str>, SandboxError> {
    let token = open_current_token(TOKEN_QUERY)?;
    let buffer = token_information(token.0, TokenPrivileges)?;
    const ENTRY_SIZE: usize = std::mem::size_of::<LUID_AND_ATTRIBUTES>();
    let count = read_u32(&buffer, 0) as usize;
    let entries = buffer
        .get(4..)
        .and_then(|bytes| {
            count
                .checked_mul(ENTRY_SIZE)
                .and_then(|size| bytes.get(..size))
        })
        .ok_or_else(|| SandboxError::Setup("Incomplete Windows token privilege list".to_owned()))?;
    let mut missing = Vec::new();
    for name in ["SeAssignPrimaryTokenPrivilege", "SeIncreaseQuotaPrivilege"] {
        let wide = super::to_wide(name);
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        // SAFETY: the name is NUL-terminated and luid is a live output buffer.
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), wide.as_ptr(), &mut luid) } == 0 {
            return Err(SandboxError::Setup(format!(
                "Could not resolve {name}: {}",
                error_text(last_error())
            )));
        }
        if !token_contains_privilege(entries, &luid) {
            missing.push(name);
        }
    }
    Ok(missing)
}

fn token_contains_privilege(entries: &[u8], luid: &LUID) -> bool {
    entries
        .as_chunks::<{ std::mem::size_of::<LUID_AND_ATTRIBUTES>() }>()
        .0
        .iter()
        .any(|entry| {
            read_u32(entry, 0) == luid.LowPart
                && read_u32(entry, 4) as i32 == luid.HighPart
                && read_u32(entry, 8) & SE_PRIVILEGE_REMOVED == 0
        })
}
/// Probe: can this host mint a restricted token at all? Creates and closes one;
/// never starts a process.
pub(crate) fn probe_restricted() -> Capability {
    let capability = match random_capability_sid() {
        Ok(sid) => sid,
        Err(_) => return Capability::Unverified,
    };
    match restricted_for_current_user(&capability) {
        Ok(token) => match token_information(token.0, TokenRestrictedSids) {
            Ok(buffer) if read_u32(&buffer, 0) > 0 => Capability::Verified,
            _ => Capability::Unverified,
        },
        Err(_) => Capability::Unverified,
    }
}

fn last_error() -> u32 {
    // SAFETY: reading the thread-local last-error value.
    unsafe { GetLastError() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_presence_distinguishes_disabled_removed_and_absent() {
        let requested = LUID {
            LowPart: 3,
            HighPart: 0,
        };
        let entry = |attributes: u32| {
            [requested.LowPart, requested.HighPart as u32, attributes]
                .into_iter()
                .flat_map(u32::to_ne_bytes)
                .collect::<Vec<_>>()
        };
        assert!(token_contains_privilege(&entry(0), &requested));
        assert!(!token_contains_privilege(
            &entry(SE_PRIVILEGE_REMOVED),
            &requested
        ));
        assert!(!token_contains_privilege(&[], &requested));
        assert!(missing_create_process_privileges().is_ok());
    }
    #[test]
    fn generated_capability_sids_are_recognised_and_forgeries_are_not() {
        assert!(is_generated_capability("S-1-5-21-1-2-3-1000"));
        assert!(is_generated_capability("S-1-5-21-4294967295-1-2-1001"));
        assert!(
            !is_generated_capability("S-1-5-21-1-2-3-999"),
            "低于 1000 视为非本 crate 生成"
        );
        assert!(
            !is_generated_capability("S-1-5-32-544"),
            "内置组不是 capability"
        );
        assert!(
            !is_generated_capability("S-1-15-2-1"),
            "AppContainer SID 不是 capability"
        );
        assert!(!is_generated_capability("S-1-5-21-1-2-3"), "缺少子授权");
        assert!(!is_generated_capability("garbage"));
    }

    #[test]
    fn capability_files_are_scoped_to_their_cell() {
        let work = std::env::temp_dir().join(format!("duthub-cap-{}", uuid::Uuid::new_v4()));
        let cell = CellSpec::for_dut(&work, "dut-a").expect("cell");
        let other = CellSpec::for_dut(&work, "dut-b").expect("other");
        std::fs::create_dir_all(cell.root()).expect("cell root");
        std::fs::create_dir_all(other.root()).expect("other root");
        let sid = capability_sid_for(&cell).expect("create capability");
        assert!(is_generated_capability(&sid.as_string()));
        // Idempotent: the same file yields the same SID.
        assert_eq!(
            capability_sid_for(&cell).unwrap().as_string(),
            sid.as_string()
        );
        // A different cell gets a different SID and cannot adopt this file.
        assert_ne!(
            capability_sid_for(&other).unwrap().as_string(),
            sid.as_string()
        );
        // Copying another cell's file verbatim must be refused: the recorded
        // root proves the SID belongs to a different unit.
        let text = std::fs::read_to_string(capability_file(&cell)).expect("read capability");
        std::fs::write(capability_file(&other), text).expect("write forged");
        match capability_sid_for(&other) {
            Err(SandboxError::Setup(_)) => {}
            Err(other) => panic!("forged capability must be a setup error, got {other:?}"),
            Ok(_) => panic!("capability of another cell must be refused"),
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn a_well_known_sid_in_the_capability_file_is_refused() {
        let work = std::env::temp_dir().join(format!("duthub-cap2-{}", uuid::Uuid::new_v4()));
        let cell = CellSpec::for_dut(&work, "dut-c").expect("cell");
        std::fs::create_dir_all(cell.root()).expect("cell root");
        let _ = capability_sid_for(&cell).expect("create");
        let path = capability_file(&cell);
        let text = format!(
            "{CAPABILITY_HEADER}\nversion={CAPABILITY_VERSION}\nroot={}\nsid=S-1-5-32-544\n",
            path_key(cell.root())
        );
        std::fs::write(&path, text).expect("write");
        match capability_sid_for(&cell) {
            Err(SandboxError::Setup(_)) => {}
            Err(other) => panic!("builtin SID must be a setup error, got {other:?}"),
            Ok(_) => panic!("built-in SID must be refused as a capability"),
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn path_keys_are_case_and_separator_insensitive() {
        let work = std::env::temp_dir().join(format!("duthub-key-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&work).expect("dir");
        let a = path_key(&work);
        let b = path_key(Path::new(&a.to_ascii_uppercase().replace('\\', "/")));
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&work);
    }
}
