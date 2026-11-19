//! Declarative tool table: what to download, which files to extract, which digest is pinned.
//!
//! Adding a new tool ("等工具") = one `pub const ToolSpec` here plus a `builtin_specs()` row.
//! Pins are md-verified probe artifacts: each pin comment records where and when the digest
//! was obtained. Pinning an alias URL (both built-ins are "latest"-style aliases) means an
//! upstream content change fails the install loudly with [`crate::ToolInstallError::
//! ChecksumMismatch`] instead of silently installing different bytes; refresh the pin after
//! re-probing (see the probe notes in the spec comments).

/// Kind of a declared archive entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Executable reported in `InstallManifest::executables` (absolute paths).
    Executable,
    /// Required support file (DLL, data) extracted next to the executables but not
    /// reported as an executable.
    Support,
}

/// One file to extract from a tool archive.
///
/// Matching is by **basename** (case-insensitive, `/` and `\` both treated as separators)
/// anywhere in the archive — version-stamped archives like gyan.dev's
/// `ffmpeg-9.1-essentials_build/bin/ffmpeg.exe` change their top-level directory on every
/// release, so full paths would be brittle. The first match in archive order wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractEntry {
    pub file_name: &'static str,
    pub kind: EntryKind,
}

/// Executable entry (declared in manifests).
pub const fn exe(file_name: &'static str) -> ExtractEntry {
    ExtractEntry {
        file_name,
        kind: EntryKind::Executable,
    }
}

/// Support-file entry (DLL etc., extracted but not listed as an executable).
pub const fn support(file_name: &'static str) -> ExtractEntry {
    ExtractEntry {
        file_name,
        kind: EntryKind::Support,
    }
}

/// Declarative description of one downloadable tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    /// Stable identifier; the tool installs into `<install_root>/<id>/`.
    pub id: &'static str,
    /// Archive URL (https).
    pub url: &'static str,
    /// sha256 (hex) of the archive bytes when known. `None` = the download is accepted
    /// UNVERIFIED: the measured digest is recorded in the manifest with
    /// `checksumVerified: false`. A present pin is enforced (mismatch = hard error).
    pub pinned_sha256: Option<&'static str>,
    /// Files to extract; every declared entry must exist in the archive or the install
    /// fails with `MissingArchiveEntry`.
    pub entries: &'static [ExtractEntry],
}

impl ToolSpec {
    /// Basenames of the `Executable` entries, in declaration order.
    pub fn executable_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::Executable)
            .map(|e| e.file_name)
    }
}

/// Android platform-tools (adb + fastboot) from Google's official "latest" alias.
///
/// Pinned on 2026-11-23: the archive was downloaded from the URL below (8,188,601 bytes)
/// and hashed with sha256sum (`89d108360a3bf2924d3bea837239edaccc22fd6ff5befd3787c4348a9fe4f09d`).
/// The URL is Google's rolling alias, so the pin must be refreshed whenever Google ships a
/// new platform-tools build — a stale pin fails installs loudly instead of installing
/// different bytes. This also blocks the update flow: `install_tool(force = true)` still
/// verifies the pin, so after an upstream refresh BOTH first install and update hard-fail
/// with `ChecksumMismatch` until the pin is re-probed and replaced here.
pub const TOOL_ADB: ToolSpec = ToolSpec {
    id: "adb",
    url: "https://dl.google.com/android/repository/platform-tools-latest-windows.zip",
    pinned_sha256: Some("89d108360a3bf2924d3bea837239edaccc22fd6ff5befd3787c4348a9fe4f09d"),
    entries: &[
        exe("adb.exe"),
        exe("fastboot.exe"),
        // adb.exe/fastboot.exe refuse to start without these two DLLs next to them.
        support("AdbWinApi.dll"),
        support("AdbWinUsbApi.dll"),
    ],
};

