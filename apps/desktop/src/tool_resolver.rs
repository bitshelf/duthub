//! Desktop-side "downloaded tool first" resolution for external executables
//! (adb/hdc/ffmpeg/ffplay), backed by `duthub-tooldown` manifests under
//! `<config dir>/tools/<tool id>/`.
//!
//! Priority contract applied at every desktop spec-selection point:
//! 1. an explicit configured path or command resolves exactly as before;
//! 2. a downloaded install shadows PATH **only** for the canonical bare name
//!    (empty setting or a bare `<tool id>` reference, case-insensitive);
//! 3. otherwise the existing PATH behavior applies untouched.
//!
//! Pure functions only — no GPUI types, safe to import from integration tests.
//! Device crates (duthub-adb / duthub-flash) stay unchanged on purpose: they
//! keep receiving concrete spec strings and never see this module.
//!
//! The config directory anchor is `duthub_core::store::data_dir()` (the exe
//! sibling `.duthub/`, same chain `main.rs` uses via `initialize_config_dir`);
//! no path logic is invented here.

use std::path::{Path, PathBuf};

use duthub_core::model::{DownloadProxyMode, ToolDownloadSettings};
use duthub_tooldown::ProxyConfig;

/// Directory holding downloaded tools: `<config dir>/tools/`.
pub fn tools_root(config_dir: &Path) -> PathBuf {
    config_dir.join("tools")
}

/// The process config dir's tools root. `None` = the exe path is unresolvable;
/// callers then fall through to the pre-existing explicit/PATH resolution.
pub fn config_tools_root() -> Option<PathBuf> {
    duthub_core::store::data_dir()
        .ok()
        .map(|dir| tools_root(&dir))
}

/// First executable of a valid downloaded install of `tool_id`, or `None`.
/// Unknown tool id, missing install, corrupt manifest or missing executable
/// files all land on `None` (fail open into the existing resolution).
pub fn downloaded_executable_in(tools_root: &Path, tool_id: &str) -> Option<PathBuf> {
    let spec = duthub_tooldown::spec_by_id(tool_id)?;
    duthub_tooldown::installed_executable(tools_root, spec)
}

/// [`downloaded_executable_in`] anchored at the process config dir.
pub fn downloaded_executable(tool_id: &str) -> Option<PathBuf> {
    downloaded_executable_in(&config_tools_root()?, tool_id)
}

/// Downloaded install of `tool_id` together with the digest of the **executable**
/// recorded by the install kernel in `manifest.json`. Callers that re-verify
/// bytes at use time (the goose launch gate) need the digest of the file they
/// will run; the manifest's `sha256` covers the archive the executable was
/// extracted from, so comparing them can never match. A corrupt manifest, an
/// empty digest, or a manifest written before executable digests existed counts
/// as not-installed (reinstall required).
pub fn downloaded_install_in(tools_root: &Path, tool_id: &str) -> Option<(PathBuf, String)> {
    let spec = duthub_tooldown::spec_by_id(tool_id)?;
    let manifest =
        duthub_tooldown::read_manifest(&duthub_tooldown::installed_tool_dir(tools_root, spec))
            .ok()
            .flatten()?;
    if manifest.tool != spec.id || manifest.sha256.trim().is_empty() {
        return None;
    }
    let recorded = manifest.executable_sha256.first()?;
    if recorded.trim().is_empty() {
        return None;
    }
    let path = duthub_tooldown::installed_executable(tools_root, spec)?;
    Some((path, recorded.trim().to_owned()))
}

/// [`downloaded_install_in`] anchored at the process config dir.
pub fn downloaded_install(tool_id: &str) -> Option<(PathBuf, String)> {
    downloaded_install_in(&config_tools_root()?, tool_id)
}

/// Whether a downloaded install may shadow PATH for this setting: only the
/// canonical bare name qualifies (the empty shipped default or an explicit bare
/// `<tool id>` reference). A configured path, another command name or a
/// `%VAR%` expansion is never hijacked by a download. Public so the settings
/// page's effective-path display classifies rows with the same predicate the
/// resolver applies (single source of truth for the shadowing rule).
pub fn downloads_shadow(configured: &str, tool_id: &str) -> bool {
    let trimmed = configured.trim();
    trimmed.is_empty() || trimmed.eq_ignore_ascii_case(tool_id)
}

