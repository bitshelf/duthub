//! Transfer configuration: serde shape, path resolution and base-path
//! normalization.
//!
//! Stage one keeps [`TransferConfig`] outside `SettingsFile`; the serde shape
//! follows the workspace convention (AGENTS.md pits #19/#24): every field with
//! a default is `#[serde(default = ..., skip_serializing_if = ...)]` so the
//! default segment serializes to `{}` and `docs/examples/settings.json`
//! roundtrips stay untouched once the section is mounted in stage two.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

const DEFAULT_BASE_PATH: &str = "/transfer";
const DEFAULT_ALLOWED_EXTENSIONS: [&str; 2] = ["img", "iso"];

fn default_base_path() -> String {
    DEFAULT_BASE_PATH.to_string()
}

fn base_path_is_default(value: &str) -> bool {
    value == DEFAULT_BASE_PATH
}

/// 30 GiB — matches the planned image size envelope (REVIEW:83).
const fn default_max_upload_bytes() -> u64 {
    32_212_254_720
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn max_upload_bytes_is_default(value: &u64) -> bool {
    *value == default_max_upload_bytes()
}

const fn default_upload_expires_secs() -> u64 {
    86_400
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn upload_expires_secs_is_default(value: &u64) -> bool {
    *value == default_upload_expires_secs()
}

const fn default_lock_timeout_secs() -> u64 {
    10
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn lock_timeout_secs_is_default(value: &u64) -> bool {
    *value == default_lock_timeout_secs()
}

const fn default_reserved_disk_bytes() -> u64 {
    1_073_741_824
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn reserved_disk_bytes_is_default(value: &u64) -> bool {
    *value == default_reserved_disk_bytes()
}

const fn default_gc_interval_secs() -> u64 {
    300
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn gc_interval_secs_is_default(value: &u64) -> bool {
    *value == default_gc_interval_secs()
}

fn default_allowed_extensions() -> Vec<String> {
    DEFAULT_ALLOWED_EXTENSIONS
        .iter()
        .map(|ext| (*ext).to_string())
        .collect()
}

fn allowed_extensions_is_default(value: &[String]) -> bool {
    value.len() == DEFAULT_ALLOWED_EXTENSIONS.len()
        && value
            .iter()
            .zip(DEFAULT_ALLOWED_EXTENSIONS)
            .all(|(actual, expected)| actual == expected)
}

const fn default_verify_concurrency() -> u64 {
    2
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn verify_concurrency_is_default(value: &u64) -> bool {
    *value == default_verify_concurrency()
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires &T"
)]
fn allow_zero_length_uploads_is_default(value: &bool) -> bool {
    !*value
}

/// Receiver-side configuration for the tus 1.0.0 image transfer surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferConfig {
    /// URL prefix the tus surface is mounted at. Accepts `transfer`,
    /// `/transfer` or `/transfer/`; see [`normalize_base_path`].
    #[serde(
        default = "default_base_path",
        skip_serializing_if = "base_path_is_default"
    )]
    pub base_path: String,
    /// Staging + publish root. `None` resolves to `<exe_dir>/duthub-data/images`
    /// (AGENTS.md: runtime data lives under `duthub-data/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_dir: Option<String>,
    /// `Upload-Length` ceiling; surfaced as `Tus-Max-Size` and fed to fileloft.
    #[serde(
        default = "default_max_upload_bytes",
        skip_serializing_if = "max_upload_bytes_is_default"
    )]
    pub max_upload_bytes: u64,
    /// Single source of truth for the unfinished-upload TTL. It feeds the
    /// tus `expiration` extension (410 Gone, `Upload-Expires`) and the sweep
    /// that removes what the TTL claimed (404 afterwards), so the two never
    /// drift apart.
    #[serde(
        default = "default_upload_expires_secs",
        skip_serializing_if = "upload_expires_secs_is_default"
    )]
    pub upload_expires_secs: u64,
    /// How long a request waits on the per-upload lock before the tus layer
    /// gives up (503, the upstream mapping).
    #[serde(
        default = "default_lock_timeout_secs",
        skip_serializing_if = "lock_timeout_secs_is_default"
    )]
    pub lock_timeout_secs: u64,
    /// Safety margin subtracted from free disk space when granting a POST.
    #[serde(
        default = "default_reserved_disk_bytes",
        skip_serializing_if = "reserved_disk_bytes_is_default"
    )]
    pub reserved_disk_bytes: u64,
    /// Period of the background GC sweep started by `serve`; GC sweeps can
    /// always be triggered manually through `TransferServer::gc_now`.
    #[serde(
        default = "default_gc_interval_secs",
        skip_serializing_if = "gc_interval_secs_is_default"
    )]
    pub gc_interval_secs: u64,
    /// Allowed publish-name extensions (case-insensitive).
    #[serde(
        default = "default_allowed_extensions",
        skip_serializing_if = "allowed_extensions_is_default"
    )]
    pub allowed_extensions: Vec<String>,
    /// How many completed uploads may be hashed at the same time. A single
    /// global mutex would serialize 30 GiB verifications and stall every other
    /// completion, so verification is bounded rather than exclusive.
    #[serde(
        default = "default_verify_concurrency",
        skip_serializing_if = "verify_concurrency_is_default"
    )]
    pub verify_concurrency: u64,
    /// Zero-length uploads publish an empty artifact immediately and never
    /// expire; they are refused unless a host explicitly wants them.
    #[serde(default, skip_serializing_if = "allow_zero_length_uploads_is_default")]
    pub allow_zero_length_uploads: bool,
    /// Per-DUT storage ceiling for this receiver, measured over the whole
    /// transfer root. `None` keeps only the global free-space budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dut_storage_bytes: Option<u64>,
    /// Bind `0.0.0.0` instead of loopback. Requires an authorizer — the bind
    /// policy fails closed in `TransferServer::serve` otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lan_access: bool,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            base_path: default_base_path(),
            transfer_dir: None,
            max_upload_bytes: default_max_upload_bytes(),
            upload_expires_secs: default_upload_expires_secs(),
            lock_timeout_secs: default_lock_timeout_secs(),
            reserved_disk_bytes: default_reserved_disk_bytes(),
            gc_interval_secs: default_gc_interval_secs(),
            allowed_extensions: default_allowed_extensions(),
            verify_concurrency: default_verify_concurrency(),
            allow_zero_length_uploads: false,
            dut_storage_bytes: None,
            lan_access: false,
        }
    }
}

