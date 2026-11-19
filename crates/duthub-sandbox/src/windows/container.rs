//! Per-cell AppContainer profiles.
//!
//! A profile carries the container SID that owns the cell. It is created with
//! **zero capabilities**: no network (loopback included), no device access and
//! no read outside paths explicitly granted to the SID. The profile is reused
//! across launches (derived by name) so repeated starts do not stack identities.

use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::PSID;

use super::{OwnedSid, to_wide};
use crate::policy::{SandboxError, profile_name};
use crate::status::Capability;

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`.
const ALREADY_EXISTS: u32 = 0x8007_00B7;
/// `E_UNEXPECTED`: the losing side of a concurrent `CreateAppContainerProfile`
/// race sees this instead of `ERROR_ALREADY_EXISTS` (observed 2026-11-25 when
/// the security tests run in parallel).
const E_UNEXPECTED: u32 = 0x8000_FFFF;
/// Retries before a profile creation failure is treated as real.
const CREATE_ATTEMPTS: u32 = 10;

/// Serializes profile creation inside this process. Parallel test threads (and
/// concurrent DUT preparations) otherwise race on the same profile name; the
/// kernel call is inexpensive and only runs on launch.
static PROFILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Create (or adopt) the profile for `scope` and return its SID.
///
/// The profile is created first on purpose: `DeriveAppContainerSidFromAppContainerName`
/// happily derives a SID for a name that has **no profile**, and a process
/// started with such a SID fails in `CreateProcess` with `ERROR_FILE_NOT_FOUND`
/// (verified 2026-11-25). `CreateAppContainerProfile` returns
/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)` for an existing profile, which is
/// then adopted via `Derive...`.
pub(crate) fn ensure_profile(scope: &str) -> Result<OwnedSid, SandboxError> {
    let name = profile_name(scope);
    let wide = to_wide(&name);
    let _guard = PROFILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut last = 0u32;
    for _ in 0..CREATE_ATTEMPTS {
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated profile name; display/description
        // use the same string; no capabilities are passed (`null`/0), which is
        // the zero-capability container this backend requires.
        let hr = unsafe {
            CreateAppContainerProfile(
                wide.as_ptr(),
                wide.as_ptr(),
                wide.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if hr >= 0 && !sid.is_null() {
            return Ok(OwnedSid::from_container_api(sid));
        }
        last = hr as u32;
        if last == ALREADY_EXISTS
            && let Some(sid) = derive(&wide)
        {
            return Ok(sid);
        }
        if last != E_UNEXPECTED {
            break;
        }
        // Another creator may still be finishing; the retry sees
        // `ALREADY_EXISTS` once its profile exists.
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(SandboxError::Setup(format!(
        "CreateAppContainerProfile 失败（{name}）: HRESULT {last:#010x}"
    )))
}

fn derive(wide: &[u16]) -> Option<OwnedSid> {
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and alive for the call.
    let hr = unsafe { DeriveAppContainerSidFromAppContainerName(wide.as_ptr(), &mut sid) };
    (hr >= 0 && !sid.is_null()).then(|| OwnedSid::from_container_api(sid))
}

/// Probe: create, derive, delete and re-create a throwaway profile. Never
/// starts a process.
///
/// The previous version asserted that `DeriveAppContainerSidFromAppContainerName`
/// fails after the delete — but that call is a pure hash of the profile name and
/// never consults the profile store, so the probe reported `Unverified` on a
/// machine where AppContainer demonstrably works (measured 2026-11-25). The
/// lifecycle is now proved by the operations themselves: the derived SID must
/// equal the created one, the delete must succeed, and a second create of the
/// same name must succeed again.
pub(crate) fn probe() -> Capability {
    // A unique name per call: `status()` is called from several threads and from
    // parallel tests, and a shared name would make create/delete race.
    let name = format!(
        "duthub.sandbox.probe.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let wide = to_wide(&name);
    let created = create_profile(&wide);
    let Some(created) = created else {
        return Capability::Unverified;
    };
    let derived_matches = match derive(&wide) {
        Some(derived) => {
            let same = derived.as_string() == created.as_string();
            drop(derived);
            same
        }
        None => false,
    };
    let deleted = {
        // SAFETY: `wide` is NUL-terminated and alive for the call.
        (unsafe { DeleteAppContainerProfile(wide.as_ptr()) }) >= 0
    };
    let recreated = create_profile(&wide).is_some();
    if recreated {
        // SAFETY: `wide` is NUL-terminated and alive for the call.
        unsafe { DeleteAppContainerProfile(wide.as_ptr()) };
    }
    if derived_matches && deleted && recreated {
        Capability::Verified
    } else {
        Capability::Unverified
    }
}

/// Create the profile for `wide` and return its SID (adopting an existing one).
fn create_profile(wide: &[u16]) -> Option<OwnedSid> {
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and alive for the call; no capabilities
    // are requested.
    let hr = unsafe {
        CreateAppContainerProfile(
            wide.as_ptr(),
            wide.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            0,
            &mut sid,
        )
    };
    if hr >= 0 && !sid.is_null() {
        return Some(OwnedSid::from_container_api(sid));
    }
    if !sid.is_null() {
        // SAFETY: `sid` came from the AppContainer API, whose contract is FreeSid.
        unsafe { windows_sys::Win32::Security::FreeSid(sid) };
    }
    if hr as u32 == ALREADY_EXISTS {
        derive(wide)
    } else {
        None
    }
}
