//! Path ACLs for sandbox identities: grants, deny-read ACEs, protected host
//! directories and read-back verification.
//!
//! Every write happens on a handle that was opened without traversing a reparse
//! point ([`super::no_reparse_dir`]) and is followed by a read-back of the DACL;
//! a path that does not show the ACE is an error, never a silent success. ACEs
//! are merged into the existing DACL (never replacing it), so unrelated
//! principals keep their access and repeated launches are idempotent.
//!
//! The handle-based shape is ported from openai/codex
//! `codex-rs/windows-sandbox-rs` (Apache-2.0), `acl.rs`/`deny_read_acl.rs`
//! (pin the directory, then mutate the security descriptor).

use std::path::Path;

use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, LocalFree, WIN32_ERROR};
use windows_sys::Win32::Security::Authorization::{
    DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, SE_FILE_OBJECT,
    SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, EqualSid, GetAce,
    OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_READ_ATTRIBUTES, FILE_WRITE_EA, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};

use super::no_reparse_dir::{
    OpenDisposition, SHARE_READ_WRITE, open_directory_no_reparse, open_file_no_reparse,
};
use super::{OwnedHandle, OwnedSid, error_text};
use crate::policy::SandboxError;
use crate::status::Capability;

/// Full file access, used for the cell root (inherited by its children).
pub(crate) const FILE_ALL: u32 = FILE_ALL_ACCESS;
/// Read + execute, used for extra read roots.
pub(crate) const FILE_READ_EXEC: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
/// ACEs apply to the directory itself and to everything created under it.
pub(crate) const INHERIT_CONTAINER_AND_OBJECT: u32 = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;

/// Every right that can change or delete content: used when proving that a
/// protected host directory is not writable by a non-administrator group.
const WRITE_RIGHTS: u32 = FILE_GENERIC_WRITE | DELETE | WRITE_DAC | WRITE_OWNER | FILE_WRITE_EA;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
/// Traverse + read the DACL + rewrite it: what a pinned security write needs.
const PINNED_DIRECTORY_ACCESS: u32 =
    windows_sys::Win32::Storage::FileSystem::FILE_TRAVERSE | READ_CONTROL | WRITE_DAC;
const PINNED_FILE_ACCESS: u32 = FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC;

/// A path already opened without traversing a reparse point.
///
/// The handle must stay alive for the security write; dropping it is the end of
/// the guarantee that the ACE lands on the validated object.
pub(crate) struct PinnedPath {
    handle: OwnedHandle,
}

impl PinnedPath {
    /// Open an existing directory or file, refusing any reparse point.
    pub(crate) fn open(path: &Path) -> Result<Self, SandboxError> {
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            SandboxError::Setup(format!("沙箱路径不可用 {}: {error}", path.display()))
        })?;
        if is_reparse(&metadata) {
            return Err(SandboxError::Setup(format!(
                "沙箱路径是重解析点（junction/符号链接），已拒绝: {}",
                path.display()
            )));
        }
        let handle = if metadata.is_dir() {
            open_directory_no_reparse(
                path,
                PINNED_DIRECTORY_ACCESS,
                SHARE_READ_WRITE,
                OpenDisposition::OpenExisting,
            )?
        } else {
            open_file_no_reparse(path, PINNED_FILE_ACCESS, SHARE_READ_WRITE)?
        };
        Ok(Self { handle })
    }

    /// Create the final directory component (parents must exist) and pin it.
    pub(crate) fn create_directory(path: &Path) -> Result<Self, SandboxError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                SandboxError::Setup(format!("无法创建目录 {}: {error}", parent.display()))
            })?;
        }
        let handle = open_directory_no_reparse(
            path,
            PINNED_DIRECTORY_ACCESS,
            SHARE_READ_WRITE,
            OpenDisposition::OpenOrCreate,
        )?;
        Ok(Self { handle })
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.handle.0
    }
}

fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Grant `mask` to `sid` on `path`; returns whether an ACE was added.
pub(crate) fn grant(
    path: &Path,
    sid: PSID,
    mask: u32,
    inheritance: u32,
) -> Result<bool, SandboxError> {
    let pinned = PinnedPath::open(path)?;
    grant_pinned(&pinned, sid, mask, inheritance)
}