/// Effective spec for callers that hand a command string to the adb/hdc
/// clients (`AdbClient::devices_l`, `HdcClient::list_targets`, `Shell::start`):
/// the downloaded absolute path when it shadows, otherwise the configured
/// value with the historical bare-name fallback.
pub fn effective_spec_in(configured: &str, tool_id: &str, tools_root: &Path) -> String {
    if downloads_shadow(configured, tool_id)
        && let Some(path) = downloaded_executable_in(tools_root, tool_id)
    {
        return path.display().to_string();
    }
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        tool_id.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// [`effective_spec_in`] anchored at the process config dir.
pub fn effective_spec(configured: &str, tool_id: &str) -> String {
    let trimmed = configured.trim();
    match config_tools_root() {
        Some(root) => effective_spec_in(trimmed, tool_id, &root),
        None if trimmed.is_empty() => tool_id.to_owned(),
        None => trimmed.to_owned(),
    }
}

/// The downloaded executable when it should run instead of PATH for this
/// setting; `None` = keep the caller's existing resolution. Used by the media
/// page's ffmpeg/ffplay lookup ahead of its PATH search.
pub fn downloaded_preferred_in(
    configured: &str,
    tool_id: &str,
    tools_root: &Path,
) -> Option<PathBuf> {
    if downloads_shadow(configured, tool_id) {
        downloaded_executable_in(tools_root, tool_id)
    } else {
        None
    }
}

/// [`downloaded_preferred_in`] anchored at the process config dir.
pub fn downloaded_preferred(configured: &str, tool_id: &str) -> Option<PathBuf> {
    downloaded_preferred_in(configured, tool_id, &config_tools_root()?)
}

/// Map the stored proxy policy onto the download kernel's client config.
/// `Custom` passes the stored URL through verbatim: an unparsable URL fails
/// tooldown's client build with `InvalidProxyUrl` (no silent rewrite here).
pub fn proxy_config(download: &ToolDownloadSettings) -> ProxyConfig {
    match download.proxy.mode {
        DownloadProxyMode::Off => ProxyConfig::Off,
        DownloadProxyMode::Env => ProxyConfig::Env,
        DownloadProxyMode::Custom => ProxyConfig::Custom(download.proxy.url.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Fake a downloaded install under `root`: `manifest.json` plus one
    /// placeholder executable file. Uses the exact on-disk camelCase keys.
    fn fake_install(root: &Path, tool_id: &str, exe_name: &str) -> PathBuf {
        let dir = root.join(tool_id);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(exe_name);
        fs::write(&exe, b"MZ placeholder").unwrap();
        let manifest = serde_json::json!({
            "tool": tool_id,
            "url": format!("https://example.invalid/{tool_id}.zip"),
            "sha256": "0".repeat(64),
            "pinned": true,
            "checksumVerified": true,
            "installedAt": "2026-11-23T00:00:00Z",
            "executables": [exe.display().to_string()],
            "executableSha256": [duthub_core::store::sha256_hex(b"MZ placeholder")],
        });
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        exe
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "duthub-tool-resolver-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tools_root_is_config_dir_plus_tools() {
        assert_eq!(
            tools_root(Path::new("C:/lab/.duthub")),
            PathBuf::from("C:/lab/.duthub/tools")
        );
    }

    #[test]
    fn downloaded_install_resolves_first_manifest_executable() {
        let root = scratch_dir("resolve-hit");
        let exe = fake_install(&root, "adb", "adb.exe");
        assert_eq!(downloaded_executable_in(&root, "adb"), Some(exe));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_manifest_without_executable_digest_counts_as_not_installed() {
        let root = scratch_dir("resolve-legacy");
        let dir = root.join("goose");
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("goose.exe");
        fs::write(&exe, b"MZ placeholder").unwrap();
        // Manifest written before `executableSha256` existed: the archive digest
        // cannot verify the executable, so the launch gate must not trust it.
        let manifest = serde_json::json!({
            "tool": "goose",
            "url": "https://example.invalid/goose.zip",
            "sha256": "0".repeat(64),
            "pinned": true,
            "checksumVerified": true,
            "installedAt": "2026-11-23T00:00:00Z",
            "executables": [exe.display().to_string()],
        });
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(downloaded_install_in(&root, "goose"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_broken_installs_fall_through_to_none() {
        let root = scratch_dir("resolve-miss");
        // No install at all.
        assert_eq!(downloaded_executable_in(&root, "adb"), None);
        // Unknown tool id: no spec in the built-in table (hdc has none yet).
        assert_eq!(downloaded_executable_in(&root, "hdc"), None);
        // Corrupt manifest counts as not installed.
        let dir = root.join("adb");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("manifest.json"), "{not json").unwrap();
        assert_eq!(downloaded_executable_in(&root, "adb"), None);
        // Manifest whose executable vanished counts as not installed.
        let exe = fake_install(&root, "adb", "adb.exe");
        fs::remove_file(&exe).unwrap();
        assert_eq!(downloaded_executable_in(&root, "adb"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn downloaded_install_exposes_recorded_digest() {
        let root = scratch_dir("install-digest");
        // goose is in the built-in table (TOOL_GOOSE): path + the digest the
        // launch gate must compare — the executable's own bytes, not the
        // archive the manifest also records.
        let exe = fake_install(&root, "goose", "goose.exe");
        let (path, sha) =
            downloaded_install_in(&root, "goose").expect("goose must resolve with digest");
        assert_eq!(path, exe);
        assert_eq!(sha, duthub_core::store::sha256_hex(b"MZ placeholder"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn downloaded_install_rejects_empty_digest_manifest() {
        let root = scratch_dir("install-empty-digest");
        fake_install(&root, "goose", "goose.exe");
        let manifest_path = root.join("goose").join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        manifest["sha256"] = serde_json::Value::String(String::new());
        fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();
        assert_eq!(downloaded_install_in(&root, "goose"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_paths_and_other_names_are_never_shadowed() {
        let root = scratch_dir("shadow-guard");
        fake_install(&root, "adb", "adb.exe");
        let configured = r"C:\other\adb.exe";
        assert_eq!(
            effective_spec_in(configured, "adb", &root),
            configured,
            "显式路径优先于已下载"
        );
        assert_eq!(
            effective_spec_in("my-adb", "adb", &root),
            "my-adb",
            "非规范裸名保持既有 PATH 行为"
        );
        assert_eq!(
            effective_spec_in("%ADB_HOME%/adb.exe", "adb", &root),
            "%ADB_HOME%/adb.exe",
            "环境变量展开的 spec 不被已下载劫持"
        );
        // hdc has no downloadable spec: even the canonical name falls back.
        assert_eq!(effective_spec_in("hdc", "hdc", &root), "hdc");
        assert_eq!(effective_spec_in("", "hdc", &root), "hdc");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn canonical_bare_name_shadows_path_case_insensitively() {
        let root = scratch_dir("shadow-hit");
        let exe = fake_install(&root, "adb", "adb.exe");
        let expected = exe.display().to_string();
        // Empty shipped default, exact name, case-insensitive variant.
        assert_eq!(effective_spec_in("", "adb", &root), expected);
        assert_eq!(effective_spec_in("adb", "adb", &root), expected);
        assert_eq!(effective_spec_in("ADB", "adb", &root), expected);
        // Without a download the historical bare-name fallback holds.
        let empty = scratch_dir("shadow-miss");
        assert_eq!(effective_spec_in("", "adb", &empty), "adb");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn downloaded_preferred_matches_effective_spec_semantics() {
        let root = scratch_dir("preferred");
        let exe = fake_install(&root, "ffmpeg", "ffmpeg.exe");
        assert_eq!(
            downloaded_preferred_in("ffmpeg", "ffmpeg", &root),
            Some(exe.clone())
        );
        assert_eq!(downloaded_preferred_in("", "ffmpeg", &root), Some(exe));
        assert_eq!(downloaded_preferred_in("ffprobe", "ffmpeg", &root), None);
        assert_eq!(
            downloaded_preferred_in("C:/bin/ffmpeg.exe", "ffmpeg", &root),
            None
        );
        assert_eq!(downloaded_preferred_in("ffmpeg", "ffplay", &root), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn proxy_config_maps_every_mode_verbatim() {
        use duthub_core::model::DownloadProxySettings;

        let build = |mode, url: &str| {
            proxy_config(&ToolDownloadSettings {
                proxy: DownloadProxySettings {
                    mode,
                    url: url.to_string(),
                },
                ..ToolDownloadSettings::default()
            })
        };
        assert_eq!(build(DownloadProxyMode::Off, ""), ProxyConfig::Off);
        assert_eq!(build(DownloadProxyMode::Env, ""), ProxyConfig::Env);
        assert_eq!(
            build(DownloadProxyMode::Custom, "socks5://127.0.0.1:1080"),
            ProxyConfig::Custom("socks5://127.0.0.1:1080".to_string())
        );
    }
}