/// Default-value predicate for the whole section (workspace pit #19 shape).
#[must_use]
pub fn transfer_is_default(config: &TransferConfig) -> bool {
    config == &TransferConfig::default()
}

impl TransferConfig {
    /// Lock wait as a [`Duration`] for the tus handlers and the sweep.
    #[must_use]
    pub fn lock_timeout(&self) -> Duration {
        Duration::from_secs(self.lock_timeout_secs.max(1))
    }

    /// TTL of an unfinished upload, as fed to the tus `expiration` extension.
    #[must_use]
    pub fn upload_ttl(&self) -> Duration {
        Duration::from_secs(self.upload_expires_secs.max(1))
    }
}

/// Normalize the configured base path to a leading slash and no trailing slash.
///
/// Accepts `transfer`, `/transfer` and `/transfer/` (plus any multi-segment
/// equivalent): all three spellings mount the surface at the same place.
pub fn normalize_base_path(raw: &str) -> Result<String, TransferConfigError> {
    let trimmed = raw.trim();
    let body = trimmed.trim_matches('/');
    if body.is_empty() {
        return Err(TransferConfigError::BasePath(
            "base path must name at least one segment".to_string(),
        ));
    }
    if body.contains('\\') || body.contains("//") || body.split('/').any(|seg| seg == "..") {
        return Err(TransferConfigError::BasePath(format!(
            "invalid base path: {raw:?}"
        )));
    }
    Ok(format!("/{body}"))
}

/// Configuration resolution failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TransferConfigError {
    /// `base_path` could not be normalized.
    #[error("invalid base path: {0}")]
    BasePath(String),
    /// `transferDir` was rejected (escape, drive-relative, UNC) or is not writable.
    #[error("invalid transfer dir: {0}")]
    TransferDir(String),
}

