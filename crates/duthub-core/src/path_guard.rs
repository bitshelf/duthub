//! Shared filesystem path safety guards used across `duthub-core`,
//! `duthub-transfer`, `duthub-agent`, and `duthub-desktop`.
//!
//! Centralizes Windows-specific hazards (reserved DOS device names including
//! `COM0`/`LPT0`, superscript digits `¹²³`, trailing spaces before extensions,
//! NTFS Alternate Data Streams, UNC paths, and symlink/junction reparse points).

use std::path::Path;

/// Returns `true` if `component` is a Windows reserved DOS device name,
/// regardless of case, trailing spaces, or extensions.
///
/// Covers `CON`, `PRN`, `AUX`, `NUL`, `COM0`..=`COM9`, `LPT0`..=`LPT9`, and
/// the Windows superscript forms `COM¹`/`COM²`/`COM³`/`LPT¹`/`LPT²`/`LPT³`
/// as well as padded stems like `"CON .txt"`.
#[must_use]
pub fn is_windows_reserved_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ');
    if stem.is_empty() {
        return false;
    }
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    if let Some(rest) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    {
        let mut chars = rest.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            return ch.is_ascii_digit() || matches!(ch, '¹' | '²' | '³');
        }
    }
    false
}

/// Returns `true` if a single path segment contains an NTFS Alternate Data
/// Stream separator (`:`).
#[must_use]
pub fn has_windows_ads(component: &str) -> bool {
    component.contains(':')
}

/// Returns `true` if `raw` looks like a Windows UNC or device namespace path
/// (`\\server\share`, `//server/share`, `\\?\...`, `\\.\...`).
#[must_use]
pub fn is_unc_or_device_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.starts_with(r"\\") || trimmed.starts_with("//")
}

/// Returns `true` if `path` is a symlink or (on Windows) any NTFS reparse point
/// such as a directory junction or mount point.
pub fn is_reparse_point(path: &Path) -> std::io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        Ok(meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_device_names_cover_all_windows_variants() {
        for bad in [
            "CON",
            "con",
            "Con.txt",
            "CON .img",
            "PRN",
            "AUX.log",
            "nul",
            "COM0",
            "COM1",
            "com9.bin",
            "COM¹",
            "COM²",
            "COM³",
            "LPT0",
            "lpt1.txt",
            "LPT³.bin",
        ] {
            assert!(is_windows_reserved_name(bad), "expected reserved: {bad}");
        }
        for good in ["console.log", "COM10", "LPT10", "normal.txt", "aux_file"] {
            assert!(!is_windows_reserved_name(good), "expected allowed: {good}");
        }
    }

    #[test]
    fn detects_ads_and_unc_paths() {
        assert!(has_windows_ads("foo.txt:stream"));
        assert!(!has_windows_ads("foo.txt"));
        assert!(is_unc_or_device_path(r"\\server\share\x.exe"));
        assert!(is_unc_or_device_path("//server/share/x.exe"));
        assert!(is_unc_or_device_path(r"\\?\C:\foo"));
        assert!(!is_unc_or_device_path(r"C:\tools\x.exe"));
    }
}
