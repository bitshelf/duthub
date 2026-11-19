//! Directory opens that never traverse a reparse point.
//!
//! Derived from openai/codex `codex-rs/windows-sandbox-rs` (Apache-2.0),
//! `no_reparse_dir.rs`: the same `NtCreateFile` + `OBJ_DONT_REPARSE` shape, the
//! same drive-letter resolution to `\Device\HarddiskVolumeN`, and the same
//! refusal of relative components, alternate data streams and non-disk devices.
//!
//! Why a pinned handle instead of a path check: every grant and deny ACE this
//! crate writes must land on the object it validated. A path-based
//! `SetNamedSecurityInfoW` after a `symlink_metadata` check leaves a window in
//! which an untrusted party can swap a directory for a junction and redirect the
//! ACE onto an unrelated tree. The handle returned here is kept alive through the
//! security write, and `NtCreateFile` with `OBJ_DONT_REPARSE` refuses a reparse
//! point in any component.

use std::ffi::c_void;
use std::mem::size_of;
use std::mem::size_of_val;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, Prefix};

use windows_sys::Win32::Foundation::{
    HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE,
    RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, QueryDosDeviceW};
use windows_sys::Win32::System::IO::{IO_STATUS_BLOCK, IO_STATUS_BLOCK_0};

use super::OwnedHandle;
use crate::policy::SandboxError;

const FILE_OPEN: u32 = 1;
const FILE_OPEN_IF: u32 = 3;
const FILE_DIRECTORY_FILE: u32 = 1;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const STATUS_REPARSE_POINT_ENCOUNTERED: NTSTATUS = 0xC000_050B_u32 as i32;
/// Longest path the object manager accepts for a local volume path.
const NT_PATH_CAPACITY: usize = 32_768;

/// `OBJECT_ATTRIBUTES`, declared locally: the layout is stable and this crate
/// does not otherwise need `Win32_System_SystemServices`.
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *const UNICODE_STRING,
    attributes: u32,
    security_descriptor: *const c_void,
    security_quality_of_service: *const c_void,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    /// SAFETY: the caller must pass a fully initialised `OBJECT_ATTRIBUTES`
    /// whose strings outlive the call; the returned handle is owned by the caller.
    fn NtCreateFile(
        file_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const ObjectAttributes,
        io_status_block: *mut IO_STATUS_BLOCK,
        allocation_size: *const i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *const c_void,
        ea_length: u32,
    ) -> NTSTATUS;
}

/// Whether a no-reparse open may create the final object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenDisposition {
    OpenExisting,
    OpenOrCreate,
}