/// `<work_root>/duthub-data/images` — the default staging + publish root.
#[must_use]
pub fn default_transfer_dir_in(work_root: &Path) -> PathBuf {
    work_root.join("duthub-data").join("images")
}

/// Resolve the transfer directory **inside** `work_root`.
///
/// - `None` / empty / `"images"` → `<work_root>/duthub-data/images`;
/// - other relative paths anchor at `work_root`;
/// - absolute paths are accepted only when they resolve inside `work_root`
///   (normalized lexically first, so `..` cannot escape);
/// - drive-relative (`D:images`), rooted-without-drive (`\images`) and UNC
///   (`\\srv\share\...`) forms are rejected fail-closed.
pub fn resolve_transfer_dir(
    configured: Option<&str>,
    work_root: &Path,
) -> Result<PathBuf, TransferConfigError> {
    let root = normalize_lexical(work_root).ok_or_else(|| {
        TransferConfigError::TransferDir(format!("invalid work root: {}", work_root.display()))
    })?;
    let raw = configured.map(str::trim).filter(|value| !value.is_empty());
    let candidate = match raw {
        None | Some("images") => default_transfer_dir_in(&root),
        Some(raw) => {
            if raw.starts_with("\\\\") || raw.starts_with("//") {
                return Err(TransferConfigError::TransferDir(
                    "UNC paths are not supported (fail-closed)".to_string(),
                ));
            }
            let after_drive = if has_windows_drive(raw) {
                &raw[2..]
            } else {
                raw
            };
            if duthub_core::path_guard::has_windows_ads(after_drive) {
                return Err(TransferConfigError::TransferDir(
                    "NTFS Alternate Data Streams (`:`) are not supported".to_string(),
                ));
            }
            let path = Path::new(raw);
            if has_windows_drive(raw) && !path.is_absolute() {
                return Err(TransferConfigError::TransferDir(
                    "drive-relative paths (e.g. D:images) are not supported".to_string(),
                ));
            }
            if path.is_absolute() {
                path.to_path_buf()
            } else if path.has_root() {
                return Err(TransferConfigError::TransferDir(
                    "rooted paths without a drive (e.g. \\images) are not supported".to_string(),
                ));
            } else {
                root.join(path)
            }
        }
    };
    let candidate = normalize_lexical(&candidate).ok_or_else(|| {
        TransferConfigError::TransferDir(format!(
            "{} would escape its parent directory",
            candidate.display()
        ))
    })?;
    if !path_starts_with(&candidate, &root) {
        return Err(TransferConfigError::TransferDir(format!(
            "{} is outside the work directory {} (absolute paths must stay inside it)",
            candidate.display(),
            root.display()
        )));
    }
    Ok(candidate)
}

/// Resolve `.`/`..` textually without touching the filesystem. `None` when a
/// `..` would pop past the root.
fn normalize_lexical(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
        }
    }
    Some(out)
}

/// `true` when `candidate` is `base` or below it. Windows paths compare
/// case-insensitively.
fn path_starts_with(candidate: &Path, base: &Path) -> bool {
    let mut candidate = candidate.components();
    for expected in base.components() {
        match candidate.next() {
            Some(found)
                if found
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy()) => {}
            _ => return false,
        }
    }
    true
}