/// Deny read + execute of `path` to `sid`; returns whether an ACE was added.
pub(crate) fn deny_read(path: &Path, sid: PSID) -> Result<bool, SandboxError> {
    let pinned = PinnedPath::open(path)?;
    deny_read_pinned(&pinned, sid)
}

pub(crate) fn grant_pinned(
    pinned: &PinnedPath,
    sid: PSID,
    mask: u32,
    inheritance: u32,
) -> Result<bool, SandboxError> {
    if has_ace(pinned.handle(), sid, ACCESS_ALLOWED_ACE_TYPE, mask)? {
        return Ok(false);
    }
    merge(
        pinned.handle(),
        explicit_access(sid, mask, GRANT_ACCESS, inheritance),
    )?;
    if !has_ace(pinned.handle(), sid, ACCESS_ALLOWED_ACE_TYPE, mask)? {
        return Err(SandboxError::Setup(
            "授权 ACE 写入后回读不到（DACL 未生效）".to_owned(),
        ));
    }
    Ok(true)
}

pub(crate) fn deny_read_pinned(pinned: &PinnedPath, sid: PSID) -> Result<bool, SandboxError> {
    let mask = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
    if has_ace(pinned.handle(), sid, ACCESS_DENIED_ACE_TYPE, mask)? {
        return Ok(false);
    }
    merge(
        pinned.handle(),
        explicit_access(sid, mask, DENY_ACCESS, INHERIT_CONTAINER_AND_OBJECT),
    )?;
    if !has_ace(pinned.handle(), sid, ACCESS_DENIED_ACE_TYPE, mask)? {
        return Err(SandboxError::Setup(
            "拒绝读取 ACE 写入后回读不到（DACL 未生效）".to_owned(),
        ));
    }
    Ok(true)
}