/// Absolute local-disk directory path without relative components or streams.
pub(crate) fn validate_local_directory_path(path: &Path) -> Result<(), SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::PolicyRejected(format!(
            "沙箱目录必须是绝对路径: {}",
            path.display()
        )));
    }
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => {}
        _ => {
            return Err(SandboxError::PolicyRejected(format!(
                "沙箱目录必须是本地磁盘路径（不接受 UNC/设备路径）: {}",
                path.display()
            )));
        }
    }
    for component in components {
        match component {
            Component::RootDir => {}
            Component::Normal(name) if !name.to_string_lossy().contains(':') => {}
            Component::Normal(_) => {
                return Err(SandboxError::PolicyRejected(format!(
                    "沙箱目录不能包含备用数据流: {}",
                    path.display()
                )));
            }
            Component::ParentDir | Component::CurDir | Component::Prefix(_) => {
                return Err(SandboxError::PolicyRejected(format!(
                    "沙箱目录不能包含相对路径段: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

/// Open `path` as a directory without following any reparse point.
///
/// Parent directories must already exist; `OpenOrCreate` only creates the final
/// component. The handle is kept for the caller's security write so the object
/// cannot be swapped between validation and mutation.
pub(crate) fn open_directory_no_reparse(
    path: &Path,
    desired_access: u32,
    share_access: u32,
    disposition: OpenDisposition,
) -> Result<OwnedHandle, SandboxError> {
    let mut nt_path = local_nt_path(path)?;
    let create_disposition = match disposition {
        OpenDisposition::OpenExisting => FILE_OPEN,
        OpenDisposition::OpenOrCreate => FILE_OPEN_IF,
    };
    open_no_reparse(
        std::ptr::null_mut(),
        &mut nt_path,
        desired_access,
        share_access,
        create_disposition,
        FILE_DIRECTORY_FILE,
    )
    .map_err(|error| SandboxError::Setup(format!("无法打开沙箱目录 {}: {error}", path.display())))
}

/// Open `path` as a regular file without following any reparse point.
pub(crate) fn open_file_no_reparse(
    path: &Path,
    desired_access: u32,
    share_access: u32,
) -> Result<OwnedHandle, SandboxError> {
    let mut nt_path = local_nt_path(path)?;
    open_no_reparse(
        std::ptr::null_mut(),
        &mut nt_path,
        desired_access,
        share_access,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE,
    )
    .map_err(|error| SandboxError::Setup(format!("无法打开沙箱文件 {}: {error}", path.display())))
}

/// `\Device\HarddiskVolumeN` + the path remainder, for a local-disk path.
///
/// Only the drive-letter alias is resolved; every filesystem component is still
/// checked by `NtCreateFile`, so a junction in the middle of the path is
/// refused rather than canonicalised away.
fn local_nt_path(path: &Path) -> Result<Vec<u16>, SandboxError> {
    validate_local_directory_path(path)?;
    let source: Vec<u16> = path.as_os_str().encode_wide().collect();
    let verbatim = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))
    );
    let source_offset = if verbatim { 4 } else { 0 };
    if source.len() < source_offset + 2 {
        return Err(SandboxError::PolicyRejected(format!(
            "沙箱目录路径不完整: {}",
            path.display()
        )));
    }
    let drive = [source[source_offset], b':' as u16, 0];
    let mut nt_path = vec![0u16; NT_PATH_CAPACITY];
    // SAFETY: `drive` is a NUL-terminated device name and `nt_path` is a
    // writable buffer of `NT_PATH_CAPACITY` elements; the call only writes there.
    let length =
        unsafe { QueryDosDeviceW(drive.as_ptr(), nt_path.as_mut_ptr(), nt_path.len() as u32) };
    if length == 0 {
        return Err(SandboxError::Setup(format!(
            "无法解析盘符 {drive:?}: {}",
            super::error_text(super::last_error())
        )));
    }
    nt_path.truncate(
        nt_path
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(length as usize),
    );
    if !is_disk_volume(&nt_path) {
        return Err(SandboxError::PolicyRejected(format!(
            "沙箱目录所在盘符没有解析到磁盘卷: {}",
            path.display()
        )));
    }
    for unit in source.into_iter().skip(source_offset + 2) {
        nt_path.push(if unit == b'/' as u16 {
            b'\\' as u16
        } else {
            unit
        });
    }
    nt_path.push(0);
    Ok(nt_path)
}

/// `\Device\HarddiskVolume` followed by a non-empty number and nothing else.
fn is_disk_volume(target: &[u16]) -> bool {
    const PREFIX: &[u8] = br"\Device\HarddiskVolume";
    let Some((prefix, number)) = target.split_at_checked(PREFIX.len()) else {
        return false;
    };
    prefix.iter().zip(PREFIX).all(|(&unit, expected)| {
        u8::try_from(unit).is_ok_and(|unit| unit.eq_ignore_ascii_case(expected))
    }) && !number.is_empty()
        && number
            .iter()
            .all(|unit| (u16::from(b'0')..=u16::from(b'9')).contains(unit))
}

/// One `NtCreateFile` call with `OBJ_DONT_REPARSE` and no inherited handle.
pub(crate) fn open_no_reparse(
    root_directory: HANDLE,
    name: &mut [u16],
    desired_access: u32,
    share_access: u32,
    disposition: u32,
    create_options: u32,
) -> Result<OwnedHandle, SandboxError> {
    let name_length = u16::try_from((name.len() - 1) * size_of::<u16>())
        .map_err(|_| SandboxError::PolicyRejected("沙箱路径过长".to_owned()))?;
    let maximum_length = u16::try_from(size_of_val(name))
        .map_err(|_| SandboxError::PolicyRejected("沙箱路径过长".to_owned()))?;
    let object_name = UNICODE_STRING {
        Length: name_length,
        MaximumLength: maximum_length,
        Buffer: name.as_mut_ptr(),
    };
    let object_attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory,
        object_name: &object_name,
        attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        security_descriptor: std::ptr::null(),
        security_quality_of_service: std::ptr::null(),
    };
    let mut io_status_block = IO_STATUS_BLOCK {
        Anonymous: IO_STATUS_BLOCK_0 { Status: 0 },
        Information: 0,
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: `object_attributes` and `object_name` are fully initialised and
    // alive for the call, `name` outlives them, and `handle` receives an owned
    // kernel handle that the caller wraps in `OwnedHandle`.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &object_attributes,
            &mut io_status_block,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share_access,
            disposition,
            create_options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        if status == STATUS_REPARSE_POINT_ENCOUNTERED {
            return Err(SandboxError::Setup(
                "沙箱路径包含重解析点（junction/符号链接），已拒绝".to_owned(),
            ));
        }
        // SAFETY: translating an NTSTATUS to a Win32 error code is a pure call.
        let error = unsafe { RtlNtStatusToDosError(status) };
        return Err(SandboxError::Setup(format!(
            "NtCreateFile 失败: {}",
            super::error_text(error)
        )));
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(SandboxError::Setup(
            "NtCreateFile 返回了无效句柄".to_owned(),
        ));
    }
    Ok(OwnedHandle(handle))
}

/// Share mode used by every security write: other readers/writers may keep the
/// directory open, but nobody may delete or rename it while the handle is held.
pub(crate) const SHARE_READ_WRITE: u32 = windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_unc_paths_are_refused() {
        assert!(
            validate_local_directory_path(Path::new("relative\\dir")).is_err(),
            "相对路径必须被拒绝"
        );
        assert!(
            validate_local_directory_path(Path::new(r"\\server\share\dir")).is_err(),
            "UNC 必须被拒绝"
        );
        assert!(
            validate_local_directory_path(Path::new(r"C:\dir\..\other")).is_err(),
            "相对路径段必须被拒绝"
        );
        assert!(
            validate_local_directory_path(Path::new(r"C:\dir\stream:name")).is_err(),
            "备用数据流必须被拒绝"
        );
        assert!(validate_local_directory_path(Path::new(r"C:\")).is_ok());
        assert!(validate_local_directory_path(Path::new(r"C:\a\b")).is_ok());
    }

    #[test]
    fn local_paths_resolve_to_a_disk_volume() {
        let nt = local_nt_path(Path::new(r"C:\Windows")).expect("resolve drive");
        let text: Vec<u8> = nt
            .iter()
            .take_while(|unit| **unit != 0)
            .filter_map(|unit| u8::try_from(*unit).ok())
            .collect();
        let text = String::from_utf8_lossy(&text).to_ascii_lowercase();
        assert!(
            text.starts_with(r"\device\harddiskvolume"),
            "unexpected NT path {text}"
        );
        assert!(text.ends_with(r"\windows"), "unexpected NT path {text}");
    }

    #[test]
    fn opening_a_real_directory_yields_a_usable_handle() {
        let dir = std::env::temp_dir().join(format!("duthub-noreparse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let handle = open_directory_no_reparse(
            &dir,
            windows_sys::Win32::Storage::FileSystem::READ_CONTROL,
            SHARE_READ_WRITE,
            OpenDisposition::OpenExisting,
        )
        .expect("open directory");
        assert!(!handle.0.is_null());
        drop(handle);
        assert!(
            open_directory_no_reparse(
                &dir.join("missing"),
                windows_sys::Win32::Storage::FileSystem::READ_CONTROL,
                SHARE_READ_WRITE,
                OpenDisposition::OpenExisting,
            )
            .is_err(),
            "打开不存在的目录必须失败"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