/// `true` for strings like `D:images` — a drive prefix without a root separator.
fn has_windows_drive(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Create the transfer root if needed, prove it is writable, and prove it is a
/// plain directory inside `work_root` after resolving links: a junction or
/// symlink swapped in beforehand must not redirect uploads elsewhere.
pub fn ensure_transfer_dir_writable(
    dir: &Path,
    work_root: &Path,
) -> Result<(), TransferConfigError> {
    // Reject any pre-planted symlink/junction on existing ancestors of `dir`
    // or `work_root` BEFORE calling `create_dir_all`, so directory creation
    // can never be redirected outside `work_root`.
    for target in [work_root, dir] {
        let mut cursor = Some(target);
        while let Some(node) = cursor {
            if let Ok(meta) = std::fs::symlink_metadata(node)
                && is_reparse(&meta)
            {
                return Err(TransferConfigError::TransferDir(format!(
                    "{} traverses a link or junction at {}",
                    target.display(),
                    node.display()
                )));
            }
            cursor = node.parent();
        }
    }

    std::fs::create_dir_all(work_root).map_err(|e| {
        TransferConfigError::TransferDir(format!("cannot create {}: {e}", work_root.display()))
    })?;
    std::fs::create_dir_all(dir).map_err(|e| {
        TransferConfigError::TransferDir(format!("cannot create {}: {e}", dir.display()))
    })?;
    let canonical_root = std::fs::canonicalize(work_root).map_err(|e| {
        TransferConfigError::TransferDir(format!("cannot resolve {}: {e}", work_root.display()))
    })?;
    let canonical_dir = std::fs::canonicalize(dir).map_err(|e| {
        TransferConfigError::TransferDir(format!("cannot resolve {}: {e}", dir.display()))
    })?;
    if !path_starts_with(&canonical_dir, &canonical_root) {
        return Err(TransferConfigError::TransferDir(format!(
            "{} resolves outside the work directory {} (junction/symlink?)",
            dir.display(),
            work_root.display()
        )));
    }
    let metadata = std::fs::symlink_metadata(dir).map_err(|e| {
        TransferConfigError::TransferDir(format!("cannot inspect {}: {e}", dir.display()))
    })?;
    if is_reparse(&metadata) {
        return Err(TransferConfigError::TransferDir(format!(
            "{} must be a plain directory, not a link or junction",
            dir.display()
        )));
    }
    let probe = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
        TransferConfigError::TransferDir(format!("{} is not writable: {e}", dir.display()))
    })?;
    drop(probe);
    Ok(())
}