/// Remove the ACEs this crate added for `sid` (used to roll back a partial
/// deny-read setup).
pub(crate) fn revoke_pinned(pinned: &PinnedPath, sid: PSID) -> Result<(), SandboxError> {
    let (dacl, descriptor) = dacl_of(pinned.handle())?;
    // Keep the security descriptor alive until `SetEntriesInAclW` finishes
    // reading the `ace_sid` pointers that point inside it.
    let _descriptor_guard = std::mem::DropGuard::new(descriptor, |ptr| {
        // SAFETY: `ptr` came from `GetSecurityInfo` (LocalAlloc).
        unsafe { LocalFree(ptr as HLOCAL) };
    });
    let count = usize::from(unsafe {
        // SAFETY: `dacl` is a valid ACL pointer taken from the descriptor.
        (*dacl).AceCount
    });
    let mut keep: Vec<EXPLICIT_ACCESS_W> = Vec::new();
    for index in 0..count {
        let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `index` is below `AceCount`; `ace` receives a pointer inside
        // the descriptor's DACL.
        if unsafe { GetAce(dacl, index as u32, &mut ace) } == 0 || ace.is_null() {
            continue;
        }
        // SAFETY: every ACE starts with an `ACE_HEADER`; read-only access.
        let header: ACE_HEADER = unsafe { std::ptr::read_unaligned(ace.cast()) };
        if usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>() {
            continue;
        }
        // SAFETY: the size check above guarantees a full access ACE; the SID
        // starts after the mask field inside the same allocation.
        let ace_sid = unsafe {
            ace.cast::<u8>()
                .add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
        }
        .cast::<std::ffi::c_void>();
        // SAFETY: both SIDs are valid for the comparison; EqualSid only reads.
        if unsafe { EqualSid(ace_sid, sid) } != 0 {
            continue;
        }
        // SAFETY: a full access ACE was validated above.
        let allowed: ACCESS_ALLOWED_ACE = unsafe { std::ptr::read_unaligned(ace.cast()) };
        let mode = if header.AceType == ACCESS_DENIED_ACE_TYPE {
            DENY_ACCESS
        } else {
            GRANT_ACCESS
        };
        keep.push(explicit_access(
            ace_sid,
            allowed.Mask,
            mode,
            u32::from(header.AceFlags),
        ));
    }
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `keep` may be empty (`null` + 0 entries is the documented "empty
    // ACL" form) or point at a live array of explicit entries whose SID
    // pointers remain valid inside `_descriptor_guard`.
    let code = unsafe {
        SetEntriesInAclW(
            keep.len() as u32,
            keep.as_ptr(),
            std::ptr::null(),
            &mut new_dacl,
        )
    };
    if code != 0 {
        return Err(SandboxError::Setup(format!(
            "无法重建 DACL: {}",
            error_text(code)
        )));
    }
    let _new_dacl_guard = std::mem::DropGuard::new(new_dacl, |ptr| {
        // SAFETY: `ptr` was allocated by `SetEntriesInAclW` (LocalAlloc).
        unsafe { LocalFree(ptr as HLOCAL) };
    });
    // SAFETY: the handle is live with WRITE_DAC and `new_dacl` is a
    // self-relative ACL that lives until the call returns.
    let result = unsafe {
        SetSecurityInfo(
            pinned.handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        return Err(SandboxError::Setup(format!(
            "恢复 DACL 失败: {}",
            error_text(result)
        )));
    }
    Ok(())
}

/// Replace the DACL of `pinned` with exactly `grantees`, inheriting nothing.
///
/// Used for the sandbox identity directory: it must not be writable by
/// `Everyone`/`Users`, because the capability SIDs and the DPAPI blob that
/// define the boundary live there.
pub(crate) fn set_protected_dacl(
    pinned: &PinnedPath,
    grantees: &[(PSID, u32)],
    inheritance: u32,
) -> Result<(), SandboxError> {
    let entries: Vec<EXPLICIT_ACCESS_W> = grantees
        .iter()
        .map(|(sid, mask)| explicit_access(*sid, *mask, GRANT_ACCESS, inheritance))
        .collect();
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `entries` is a live array of fully initialised explicit access
    // records whose trustee SIDs outlive the call.
    let code = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null(),
            &mut new_dacl,
        )
    };
    if code != 0 || new_dacl.is_null() {
        return Err(SandboxError::Setup(format!(
            "无法构造保护 DACL: {}",
            error_text(code)
        )));
    }
    // SAFETY: the handle is live with WRITE_DAC; `new_dacl` is a self-relative
    // ACL alive for the call; `PROTECTED_DACL_SECURITY_INFORMATION` drops
    // inherited ACEs so no parent grant can re-open the directory.
    let result = unsafe {
        SetSecurityInfo(
            pinned.handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: `new_dacl` was allocated by `SetEntriesInAclW` (LocalAlloc).
    unsafe { LocalFree(new_dacl as HLOCAL) };
    if result != 0 {
        return Err(SandboxError::Setup(format!(
            "写入保护 DACL 失败: {}",
            error_text(result)
        )));
    }
    Ok(())
}

/// Whether any of `sids` holds an allow ACE covering `mask` (0 = any rights).
pub(crate) fn has_any_allow_ace(
    pinned: &PinnedPath,
    sids: &[&OwnedSid],
    mask: u32,
) -> Result<bool, SandboxError> {
    for sid in sids {
        if has_ace(pinned.handle(), sid.as_ptr(), ACCESS_ALLOWED_ACE_TYPE, mask)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Rights a write ACE could use; exported for the audit assertions in tests.
pub(crate) const fn write_rights() -> u32 {
    WRITE_RIGHTS
}

pub(crate) fn has_allow_ace(path: &Path, sid: PSID, mask: u32) -> Result<bool, SandboxError> {
    let pinned = PinnedPath::open(path)?;
    has_ace(pinned.handle(), sid, ACCESS_ALLOWED_ACE_TYPE, mask)
}

pub(crate) fn has_deny_read_ace(path: &Path, sid: PSID) -> Result<bool, SandboxError> {
    let pinned = PinnedPath::open(path)?;
    has_ace(
        pinned.handle(),
        sid,
        ACCESS_DENIED_ACE_TYPE,
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
    )
}

fn explicit_access(sid: PSID, mask: u32, mode: i32, inheritance: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast::<u16>(),
        },
    }
}

fn merge(handle: HANDLE, entry: EXPLICIT_ACCESS_W) -> Result<(), SandboxError> {
    let (old_dacl, descriptor) = dacl_of(handle)?;
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `entry` is a fully initialised explicit access whose trustee SID
    // is owned by the caller; `old_dacl` belongs to the live descriptor; the API
    // allocates a new ACL in `new_dacl`.
    let code = unsafe { SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl) };
    // SAFETY: `descriptor` came from `GetSecurityInfo` (LocalAlloc).
    unsafe { LocalFree(descriptor as HLOCAL) };
    if code != 0 || new_dacl.is_null() {
        return Err(SandboxError::Setup(format!(
            "SetEntriesInAclW 失败: {}",
            error_text(code)
        )));
    }
    // SAFETY: the handle is live with WRITE_DAC; `new_dacl` is a self-relative
    // ACL alive for the call.
    let result = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: `new_dacl` was allocated by `SetEntriesInAclW` (LocalAlloc).
    unsafe { LocalFree(new_dacl as HLOCAL) };
    if result != 0 {
        return Err(SandboxError::Setup(format!(
            "SetSecurityInfo 失败: {}",
            error_text(result)
        )));
    }
    Ok(())
}

/// The DACL of a handle and the descriptor that owns it. The caller must
/// `LocalFree` the descriptor.
fn dacl_of(handle: HANDLE) -> Result<(*mut ACL, PSECURITY_DESCRIPTOR), SandboxError> {
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the handle is live and opened with READ_CONTROL; the API returns a
    // self-relative descriptor whose DACL pointer is valid until freed.
    let code: WIN32_ERROR = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(SandboxError::Setup(format!(
            "GetSecurityInfo 失败: {}",
            error_text(code)
        )));
    }
    if dacl.is_null() {
        // A null DACL grants everyone everything; refusing is the only safe
        // reading for a sandbox setup path.
        // SAFETY: `descriptor` came from `GetSecurityInfo`.
        unsafe { LocalFree(descriptor as HLOCAL) };
        return Err(SandboxError::Setup(
            "路径没有 DACL（NULL DACL），拒绝在其上建立沙箱".to_owned(),
        ));
    }
    Ok((dacl, descriptor))
}

