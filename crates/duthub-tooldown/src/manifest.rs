//! On-disk install record (`manifest.json`) and installed-state checks.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ToolInstallError;
use crate::spec::ToolSpec;

pub(crate) const MANIFEST_FILE: &str = "manifest.json";

/// Install record written as `manifest.json` inside `<install_root>/<tool id>/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallManifest {
    /// Tool id from the spec.
    pub tool: String,
    /// Archive URL the bytes were downloaded from.
    pub url: String,
    /// Measured sha256 (lowercase hex) of the downloaded archive bytes.
    pub sha256: String,
    /// Whether the spec carried a pinned digest.
    pub pinned: bool,
    /// True only when pinned AND the measured digest matched the pin.
    /// `false` = the download was accepted unverified (no pin available);
    /// the measured digest is still recorded above.
    pub checksum_verified: bool,
    /// RFC3339 UTC timestamp of the install.
    pub installed_at: String,
    /// Absolute paths of the installed executables (`EntryKind::Executable` entries),
    /// in spec declaration order.
    pub executables: Vec<String>,
    /// Measured sha256 (lowercase hex) of each installed executable, aligned with
    /// [`InstallManifest::executables`].
    ///
    /// `sha256` above covers the downloaded archive; a launch-time integrity
    /// check needs the bytes that will actually be executed. Empty only for
    /// manifests written before this field existed — such installs cannot be
    /// verified at launch and are reported as needing a reinstall.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executable_sha256: Vec<String>,
}

/// Directory an installed tool lives in: `<install_root>/<spec.id>/`.
pub fn installed_tool_dir(install_root: &Path, spec: &ToolSpec) -> PathBuf {
    install_root.join(spec.id)
}

/// Write `manifest.json` into `dir` (pretty JSON, trailing newline). Returns the path.
pub(crate) fn write(dir: &Path, manifest: &InstallManifest) -> Result<PathBuf, ToolInstallError> {
    let path = dir.join(MANIFEST_FILE);
    let mut text =
        serde_json::to_string_pretty(manifest).map_err(|source| ToolInstallError::Manifest {
            path: path.clone(),
            source,
        })?;
    text.push('\n');
    std::fs::write(&path, text.as_bytes()).map_err(|source| ToolInstallError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Read the manifest of an (allegedly) installed tool.
///
/// `Ok(None)` = no manifest file; `Err(Manifest)` = present but unparsable;
/// `Err(Io)` = present but unreadable for another reason.
pub fn read_manifest(tool_dir: &Path) -> Result<Option<InstallManifest>, ToolInstallError> {
    let path = tool_dir.join(MANIFEST_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text)
                .map(Some)
                .map_err(|source| ToolInstallError::Manifest {
                    path: path.clone(),
                    source,
                })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ToolInstallError::Io { path, source }),
    }
}

/// The manifest if `tool_dir` holds a usable install of `spec`: parses, belongs to this
/// tool id, and every recorded executable still exists on disk.
/// A corrupt or mismatched manifest counts as not-installed — the next install run
/// repairs the directory.
pub(crate) fn validated_manifest(tool_dir: &Path, spec: &ToolSpec) -> Option<InstallManifest> {
    let manifest = read_manifest(tool_dir).ok().flatten()?;
    if manifest.tool != spec.id
        || manifest.executables.is_empty()
        || !manifest
            .executables
            .iter()
            .all(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
    {
        return None;
    }
    Some(manifest)
}

/// Whether `tool_dir` holds a usable install of `spec` (see `validated_manifest`).
pub fn is_installed(tool_dir: &Path, spec: &ToolSpec) -> bool {
    validated_manifest(tool_dir, spec).is_some()
}

/// First executable of a usable install of `spec`, or `None` when not installed.
/// Intended for the desktop's "downloaded tool first" resolution (user-configured
/// absolute path > downloaded tool > PATH stays a desktop-layer decision).
pub fn installed_executable(install_root: &Path, spec: &ToolSpec) -> Option<PathBuf> {
    let dir = installed_tool_dir(install_root, spec);
    validated_manifest(&dir, spec)?
        .executables
        .first()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InstallManifest {
        InstallManifest {
            tool: "adb".to_string(),
            url: "https://dl.google.com/android/repository/platform-tools-latest-windows.zip"
                .to_string(),
            sha256: "89d108360a3bf2924d3bea837239edaccc22fd6ff5befd3787c4348a9fe4f09d".to_string(),
            pinned: true,
            checksum_verified: true,
            installed_at: "2026-11-23T12:00:00Z".to_string(),
            executables: vec!["C:\\tools\\adb\\adb.exe".to_string()],
            executable_sha256: vec![
                "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945".to_string(),
            ],
        }
    }

    #[test]
    fn manifests_without_executable_digests_still_parse() {
        // Written before `executableSha256` existed: parsing must succeed and
        // report an empty list, which callers treat as "reinstall required".
        let legacy = r#"{
            "tool": "adb",
            "url": "https://example.invalid/x.zip",
            "sha256": "00",
            "pinned": true,
            "checksumVerified": true,
            "installedAt": "2026-11-01T00:00:00Z",
            "executables": ["C:\\tools\\adb\\adb.exe"]
        }"#;
        let parsed: InstallManifest = serde_json::from_str(legacy).unwrap();
        assert!(parsed.executable_sha256.is_empty());
    }

    #[test]
    fn serde_roundtrip_is_lossless() {
        let manifest = sample();
        let text = serde_json::to_string(&manifest).unwrap();
        let parsed: InstallManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, manifest);
        // camelCase keys are the on-disk contract (installedAt etc.).
        assert!(text.contains("\"installedAt\""));
        assert!(text.contains("\"checksumVerified\""));
    }

    #[test]
    fn unpinned_manifest_roundtrips_with_unverified_marker() {
        let manifest = InstallManifest {
            pinned: false,
            checksum_verified: false,
            ..sample()
        };
        let parsed: InstallManifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();
        assert_eq!(parsed, manifest);
        assert!(!parsed.checksum_verified);
    }

    #[test]
    fn write_then_read_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = sample();
        write(dir.path(), &manifest).unwrap();
        let read_back = read_manifest(dir.path()).unwrap().unwrap();
        assert_eq!(read_back, manifest);
    }

    #[test]
    fn missing_manifest_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_manifest(dir.path()).unwrap(), None);
        assert!(!is_installed(dir.path(), &crate::spec::TOOL_ADB));
    }

    #[test]
    fn corrupt_manifest_counts_as_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MANIFEST_FILE), "{not json").unwrap();
        assert!(matches!(
            read_manifest(dir.path()),
            Err(ToolInstallError::Manifest { .. })
        ));
        assert!(!is_installed(dir.path(), &crate::spec::TOOL_ADB));
    }

    #[test]
    fn manifest_with_missing_executable_counts_as_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = InstallManifest {
            // points at a file that does not exist
            executables: vec![dir.path().join("adb.exe").display().to_string()],
            ..sample()
        };
        write(dir.path(), &manifest).unwrap();
        assert!(!is_installed(dir.path(), &crate::spec::TOOL_ADB));
    }

    #[test]
    fn foreign_tool_manifest_counts_as_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &sample()).unwrap(); // tool = "adb"
        assert!(!is_installed(dir.path(), &crate::spec::TOOL_FFMPEG));
    }
}