/// `true` when the metadata describes a symlink/junction/reparse point.
fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names;
    use serde_json::json;

    fn exe_dir() -> PathBuf {
        PathBuf::from("C:/app")
    }

    #[test]
    fn empty_object_parses_to_default() {
        let parsed: TransferConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(parsed, TransferConfig::default());
    }

    #[test]
    fn default_serializes_to_empty_object() {
        let value = serde_json::to_value(TransferConfig::default()).unwrap();
        assert_eq!(value, json!({}));
    }

    #[test]
    fn camel_case_roundtrip_is_semantically_equal() {
        let config = TransferConfig {
            base_path: "/boards/".to_string(),
            transfer_dir: Some("C:/data/images".to_string()),
            max_upload_bytes: 1024,
            upload_expires_secs: 60,
            lock_timeout_secs: 3,
            reserved_disk_bytes: 4096,
            gc_interval_secs: 5,
            allowed_extensions: vec!["img".to_string()],
            verify_concurrency: 4,
            allow_zero_length_uploads: true,
            dut_storage_bytes: Some(8192),
            lan_access: true,
        };
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(value["basePath"], json!("/boards/"));
        assert_eq!(value["maxUploadBytes"], json!(1024));
        assert_eq!(value["lanAccess"], json!(true));
        let parsed: TransferConfig = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn snake_case_keys_silently_fall_back_to_defaults() {
        // Pit #15: unknown keys never fail parsing — they silently drop the
        // default path. Pin that behavior so roundtrip tests keep camelCase.
        let parsed: TransferConfig =
            serde_json::from_value(json!({ "upload_expires_secs": 5 })).unwrap();
        assert_eq!(parsed.upload_expires_secs, default_upload_expires_secs());
    }

    #[test]
    fn transfer_is_default_only_for_default() {
        assert!(transfer_is_default(&TransferConfig::default()));
        let changed = TransferConfig {
            lan_access: true,
            ..TransferConfig::default()
        };
        assert!(!transfer_is_default(&changed));
    }

    #[test]
    fn base_path_writings_are_equivalent() {
        for spelling in ["transfer", "/transfer", "/transfer/"] {
            assert_eq!(normalize_base_path(spelling).unwrap(), "/transfer");
        }
        assert_eq!(normalize_base_path("/a/b/").unwrap(), "/a/b");
        assert!(normalize_base_path("/").is_err());
        assert!(normalize_base_path("a\\b").is_err());
        assert!(normalize_base_path("/a//b").is_err());
        assert!(normalize_base_path("/a/../b").is_err());
    }

    #[test]
    fn upload_ttl_never_zero() {
        // A zero TTL would disable expiration in the tus handlers; clamp so a
        // misconfigured `0` cannot make abandoned uploads immortal.
        assert_eq!(
            TransferConfig::default().upload_ttl(),
            Duration::from_secs(default_upload_expires_secs())
        );
        assert_eq!(
            TransferConfig {
                upload_expires_secs: 0,
                ..TransferConfig::default()
            }
            .upload_ttl(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn transfer_dir_defaults_under_duthub_data() {
        assert_eq!(
            resolve_transfer_dir(None, &exe_dir()).unwrap(),
            exe_dir().join("duthub-data").join("images")
        );
        assert_eq!(
            resolve_transfer_dir(Some(""), &exe_dir()).unwrap(),
            resolve_transfer_dir(None, &exe_dir()).unwrap()
        );
        assert_eq!(
            resolve_transfer_dir(Some("images"), &exe_dir()).unwrap(),
            exe_dir().join("duthub-data").join("images")
        );
        assert_eq!(
            resolve_transfer_dir(Some("  images  "), &exe_dir()).unwrap(),
            exe_dir().join("duthub-data").join("images")
        );
    }

    #[test]
    fn transfer_dir_relative_paths_anchor_at_exe_dir() {
        assert_eq!(
            resolve_transfer_dir(Some("data/images"), &exe_dir()).unwrap(),
            exe_dir().join("data/images")
        );
    }

    #[test]
    fn transfer_dir_rejects_escapes_and_windows_oddities() {
        // Parent components that escape the work root are refused.
        assert!(resolve_transfer_dir(Some("../outside"), &exe_dir()).is_err());
        assert!(resolve_transfer_dir(Some("data/../.."), &exe_dir()).is_err());
        // Drive-relative, rooted-without-drive and UNC forms.
        assert!(resolve_transfer_dir(Some("D:images"), &exe_dir()).is_err());
        assert!(resolve_transfer_dir(Some("\\images"), &exe_dir()).is_err());
        assert!(resolve_transfer_dir(Some("\\\\srv\\share\\images"), &exe_dir()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn transfer_dir_absolute_must_stay_inside_the_work_root() {
        let inside = resolve_transfer_dir(Some("C:/app/data/images"), &exe_dir()).unwrap();
        assert_eq!(inside, PathBuf::from("C:/app/data/images"));
        // `..` is normalized before the containment check, so it cannot tunnel
        // out of the work root.
        assert!(resolve_transfer_dir(Some("C:/app/../outside"), &exe_dir()).is_err());
        for outside in ["C:/elsewhere/images", "D:/images", "C:/app2/images"] {
            assert!(
                resolve_transfer_dir(Some(outside), &exe_dir()).is_err(),
                "{outside}"
            );
        }
    }

    #[test]
    fn writability_probe_accepts_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_transfer_dir_writable(tmp.path(), tmp.path()).unwrap();
    }

    #[test]
    fn lock_timeout_never_zero() {
        assert_eq!(
            TransferConfig::default().lock_timeout(),
            Duration::from_secs(10)
        );
        let config = TransferConfig {
            lock_timeout_secs: 0,
            ..TransferConfig::default()
        };
        assert_eq!(config.lock_timeout(), Duration::from_secs(1));
    }

    #[test]
    fn allowed_extensions_feed_publish_validation() {
        let config = TransferConfig::default();
        assert!(names::validate_publish_name("flash.img", &config.allowed_extensions).is_ok());
        assert!(names::validate_publish_name("flash.iso", &config.allowed_extensions).is_ok());
        assert!(names::validate_publish_name("flash.exe", &config.allowed_extensions).is_err());
    }
}
