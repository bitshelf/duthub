//! Shared low-level utilities used across `duthub-*` crates:
//! - Lowercase hex and SHA-256 digests
//! - Unix timestamps and proleptic Gregorian calendar conversion
//! - Corrupt configuration file backup naming

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Acquire a [`Mutex`] guard, recovering the inner value if a previous holder panicked.
pub fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Acquire a [`RwLock`] read guard, recovering the inner value if a writer panicked.
pub fn read_unpoisoned<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Acquire a [`RwLock`] write guard, recovering the inner value if a previous writer panicked.
pub fn write_unpoisoned<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Encode a byte slice as lowercase hexadecimal without separators.
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        output.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Compute the lowercase SHA-256 hex digest of an in-memory byte slice.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

/// Stream a file in 64 KiB chunks and return its lowercase SHA-256 hex digest.
///
/// # Errors
///
/// Returns an `io::Error` if the file cannot be opened or read.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

/// Current Unix timestamp in milliseconds (saturates to `0` before the epoch).
#[must_use]
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Current Unix timestamp in whole seconds (saturates to `0` before the epoch).
#[must_use]
pub fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Convert days since `1970-01-01` (UTC) into `(year, month, day)` using Howard
/// Hinnant's civil-from-days algorithm.
#[must_use]
pub fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097).cast_unsigned();
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe.cast_signed() + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = i32::try_from(y + i64::from(m <= 2)).unwrap_or(i32::MAX);
    let month = u32::try_from(m).unwrap_or(12);
    let day = u32::try_from(d).unwrap_or(31);
    (year, month, day)
}

/// Rename a corrupt JSON file beside itself as `<stem>.corrupt-<unix_secs>.json`
/// and return the backup path.
#[must_use]
pub fn backup_corrupt(path: &Path) -> PathBuf {
    let backup = path.with_extension(format!("corrupt-{}.json", unix_secs()));
    let _ = std::fs::rename(path, &backup);
    backup
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_sha256_match_known_vector() {
        assert_eq!(hex_lower(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn civil_from_days_handles_epoch_and_leap_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_782), (2026, 11, 25));
    }
}