/// ffmpeg (with ffprobe and ffplay) from gyan.dev's release-essentials alias.
///
/// Pinned on 2026-11-23 from the gyan.dev sidecar
/// `ffmpeg-release-essentials.zip.sha256`. The alias currently resolves to
/// `packages/ffmpeg-9.1-essentials_build.zip`; refresh the pin whenever gyan.dev re-points
/// the release alias.
///
/// ffplay.exe is extracted so the media page's preview sibling lookup
/// (`<ffmpeg dir>/ffplay.exe`) resolves to the downloaded copy instead of
/// falling back to PATH. It stays the LAST entry: `installed_executable`
/// reports the first declared executable, which must remain ffmpeg.exe.
pub const TOOL_FFMPEG: ToolSpec = ToolSpec {
    id: "ffmpeg",
    url: "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
    pinned_sha256: Some("b9bbbb3acece000cbeec994f856649354e52c773807841e78908374b24d658fd"),
    entries: &[exe("ffmpeg.exe"), exe("ffprobe.exe"), exe("ffplay.exe")],
};

/// Goose CLI from the project's rolling "latest" release asset.
///
/// Deliberately NOT pinned: this is a rolling asset whose bytes change with every upstream
/// release, so a pin would block every future update with `ChecksumMismatch`. Updates ride
/// the same alias URL (`install_tool(force = true)`), and each run records the measured
/// sha256 in the manifest with `checksumVerified = false` — unverified by design, stated
/// honestly rather than pretending integrity that is not checked.
///
/// Probed 2026-11-23: `https://github.com/block/goose/...` 301-redirects to the moved repo
/// `aaif-goose/goose` (GitHub org rename), whose latest release at probe time was v1.60.2.
/// The Windows x86_64 asset is `goose-x86_64-pc-windows-msvc.zip` (200, 54,449,330 bytes);
/// the `-gnu` variant returned 404. The canonical moved-repo URL is used directly to save
/// a redirect hop; GitHub org redirects keep old URLs working if it moves again.
/// Audit note (NOT a pin): the v1.60.2 asset measured sha256
/// `5b838ab498f7a6b121fab621b99d39a480458c373f16be7f7ce7869e9bc54888` and contains a single
/// member `goose-package/goose.exe`; later releases will measure differently by design.
pub const TOOL_GOOSE: ToolSpec = ToolSpec {
    id: "goose",
    url: "https://github.com/aaif-goose/goose/releases/latest/download/goose-x86_64-pc-windows-msvc.zip",
    pinned_sha256: None,
    entries: &[exe("goose.exe")],
};

/// All built-in tool specs, in table order. New tools append here.
pub fn builtin_specs() -> &'static [&'static ToolSpec] {
    &[&TOOL_ADB, &TOOL_FFMPEG, &TOOL_GOOSE]
}

/// Look up a built-in spec by id.
pub fn spec_by_id(id: &str) -> Option<&'static ToolSpec> {
    builtin_specs().iter().copied().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_is_sane() {
        let specs = builtin_specs();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].id, "adb");
        assert_eq!(specs[1].id, "ffmpeg");
        assert_eq!(specs[2].id, "goose");
        assert_eq!(spec_by_id("adb"), Some(&TOOL_ADB));
        assert_eq!(spec_by_id("goose"), Some(&TOOL_GOOSE));
        assert_eq!(spec_by_id("nope"), None);
        for spec in specs {
            assert!(
                spec.url.starts_with("https://"),
                "insecure url for {}",
                spec.id
            );
            // Pins are either absent (rolling assets) or well-formed sha256 hex.
            if let Some(pin) = spec.pinned_sha256 {
                assert_eq!(pin.len(), 64, "pin length for {}", spec.id);
                assert!(pin.bytes().all(|b| b.is_ascii_hexdigit()));
            }
        }
        // Goose is the rolling, unpinned tool by design.
        assert_eq!(TOOL_GOOSE.pinned_sha256, None);
        assert_eq!(
            TOOL_GOOSE.executable_names().collect::<Vec<_>>(),
            ["goose.exe"]
        );
    }

    #[test]
    fn adb_extracts_executables_and_dlls() {
        assert_eq!(
            TOOL_ADB.executable_names().collect::<Vec<_>>(),
            ["adb.exe", "fastboot.exe"]
        );
        assert_eq!(TOOL_ADB.entries.len(), 4);
        assert_eq!(
            TOOL_FFMPEG.executable_names().collect::<Vec<_>>(),
            ["ffmpeg.exe", "ffprobe.exe", "ffplay.exe"]
        );
    }
}