/// Whether the DACL of `handle` has a `ace_type` ACE for `sid` covering all bits
/// of `mask`. A zero `mask` matches any rights.
fn has_ace(handle: HANDLE, sid: PSID, ace_type: u8, mask: u32) -> Result<bool, SandboxError> {
    let (dacl, descriptor) = dacl_of(handle)?;
    // SAFETY: `dacl` is a valid ACL pointer taken from the live descriptor.
    let count = usize::from(unsafe { (*dacl).AceCount });
    let mut found = false;
    for index in 0..count {
        let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `index` is below `AceCount` and `ace` receives a pointer inside
        // the descriptor's DACL.
        if unsafe { GetAce(dacl, index as u32, &mut ace) } == 0 || ace.is_null() {
            continue;
        }
        // SAFETY: every ACE starts with an `ACE_HEADER`; read-only access.
        let header: ACE_HEADER = unsafe { std::ptr::read_unaligned(ace.cast()) };
        if header.AceType != ace_type
            || usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        {
            continue;
        }
        // SAFETY: allow and deny ACEs share the `ACCESS_ALLOWED_ACE` layout
        // (header, mask, first SID sub-authority); the size check above
        // guarantees the record fits.
        let allowed: ACCESS_ALLOWED_ACE = unsafe { std::ptr::read_unaligned(ace.cast()) };
        if allowed.Mask & mask != mask {
            continue;
        }
        // The SID continues past the struct's first four bytes, so it must be
        // addressed inside the original ACE buffer, never in the local copy.
        // SAFETY: `ace` points at a full ACE record whose recorded size passed
        // the `AceSize` check above, so the `SidStart` offset stays inside it.
        let ace_sid = unsafe {
            ace.cast::<u8>()
                .add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
        }
        .cast::<std::ffi::c_void>();
        // SAFETY: `ace_sid` points inside the ACE buffer and `sid` is the
        // caller's valid SID; `EqualSid` only reads both.
        if unsafe { EqualSid(ace_sid, sid) } != 0 {
            found = true;
            break;
        }
    }
    // SAFETY: `descriptor` came from `GetSecurityInfo`.
    unsafe { LocalFree(descriptor as HLOCAL) };
    Ok(found)
}

