//! DPAPI wrapper for the sandbox account credential.
//!
//! Derived from openai/codex `codex-rs/windows-sandbox-rs` (Apache-2.0),
//! `dpapi.rs`: the same machine-scope protection so an elevated provisioner and
//! a non-elevated duthub process can both decrypt the stored password.
//!
//! Every failure is returned to the caller; there is no fallback to an
//! unencrypted credential and no "the blob may be plaintext" path.

use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    CryptUnprotectData,
};

use crate::policy::SandboxError;

/// `CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN`.
///
/// Machine scope lets the elevated provisioner write the blob and the
/// unprivileged duthub process read it back; `UI_FORBIDDEN` guarantees the call
/// never blocks on a desktop prompt.
const FLAGS: u32 = CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN;

fn blob_of(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    }
}

/// Copy an output blob allocated by crypt32 (LocalAlloc) and free it.
fn take_output(output: CRYPT_INTEGER_BLOB) -> Result<Vec<u8>, SandboxError> {
    if output.pbData.is_null() || output.cbData == 0 {
        return Err(SandboxError::Setup(
            "DPAPI 返回空数据（凭据不可用）".to_owned(),
        ));
    }
    // SAFETY: crypt32 filled `pbData` with `cbData` readable bytes it allocated;
    // the slice is copied before the allocation is released below.
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    // SAFETY: the buffer was allocated by crypt32 with LocalAlloc, which is what
    // LocalFree expects; it is freed exactly once and no longer used afterwards.
    unsafe { LocalFree(output.pbData as HLOCAL) };
    Ok(bytes)
}

pub(crate) fn protect(data: &[u8]) -> Result<Vec<u8>, SandboxError> {
    let input = blob_of(data);
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: `input` points at `data`, which outlives the call; the optional
    // entropy/reserved/prompt pointers are null, which the API documents as
    // "none"; `output` receives a LocalAlloc'd buffer handled by `take_output`.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            FLAGS,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Setup(format!(
            "CryptProtectData 失败: {}",
            super::error_text(super::last_error())
        )));
    }
    take_output(output)
}

pub(crate) fn unprotect(data: &[u8]) -> Result<Vec<u8>, SandboxError> {
    let input = blob_of(data);
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: as in `protect`; `output` receives a LocalAlloc'd buffer that
    // `take_output` copies and frees.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            FLAGS,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Setup(format!(
            "CryptUnprotectData 失败（凭据损坏或不属于本机）: {}",
            super::error_text(super::last_error())
        )));
    }
    take_output(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_damaged_blobs() {
        let plain = b"duthub-sandbox-password-Dth!0123456789abcdef".to_vec();
        let sealed = protect(&plain).expect("protect");
        assert_ne!(sealed, plain, "the blob must not be the plaintext");
        assert_eq!(unprotect(&sealed).expect("unprotect"), plain);
        assert!(
            unprotect(b"not-a-dpapi-blob").is_err(),
            "a damaged blob must fail closed"
        );
    }
}
