//! Handle-pinned private directory ACL hardening and inspection on Windows.
//!
//! Reuses `acl::PinnedPath`, `acl::set_protected_dacl`, `OwnedSid`, and
//! `token::probe_restricted` from the sandbox backend so directory hardening is
//! reparse-safe and shares one Win32 implementation.

use std::ffi::c_void;
use std::os::windows::fs::MetadataExt as _;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, TOKEN_QUERY,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};

use super::acl::{self, PinnedPath};
use super::{OwnedSid, sid_to_string, token};
use crate::os_acl::{AclError, OsIsolationStatus, PrivateDirAcl};

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const EVERYONE_SID: &str = "S-1-1-0";
const BUILTIN_USERS_SID: &str = "S-1-5-32-545";

fn validate_directory(path: &Path) -> Result<(), AclError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AclError::Io(format!("{}: {error}", path.display())))?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(AclError::InvalidPath(path.to_path_buf()));
    }
    Ok(())
}

pub(crate) fn harden(path: &Path) -> Result<(), AclError> {
    validate_directory(path)?;
    let pinned = PinnedPath::open(path).map_err(|e| AclError::Io(e.to_string()))?;
    let current_token =
        token::open_current_token(TOKEN_QUERY).map_err(|e| AclError::Io(e.to_string()))?;
    let user = token::user_sid_of(current_token.0).map_err(|e| AclError::Io(e.to_string()))?;
    let system =
        OwnedSid::well_known(WinLocalSystemSid).map_err(|e| AclError::Io(e.to_string()))?;
    let administrators = OwnedSid::well_known(WinBuiltinAdministratorsSid)
        .map_err(|e| AclError::Io(e.to_string()))?;
    acl::set_protected_dacl(
        &pinned,
        &[
            (user.as_ptr(), acl::FILE_ALL),
            (system.as_ptr(), acl::FILE_ALL),
            (administrators.as_ptr(), acl::FILE_ALL),
        ],
        acl::INHERIT_CONTAINER_AND_OBJECT,
    )
    .map_err(|e| AclError::Rejected(e.to_string()))
}

pub(crate) fn inspect(path: &Path) -> Result<PrivateDirAcl, AclError> {
    validate_directory(path)?;
    let pinned = PinnedPath::open(path).map_err(|e| AclError::Io(e.to_string()))?;
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `pinned.handle()` is a live directory handle with READ_CONTROL;
    // `descriptor` receives a LocalAlloc security descriptor freed by DropGuard.
    let code = unsafe {
        GetSecurityInfo(
            pinned.handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(AclError::Win32 {
            call: "GetSecurityInfo",
            code,
        });
    }
    let _guard = std::mem::DropGuard::new(descriptor, |ptr| {
        // SAFETY: `ptr` was allocated by `GetSecurityInfo` (LocalAlloc).
        unsafe { LocalFree(ptr as HLOCAL) };
    });

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `descriptor` is a valid security descriptor owned by `_guard`.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(AclError::Win32 {
            call: "GetSecurityDescriptorControl",
            code: super::last_error(),
        });
    }
    let protected = control & SE_DACL_PROTECTED != 0;

    let mut present = 0i32;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut defaulted = 0i32;
    // SAFETY: `descriptor` is live; `dacl` points inside `descriptor`.
    if unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        == 0
    {
        return Err(AclError::Win32 {
            call: "GetSecurityDescriptorDacl",
            code: super::last_error(),
        });
    }

    let mut entries = Vec::new();
    if present != 0 && !dacl.is_null() {
        // SAFETY: `dacl` points inside the live descriptor; `AceCount` bounds `GetAce`.
        let count = unsafe { (*dacl).AceCount };
        for index in 0..u32::from(count) {
            let mut ace: *mut c_void = ptr::null_mut();
            // SAFETY: valid DACL and in-range index.
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
                continue;
            }
            let ace_ptr = ace as *const ACCESS_ALLOWED_ACE;
            // SAFETY: every ACE begins with an `ACE_HEADER`.
            if unsafe { (*ace_ptr).Header.AceType } != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            // SAFETY: for `ACCESS_ALLOWED_ACE`, the SID starts at `SidStart`.
            let sid = unsafe { ptr::addr_of!((*ace_ptr).SidStart) as PSID };
            if let Ok(text) = sid_to_string(sid) {
                entries.push(text);
            }
        }
    }

    let private = protected
        && present != 0
        && !dacl.is_null()
        && !entries
            .iter()
            .any(|sid| sid == EVERYONE_SID || sid == BUILTIN_USERS_SID);
    Ok(PrivateDirAcl {
        protected,
        private,
        entries,
    })
}

pub(crate) fn probe_restricted_token() -> Result<(), AclError> {
    if token::probe_restricted().is_verified() {
        Ok(())
    } else {
        Err(AclError::Rejected("受限令牌探测未通过".to_owned()))
    }
}

pub(crate) fn probe(path: &Path) -> OsIsolationStatus {
    let acl = inspect(path);
    let private_dir_acl = acl.as_ref().is_ok_and(|report| report.private);
    let restricted_token = token::probe_restricted().is_verified();
    let app_container = super::container::probe().is_verified();
    let acl_note = match &acl {
        Ok(report) if report.private => "目录 ACL 仅限当前用户/SYSTEM/Administrators".to_owned(),
        Ok(report) => format!(
            "目录 ACL 未限定（protected={}，ACE={:?}）",
            report.protected, report.entries
        ),
        Err(error) => format!("目录 ACL 无法读取：{error}"),
    };
    let token_note = if restricted_token {
        "受限令牌可用"
    } else {
        "受限令牌探测失败"
    };
    let container_note = if app_container {
        "AppContainer 可用"
    } else {
        "AppContainer 不可用"
    };
    OsIsolationStatus {
        private_dir_acl,
        restricted_token,
        app_container,
        unsupported: false,
        detail: format!("{acl_note}；{token_note}；{container_note}"),
    }
}