/// Probe: can this host write and read back a deny-read ACE on a scratch
/// directory? The SID is a fixed, non-existent local SID, so the ACE is inert
/// for every real principal.
pub(crate) fn probe_deny_read() -> Capability {
    let root = std::env::temp_dir().join(format!("duthub-sbx-probe-{}", uuid::Uuid::new_v4()));
    if std::fs::create_dir_all(&root).is_err() {
        return Capability::Unverified;
    }
    let sid = match OwnedSid::from_string("S-1-5-21-1-2-3-4") {
        Ok(sid) => sid,
        Err(_) => return Capability::Unverified,
    };
    let applied =
        deny_read(&root, sid.as_ptr()).and_then(|_| has_deny_read_ace(&root, sid.as_ptr()));
    let _ = std::fs::remove_dir_all(&root);
    match applied {
        Ok(true) => Capability::Verified,
        _ => Capability::Unverified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("duthub-acl-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn grant_round_trips_and_is_idempotent() {
        let dir = scratch("grant");
        let sid = OwnedSid::from_string("S-1-5-21-11-22-33-1500").expect("sid");
        assert!(
            grant(
                &dir,
                sid.as_ptr(),
                FILE_READ_EXEC,
                INHERIT_CONTAINER_AND_OBJECT
            )
            .unwrap()
        );
        assert!(
            !grant(
                &dir,
                sid.as_ptr(),
                FILE_READ_EXEC,
                INHERIT_CONTAINER_AND_OBJECT
            )
            .unwrap()
        );
        assert!(has_allow_ace(&dir, sid.as_ptr(), FILE_READ_EXEC).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deny_read_ace_propagates_to_existing_children() {
        let dir = scratch("deny");
        let file = dir.join("secret.txt");
        std::fs::write(&file, "secret").expect("write secret");
        let sid = OwnedSid::from_string("S-1-5-21-11-22-33-1600").expect("sid");
        assert!(deny_read(&dir, sid.as_ptr()).unwrap());
        assert!(has_deny_read_ace(&dir, sid.as_ptr()).unwrap());
        assert!(
            has_deny_read_ace(&file, sid.as_ptr()).unwrap(),
            "existing children must inherit the deny ACE (T4 depends on it)"
        );
        revoke_pinned(&PinnedPath::open(&dir).unwrap(), sid.as_ptr()).unwrap();
        assert!(
            !has_deny_read_ace(&dir, sid.as_ptr()).unwrap(),
            "rollback must remove the deny ACE"
        );
        assert!(
            !has_allow_ace(&dir, sid.as_ptr(), FILE_READ_EXEC).unwrap(),
            "rollback must never rewrite unrelated ACEs into grants for the sandbox SID"
        );
        // Host owner must still be able to read and create files after rollback.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "secret");
        std::fs::write(dir.join("after-rollback.txt"), "ok").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn protected_dacl_drops_public_grants() {
        let dir = scratch("protect");
        let pinned = PinnedPath::open(&dir).expect("pin");
        let user = OwnedSid::well_known(windows_sys::Win32::Security::WinSelfSid).expect("self");
        let admin = OwnedSid::well_known(windows_sys::Win32::Security::WinBuiltinAdministratorsSid)
            .expect("admins");
        set_protected_dacl(
            &pinned,
            &[(user.as_ptr(), FILE_ALL), (admin.as_ptr(), FILE_ALL)],
            INHERIT_CONTAINER_AND_OBJECT,
        )
        .expect("protect");
        let everyone =
            OwnedSid::well_known(windows_sys::Win32::Security::WinWorldSid).expect("world");
        let users =
            OwnedSid::well_known(windows_sys::Win32::Security::WinBuiltinUsersSid).expect("users");
        assert!(
            !has_any_allow_ace(&pinned, &[&everyone, &users], write_rights()).unwrap(),
            "保护目录不得允许 Everyone/Users 写入"
        );
        assert!(has_any_allow_ace(&pinned, &[&user], FILE_ALL).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
