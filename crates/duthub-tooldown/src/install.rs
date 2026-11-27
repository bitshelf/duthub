//! Download -> verify -> extract -> atomic install pipeline.
//!
//! Everything stages inside `<install_root>/.tooldown-<random>/` so the final move is a
//! same-volume rename (`fs::rename` cannot cross volumes — a %TEMP% staging dir would break
//! the atomic move). The staging `TempDir` cleans the archive and any partial extraction on
//! every early return: error, checksum mismatch or cancellation. The previous install is
//! left untouched until the new one is complete.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::chunks::{ChunkSource, HttpSource};
use crate::error::ToolInstallError;
use crate::extract;
use crate::manifest::{self, InstallManifest};
use crate::proxy::ProxyConfig;
use crate::spec::{EntryKind, ToolSpec};
use crate::timefmt;

/// Real-time download progress, reported through [`InstallOptions::progress`].
///
/// Both numbers are facts observed on the stream — `bytes_downloaded` counts the
/// bytes actually pulled, `total_bytes` mirrors the server's Content-Length and is
/// `None` when the response does not declare one. Callers must not invent progress
/// when `total_bytes` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    /// Bytes streamed so far (monotonic within one run).
    pub bytes_downloaded: u64,
    /// Total payload size when the server reports one, `None` otherwise.
    pub total_bytes: Option<u64>,
}

/// Progress sink signature. Invoked from the download task after every streamed
/// chunk; cheap bodies only (e.g. store the latest value behind a mutex). The
/// `'a` lifetime lets callers borrow local state instead of forcing `'static`.
pub type ProgressCallback<'a> = dyn Fn(DownloadProgress) + Send + Sync + 'a;

/// Options for one [`install_tool`] run. All paths are caller-supplied — this crate has
/// no opinion about where tools live and no workspace-internal dependencies.
pub struct InstallOptions<'a> {
    /// Root directory; the tool lands in `<install_root>/<spec.id>/`. Created when missing.
    pub install_root: &'a Path,
    /// Reinstall even when a valid install already exists (the replaced directory is
    /// parked inside staging and removed only after the new one is in place).
    pub force: bool,
    /// Cooperative cancellation, polled between stream chunks (and once more before the
    /// final move). A cancelled run cleans its staging dir and leaves any existing
    /// install untouched.
    pub cancel: Option<&'a AtomicBool>,
    /// Proxy policy for the HTTP client.
    pub proxy: &'a ProxyConfig,
    /// Optional progress sink (see [`DownloadProgress`]). `None` = silent.
    pub progress: Option<&'a ProgressCallback<'a>>,
}

impl std::fmt::Debug for InstallOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallOptions")
            .field("install_root", &self.install_root)
            .field("force", &self.force)
            .field("cancel", &self.cancel.is_some())
            .field("proxy", &self.proxy)
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

/// Result of an [`install_tool`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Fresh install (or forced reinstall) completed; contains the written manifest.
    Installed(InstallManifest),
    /// A valid install already existed and `force` was not set; contains the existing
    /// manifest (nothing was downloaded).
    AlreadyInstalled(InstallManifest),
    /// The cancel flag was observed before the stream ended; nothing was installed.
    Cancelled,
}

/// Download and install the tool described by `spec`.
///
/// No total request timeout: large tool archives over slow links must not be cut off
/// mid-stream; user cancellation goes through `InstallOptions::cancel`.
pub async fn install_tool(
    spec: &ToolSpec,
    opts: &InstallOptions<'_>,
) -> Result<InstallOutcome, ToolInstallError> {
    install_tool_from_url(spec, spec.url, opts).await
}

/// Validate a custom archive source. Unpinned Goose archives are restricted to
/// release assets from the official repository.
pub fn validate_download_url(
    spec: &ToolSpec,
    value: &str,
) -> Result<reqwest::Url, ToolInstallError> {
    let url =
        reqwest::Url::parse(value.trim()).map_err(|_| ToolInstallError::InvalidDownloadUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ToolInstallError::InvalidDownloadUrl);
    }
    if spec.id == "goose" && spec.pinned_sha256.is_none() {
        let parts: Vec<_> = url.path_segments().into_iter().flatten().collect();
        if url.host_str() != Some("github.com")
            || url.port().is_some()
            || !matches!(parts.as_slice(),
                ["aaif-goose", "goose", "releases", "latest", "download", name]
                | ["aaif-goose", "goose", "releases", "download", _, name]
                if *name == "goose-x86_64-pc-windows-msvc.zip")
        {
            return Err(ToolInstallError::InvalidDownloadUrl);
        }
    }
    Ok(url)
}

/// Install from an explicit source while retaining the tool's archive checks.
pub async fn install_tool_from_url(
    spec: &ToolSpec,
    value: &str,
    opts: &InstallOptions<'_>,
) -> Result<InstallOutcome, ToolInstallError> {
    let url = validate_download_url(spec, value)?;
    let client = opts.proxy.build_client()?;
    let response =
        client
            .get(url.clone())
            .send()
            .await
            .map_err(|source| ToolInstallError::Http {
                url: url.to_string(),
                source: source.without_url(),
            })?;
    if response.url().scheme() != "https" {
        return Err(ToolInstallError::InvalidDownloadUrl);
    }
    let response = response
        .error_for_status()
        .map_err(|source| ToolInstallError::HttpStatus {
            url: url.to_string(),
            status: source.status().unwrap_or_default(),
        })?;
    let mut source = HttpSource::new(response, url.as_str());
    install_from_source_at(spec, url.as_str(), opts, &mut source).await
}

/// The pipeline driven by an arbitrary chunk source (network source in production,
/// in-memory mocks in the tests).
#[cfg(test)]
async fn install_from_source<S: ChunkSource + ?Sized>(
    spec: &ToolSpec,
    opts: &InstallOptions<'_>,
    source: &mut S,
) -> Result<InstallOutcome, ToolInstallError> {
    install_from_source_at(spec, spec.url, opts, source).await
}

async fn install_from_source_at<S: ChunkSource + ?Sized>(
    spec: &ToolSpec,
    url: &str,
    opts: &InstallOptions<'_>,
    source: &mut S,
) -> Result<InstallOutcome, ToolInstallError> {
    let cancelled = || opts.cancel.is_some_and(|flag| flag.load(Ordering::SeqCst));
    if cancelled() {
        return Ok(InstallOutcome::Cancelled);
    }

    // 1. Idempotence: a valid install (manifest + all exes on disk) skips the download.
    let root = std::path::absolute(opts.install_root).map_err(|source| ToolInstallError::Io {
        path: opts.install_root.to_owned(),
        source,
    })?;
    ensure_plain_dir(&root)?;
    let tool_dir = root.join(spec.id);
    if !opts.force
        && let Some(existing) = manifest::validated_manifest(&tool_dir, spec)
    {
        tracing::info!(tool = spec.id, "already installed; skipping download");
        return Ok(InstallOutcome::AlreadyInstalled(existing));
    }

    // 2. Staging inside the install root (same volume as the final location).
    let staging = tempfile::Builder::new()
        .prefix(".tooldown-")
        .tempdir_in(&root)
        .map_err(|source| ToolInstallError::Io {
            path: root.clone(),
            source,
        })?;
    let staging_path = staging.path().to_owned();
    let zip_path = staging_path.join(format!("{}.zip", spec.id));

    // 3. Stream download + running digest. Any early return below drops `staging`,
    //    which deletes the downloaded archive and everything else inside.
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut file =
        tokio::fs::File::create(&zip_path)
            .await
            .map_err(|source| ToolInstallError::Io {
                path: zip_path.clone(),
                source,
            })?;
    loop {
        if cancelled() {
            tracing::info!(tool = spec.id, "cancelled mid-download; staging discarded");
            return Ok(InstallOutcome::Cancelled);
        }
        let Some(chunk) = source.next_chunk().await? else {
            break;
        };
        file.write_all(&chunk)
            .await
            .map_err(|source| ToolInstallError::Io {
                path: zip_path.clone(),
                source,
            })?;
        hasher.update(&chunk);
        downloaded += chunk.len() as u64;
        if let Some(progress) = opts.progress {
            progress(DownloadProgress {
                bytes_downloaded: downloaded,
                total_bytes: source.total_bytes(),
            });
        }
    }
    file.flush().await.map_err(|source| ToolInstallError::Io {
        path: zip_path.clone(),
        source,
    })?;
    drop(file);
    let digest = hex_encode(&hasher.finalize());

    // 4. Enforce the pin when present; without a pin the download is accepted UNVERIFIED
    //    and the measured digest is recorded as such in the manifest.
    if let Some(pin) = spec.pinned_sha256
        && !digest.eq_ignore_ascii_case(pin)
    {
        return Err(ToolInstallError::ChecksumMismatch {
            expected: pin.to_string(),
            actual: digest,
        });
    }

    // 5 & 6. Extract into staging and compute executable digests off the async
    //        worker pool using streaming 64 KiB reads so large binaries never
    //        spike heap usage or stall Tokio workers.
    if cancelled() {
        tracing::info!(
            tool = spec.id,
            "cancelled before extraction; staging discarded"
        );
        return Ok(InstallOutcome::Cancelled);
    }
    let extract_dir = staging_path.join("extract");
    let zip_for_task = zip_path.clone();
    let extract_for_task = extract_dir.clone();
    let tool_dir_for_task = tool_dir.clone();
    let spec_for_task = *spec;
    let (executables, executable_sha256) = tokio::task::spawn_blocking(move || {
        let zip_file =
            std::fs::File::open(&zip_for_task).map_err(|source| ToolInstallError::Io {
                path: zip_for_task.clone(),
                source,
            })?;
        let extraction = extract::extract_tool(zip_file, &spec_for_task, &extract_for_task)?;
        let mut executables: Vec<String> = Vec::new();
        let mut executable_sha256: Vec<String> = Vec::new();
        for (name, entry) in extraction.files.iter().zip(spec_for_task.entries) {
            if entry.kind != EntryKind::Executable {
                continue;
            }
            let staged = extract_for_task.join(name);
            let digest = sha256_file_streaming(&staged).map_err(|source| ToolInstallError::Io {
                path: staged.clone(),
                source,
            })?;
            executables.push(tool_dir_for_task.join(name).display().to_string());
            executable_sha256.push(digest);
        }
        Ok::<_, ToolInstallError>((executables, executable_sha256))
    })
    .await
    .map_err(|error| ToolInstallError::Io {
        path: zip_path.clone(),
        source: std::io::Error::other(format!("extraction task failed: {error}")),
    })??;
    let manifest = InstallManifest {
        tool: spec.id.to_string(),
        url: url.to_string(),
        sha256: digest,
        pinned: spec.pinned_sha256.is_some(),
        checksum_verified: spec.pinned_sha256.is_some(),
        installed_at: timefmt::now_rfc3339_utc(),
        executables,
        executable_sha256,
    };
    manifest::write(&extract_dir, &manifest)?;

    // 7. Atomic move into place; on forced reinstall the old directory is parked inside
    //    staging first (Windows rename fails onto an existing target) and is removed
    //    together with the staging dir afterwards.
    swap_into_place(&extract_dir, &tool_dir, &staging_path)?;
    tracing::info!(tool = spec.id, "installed");
    Ok(InstallOutcome::Installed(manifest))
}

fn sha256_file_streaming(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

/// Create `dir` if missing; refuse symlink/junction targets so an install cannot be
/// redirected elsewhere. Only the direct component is checked — validating the whole
/// ancestor chain stays the caller's job (the desktop passes `.duthub/tools` through its
/// own plain-dir helpers).
fn ensure_plain_dir(dir: &Path) -> Result<(), ToolInstallError> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            #[cfg(windows)]
            let reparse = {
                use std::os::windows::fs::MetadataExt;
                meta.file_attributes() & 0x0400 != 0
            };
            #[cfg(not(windows))]
            let reparse = false;
            if meta.is_symlink() || reparse || !meta.is_dir() {
                return Err(ToolInstallError::UnsafePath {
                    path: dir.to_owned(),
                });
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(|source| ToolInstallError::Io {
                path: dir.to_owned(),
                source,
            })
        }
        Err(source) => Err(ToolInstallError::Io {
            path: dir.to_owned(),
            source,
        }),
    }
}

fn swap_into_place(from: &Path, to: &Path, staging: &Path) -> Result<(), ToolInstallError> {
    let io_err = |path: &Path, source| ToolInstallError::Io {
        path: path.to_owned(),
        source,
    };
    let parked = staging.join("previous");
    if let Ok(meta) = std::fs::symlink_metadata(to) {
        if meta.is_symlink() {
            return Err(ToolInstallError::UnsafePath {
                path: to.to_owned(),
            });
        }
        // Existing real directory (forced reinstall / update): park it inside staging —
        // Windows rename fails onto an existing target. The parked copy is deleted
        // together with the staging TempDir after the new install lands.
        std::fs::rename(to, &parked).map_err(|source| io_err(to, source))?;
    }
    if let Err(move_in) = std::fs::rename(from, to) {
        // Update contract: when the new version fails to land, the previous install must
        // stay usable. Put the parked old install back before the staging TempDir erases
        // it with the rest of staging.
        if parked.exists()
            && let Err(restore) = std::fs::rename(&parked, to)
        {
            tracing::warn!(
                to = %to.display(),
                "failed to restore previous install after failed swap: {restore}"
            );
        }
        return Err(io_err(from, move_in));
    }
    Ok(())
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn download_overrides_preserve_archive_trust() {
        use crate::{TOOL_ADB, TOOL_GOOSE};

        assert!(validate_download_url(&TOOL_ADB, "https://mirror.example/adb.zip").is_ok());
        assert!(validate_download_url(&TOOL_ADB, "http://mirror.example/adb.zip").is_err());
        assert!(
            validate_download_url(&TOOL_ADB, "https://user:pass@mirror.example/adb.zip").is_err()
        );
        assert!(validate_download_url(&TOOL_GOOSE, TOOL_GOOSE.url).is_ok());
        assert!(validate_download_url(&TOOL_GOOSE, "https://github.com/aaif-goose/goose/releases/download/v1.60.2/goose-x86_64-pc-windows-msvc.zip").is_ok());
        assert!(validate_download_url(&TOOL_GOOSE, "https://mirror.example/goose.zip").is_err());
    }

    use super::*;
    use crate::extract::tests::build_zip;
    use crate::spec::{exe, support};
    use std::sync::Arc;

    static TEST_ENTRIES: [crate::spec::ExtractEntry; 2] = [exe("tool.exe"), support("helper.dll")];

    fn test_spec(pin: Option<&'static str>) -> ToolSpec {
        ToolSpec {
            id: "testtool",
            url: "https://example.invalid/test.zip",
            pinned_sha256: pin,
            entries: &TEST_ENTRIES,
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex_encode(&Sha256::digest(bytes))
    }

    /// Mock chunk source for the offline pipeline tests.
    struct MockSource {
        chunks: std::collections::VecDeque<Vec<u8>>,
        calls: usize,
        /// Runs after each successfully yielded chunk (tests arm the cancel flag here).
        after_chunk: Option<Box<dyn FnMut() + Send>>,
    }

    impl MockSource {
        fn from_chunks(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
                calls: 0,
                after_chunk: None,
            }
        }
    }

    impl ChunkSource for MockSource {
        #[expect(
            clippy::unused_async_trait_impl,
            reason = "test mock satisfies async ChunkSource trait without awaiting"
        )]
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ToolInstallError> {
            self.calls += 1;
            match self.chunks.pop_front() {
                Some(chunk) => {
                    if let Some(hook) = self.after_chunk.as_mut() {
                        hook();
                    }
                    Ok(Some(chunk))
                }
                None => Ok(None),
            }
        }
    }

    fn opts_for<'a>(
        root: &'a Path,
        force: bool,
        cancel: Option<&'a AtomicBool>,
    ) -> InstallOptions<'a> {
        InstallOptions {
            install_root: root,
            force,
            cancel,
            proxy: &ProxyConfig::Off,
            progress: None,
        }
    }

    /// No staging directories left behind under `root`.
    fn assert_no_staging_leftovers(root: &Path) {
        for entry in std::fs::read_dir(root).expect("read install root") {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().starts_with(".tooldown-"),
                "staging leftover: {name:?}"
            );
        }
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn pinned_install_is_atomic_and_manifest_records_absolute_exe_paths() {
        let zip_bytes = build_zip(&[
            ("t/tool.exe", &b"MZtool"[..]),
            ("t/helper.dll", &b"MZdll"[..]),
            ("x/other.txt", &b"skipped"[..]),
        ])
        .into_inner();
        let digest = sha256_hex(&zip_bytes);
        // Leak into &'static for the spec pin (test-only).
        let pin: &'static str = Box::leak(digest.clone().into_boxed_str());
        let spec = test_spec(Some(pin));

        let root = tempfile::tempdir().unwrap();
        let mut mock = MockSource::from_chunks(vec![zip_bytes]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), false, None), &mut mock)
            .await
            .unwrap();
        let InstallOutcome::Installed(manifest) = outcome else {
            panic!("expected Installed, got {outcome:?}");
        };

        // Absolute final paths, executables only (the dll is not reported).
        let expected_root = std::path::absolute(root.path()).unwrap();
        assert_eq!(
            manifest.executables,
            vec![
                expected_root
                    .join("testtool")
                    .join("tool.exe")
                    .display()
                    .to_string()
            ]
        );
        assert_eq!(manifest.tool, "testtool");
        assert_eq!(manifest.sha256, digest);
        assert_eq!(
            manifest.executable_sha256,
            vec![sha256_hex(b"MZtool")],
            "manifest 必须记录可执行文件自身的摘要，启动门才能校验真实运行的文件"
        );
        assert!(manifest.pinned && manifest.checksum_verified);
        assert_ne!(manifest.installed_at, "");

        // Files on disk, extras skipped, staging cleaned up.
        let tool_dir = root.path().join("testtool");
        assert_eq!(std::fs::read(tool_dir.join("tool.exe")).unwrap(), b"MZtool");
        assert_eq!(
            std::fs::read(tool_dir.join("helper.dll")).unwrap(),
            b"MZdll"
        );
        assert!(!tool_dir.join("other.txt").exists());
        assert!(tool_dir.join("manifest.json").is_file());
        assert_no_staging_leftovers(root.path());
        // One chunk call + one end-of-stream call for a single-chunk source.
        assert_eq!(mock.calls, 2);
    }

    #[tokio::test]
    async fn unpinned_download_is_recorded_as_unverified() {
        let zip_bytes = build_zip(&[
            ("t/tool.exe", &b"MZtool"[..]),
            ("t/helper.dll", &b"MZdll"[..]),
        ])
        .into_inner();
        let digest = sha256_hex(&zip_bytes);
        let spec = test_spec(None);

        let root = tempfile::tempdir().unwrap();
        let mut mock = MockSource::from_chunks(vec![zip_bytes]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), false, None), &mut mock)
            .await
            .unwrap();
        let InstallOutcome::Installed(manifest) = outcome else {
            panic!("expected Installed, got {outcome:?}");
        };
        assert!(!manifest.pinned);
        assert!(!manifest.checksum_verified);
        assert_eq!(manifest.sha256, digest);
    }

    #[tokio::test]
    async fn checksum_mismatch_fails_and_cleans_up_staging() {
        let zip_bytes = build_zip(&[
            ("t/tool.exe", &b"MZtool"[..]),
            ("t/helper.dll", &b"MZdll"[..]),
        ])
        .into_inner();
        let measured = sha256_hex(&zip_bytes);
        let wrong_pin: &'static str = Box::leak("0".repeat(64).into_boxed_str());
        let spec = test_spec(Some(wrong_pin));

        let root = tempfile::tempdir().unwrap();
        let mut mock = MockSource::from_chunks(vec![zip_bytes]);
        let err = install_from_source(&spec, &opts_for(root.path(), false, None), &mut mock)
            .await
            .unwrap_err();
        let ToolInstallError::ChecksumMismatch { expected, actual } = err else {
            panic!("expected ChecksumMismatch, got {err:?}");
        };
        assert_eq!(expected, wrong_pin);
        assert_eq!(actual, measured);

        // Half-finished install is fully cleaned: no tool dir, no staging leftovers.
        assert!(!root.path().join("testtool").exists());
        assert_no_staging_leftovers(root.path());
    }

    #[tokio::test]
    async fn cancel_flag_stops_the_download_loop_and_cleans_up() {
        let zip_bytes = build_zip(&[
            ("t/tool.exe", &b"MZtool"[..]),
            ("t/helper.dll", &b"MZdll"[..]),
        ])
        .into_inner();
        let spec = test_spec(None);

        let root = tempfile::tempdir().unwrap();
        // Arc so the hook can own the flag (Box<dyn FnMut()> requires 'static).
        let cancel = Arc::new(AtomicBool::new(false));
        let mut mock = MockSource::from_chunks(vec![
            vec![0xAA; 8],
            zip_bytes[..zip_bytes.len() / 2].to_vec(),
            zip_bytes[zip_bytes.len() / 2..].to_vec(),
        ]);
        // Arm the flag after the first chunk so the loop observes it between chunks.
        let flag = Arc::clone(&cancel);
        mock.after_chunk = Some(Box::new(move || flag.store(true, Ordering::SeqCst)));

        let outcome = install_from_source(
            &spec,
            &opts_for(root.path(), false, Some(cancel.as_ref())),
            &mut mock,
        )
        .await
        .unwrap();
        assert_eq!(outcome, InstallOutcome::Cancelled);
        // The first chunk was pulled; the loop aborted before pulling the second one.
        assert_eq!(mock.calls, 1);
        assert!(!root.path().join("testtool").exists());
        assert_no_staging_leftovers(root.path());
    }

    #[tokio::test]
    async fn cancel_before_start_downloads_nothing() {
        let spec = test_spec(None);
        let root = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(true);
        let mut mock = MockSource::from_chunks(vec![vec![0xAA]]);
        let outcome = install_from_source(
            &spec,
            &opts_for(root.path(), false, Some(&cancel)),
            &mut mock,
        )
        .await
        .unwrap();
        assert_eq!(outcome, InstallOutcome::Cancelled);
        assert_eq!(mock.calls, 0);
        assert_no_staging_leftovers(root.path());
    }

    #[tokio::test]
    async fn valid_install_skips_download_broken_install_repairs() {
        let zip_bytes = build_zip(&[
            ("t/tool.exe", &b"MZtool"[..]),
            ("t/helper.dll", &b"MZdll"[..]),
        ])
        .into_inner();
        let spec = test_spec(None);

        let root = tempfile::tempdir().unwrap();
        let mut first = MockSource::from_chunks(vec![zip_bytes.clone()]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), false, None), &mut first)
            .await
            .unwrap();
        assert!(matches!(outcome, InstallOutcome::Installed(_)));

        // Second run: valid install exists -> no download at all.
        let mut second = MockSource::from_chunks(vec![zip_bytes.clone()]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), false, None), &mut second)
            .await
            .unwrap();
        let InstallOutcome::AlreadyInstalled(manifest) = outcome else {
            panic!("expected AlreadyInstalled, got {outcome:?}");
        };
        assert_eq!(manifest.executables.len(), 1);
        assert_eq!(second.calls, 0, "skip must not download");

        // Broken install (exe deleted) counts as not installed -> repaired by a rerun.
        std::fs::remove_file(root.path().join("testtool").join("tool.exe")).unwrap();
        let mut third = MockSource::from_chunks(vec![zip_bytes]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), false, None), &mut third)
            .await
            .unwrap();
        assert!(matches!(outcome, InstallOutcome::Installed(_)));
        assert_eq!(third.calls, 2);
        assert!(root.path().join("testtool").join("tool.exe").is_file());
    }

    #[tokio::test]
    async fn forced_reinstall_replaces_previous_directory() {
        let spec = test_spec(None);
        let root = tempfile::tempdir().unwrap();

        let old_zip = build_zip(&[
            ("t/tool.exe", &b"OLD"[..]),
            ("t/helper.dll", &b"OLDdll"[..]),
        ])
        .into_inner();
        let mut first = MockSource::from_chunks(vec![old_zip]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), false, None), &mut first)
            .await
            .unwrap();
        assert!(matches!(outcome, InstallOutcome::Installed(_)));
        // Simulate drift in the previous install directory.
        std::fs::write(root.path().join("testtool").join("stale.txt"), b"old").unwrap();

        let new_zip = build_zip(&[
            ("t/tool.exe", &b"NEW"[..]),
            ("t/helper.dll", &b"NEWdll"[..]),
        ])
        .into_inner();
        let mut second = MockSource::from_chunks(vec![new_zip]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), true, None), &mut second)
            .await
            .unwrap();
        assert!(matches!(outcome, InstallOutcome::Installed(_)));

        // Replaced, not merged: new bytes in place, stale file gone, staging cleaned.
        let tool_dir = root.path().join("testtool");
        assert_eq!(std::fs::read(tool_dir.join("tool.exe")).unwrap(), b"NEW");
        assert_eq!(
            std::fs::read(tool_dir.join("helper.dll")).unwrap(),
            b"NEWdll"
        );
        assert!(!tool_dir.join("stale.txt").exists());
        assert!(tool_dir.join("manifest.json").is_file());
        assert_no_staging_leftovers(root.path());
    }

    #[test]
    fn ensure_plain_dir_rejects_files_and_creates_missing_roots() {
        let root = tempfile::tempdir().unwrap();
        let file_path = root.path().join("not-a-dir");
        std::fs::write(&file_path, b"x").unwrap();
        let err = ensure_plain_dir(&file_path).unwrap_err();
        assert!(
            matches!(err, ToolInstallError::UnsafePath { .. }),
            "got {err:?}"
        );
        // Missing nested root is created.
        let nested = root.path().join("a").join("b");
        ensure_plain_dir(&nested).unwrap();
        assert!(nested.is_dir());
    }

    fn read_installed(root: &Path, name: &str) -> Vec<u8> {
        std::fs::read(root.join("testtool").join(name)).unwrap()
    }

    /// Update semantics: force-install of different bytes replaces manifest + exes and
    /// leaves nothing of v1 behind (update = `install_tool(force = true)`).
    #[tokio::test]
    async fn update_flow_v1_to_v2_replaces_manifest_and_exes() {
        let spec = test_spec(None);
        let root = tempfile::tempdir().unwrap();

        let v1_zip = build_zip(&[
            ("t/tool.exe", &b"goose-v1-bytes"[..]),
            ("t/helper.dll", &b"dll-v1"[..]),
        ])
        .into_inner();
        let mut first = MockSource::from_chunks(vec![v1_zip]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), false, None), &mut first)
            .await
            .unwrap();
        let InstallOutcome::Installed(v1_manifest) = outcome else {
            panic!("expected Installed for v1, got {outcome:?}");
        };
        // A v1-only stray file must not survive the update (replaced, not merged).
        std::fs::write(root.path().join("testtool").join("v1-only.txt"), b"old").unwrap();

        let v2_zip = build_zip(&[
            ("t/tool.exe", &b"goose-v2-bytes"[..]),
            ("t/helper.dll", &b"dll-v2"[..]),
        ])
        .into_inner();
        let v2_digest = sha256_hex(&v2_zip);
        let mut second = MockSource::from_chunks(vec![v2_zip]);
        let outcome = install_from_source(&spec, &opts_for(root.path(), true, None), &mut second)
            .await
            .unwrap();
        let InstallOutcome::Installed(v2_manifest) = outcome else {
            panic!("expected Installed for v2, got {outcome:?}");
        };

        assert_eq!(read_installed(root.path(), "tool.exe"), b"goose-v2-bytes");
        assert_eq!(read_installed(root.path(), "helper.dll"), b"dll-v2");
        assert!(
            !root.path().join("testtool").join("v1-only.txt").exists(),
            "v1-only file must not survive the update"
        );
        assert_eq!(v2_manifest.sha256, v2_digest);
        assert_ne!(v2_manifest.sha256, v1_manifest.sha256);
        // The on-disk manifest reflects v2, not v1.
        let on_disk = manifest::read_manifest(&root.path().join("testtool"))
            .unwrap()
            .unwrap();
        assert_eq!(on_disk, v2_manifest);
        assert_no_staging_leftovers(root.path());
    }

    /// Cancelled update: the previous install stays byte-identical and usable.
    #[tokio::test]
    async fn cancelled_update_keeps_old_install_usable() {
        let spec = test_spec(None);
        let root = tempfile::tempdir().unwrap();

        let v1_zip = build_zip(&[
            ("t/tool.exe", &b"goose-v1-bytes"[..]),
            ("t/helper.dll", &b"dll-v1"[..]),
        ])
        .into_inner();
        let v1_digest = sha256_hex(&v1_zip);
        let mut first = MockSource::from_chunks(vec![v1_zip]);
        install_from_source(&spec, &opts_for(root.path(), false, None), &mut first)
            .await
            .unwrap();

        // v2 update with the cancel flag armed after the first chunk.
        let v2_zip = build_zip(&[
            ("t/tool.exe", &b"goose-v2-bytes"[..]),
            ("t/helper.dll", &b"dll-v2"[..]),
        ])
        .into_inner();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut second = MockSource::from_chunks(vec![
            vec![0xBB; 8],
            v2_zip[..v2_zip.len() / 2].to_vec(),
            v2_zip[v2_zip.len() / 2..].to_vec(),
        ]);
        let flag = Arc::clone(&cancel);
        second.after_chunk = Some(Box::new(move || flag.store(true, Ordering::SeqCst)));
        let outcome = install_from_source(
            &spec,
            &opts_for(root.path(), true, Some(cancel.as_ref())),
            &mut second,
        )
        .await
        .unwrap();
        assert_eq!(outcome, InstallOutcome::Cancelled);

        // v1 survives byte-identically, still recognized as installed.
        assert_eq!(read_installed(root.path(), "tool.exe"), b"goose-v1-bytes");
        assert_eq!(read_installed(root.path(), "helper.dll"), b"dll-v1");
        let on_disk = manifest::read_manifest(&root.path().join("testtool"))
            .unwrap()
            .unwrap();
        assert_eq!(on_disk.sha256, v1_digest);
        assert!(manifest::is_installed(&root.path().join("testtool"), &spec));
        assert_no_staging_leftovers(root.path());
    }

    /// Checksum-failed update: the previous install stays byte-identical and usable.
    #[tokio::test]
    async fn checksum_failed_update_keeps_old_install_usable() {
        let root = tempfile::tempdir().unwrap();

        let v1_zip = build_zip(&[
            ("t/tool.exe", &b"goose-v1-bytes"[..]),
            ("t/helper.dll", &b"dll-v1"[..]),
        ])
        .into_inner();
        let v1_digest = sha256_hex(&v1_zip);
        // Pin frozen at v1: emulates a spec whose upstream moved on (stale pin scenario).
        let v1_pin: &'static str = Box::leak(v1_digest.clone().into_boxed_str());
        let spec = test_spec(Some(v1_pin));
        let mut first = MockSource::from_chunks(vec![v1_zip]);
        install_from_source(&spec, &opts_for(root.path(), false, None), &mut first)
            .await
            .unwrap();

        let v2_zip = build_zip(&[
            ("t/tool.exe", &b"goose-v2-bytes"[..]),
            ("t/helper.dll", &b"dll-v2"[..]),
        ])
        .into_inner();
        let v2_digest = sha256_hex(&v2_zip);
        let mut second = MockSource::from_chunks(vec![v2_zip]);
        let err = install_from_source(&spec, &opts_for(root.path(), true, None), &mut second)
            .await
            .unwrap_err();
        let ToolInstallError::ChecksumMismatch { expected, actual } = err else {
            panic!("expected ChecksumMismatch for stale-pin update, got {err:?}");
        };
        assert_eq!(expected, v1_digest);
        assert_eq!(actual, v2_digest);

        // v1 survives byte-identically, still recognized as installed.
        assert_eq!(read_installed(root.path(), "tool.exe"), b"goose-v1-bytes");
        assert_eq!(read_installed(root.path(), "helper.dll"), b"dll-v1");
        let on_disk = manifest::read_manifest(&root.path().join("testtool"))
            .unwrap()
            .unwrap();
        assert_eq!(on_disk.sha256, v1_digest);
        assert!(manifest::is_installed(&root.path().join("testtool"), &spec));
        assert_no_staging_leftovers(root.path());
    }

    /// If the new version fails to move in AFTER the old install was parked, the old
    /// install is restored — it must not be lost together with the staging dir.
    #[test]
    fn swap_failure_restores_previous_install() {
        let staging = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let tool_dir = root.path().join("testtool");
        std::fs::create_dir_all(&tool_dir).unwrap();
        std::fs::write(tool_dir.join("tool.exe"), b"old-bytes").unwrap();

        // `from` does not exist -> parking succeeds, moving in fails, restore kicks in.
        let missing = staging.path().join("no-such-extract");
        let err = swap_into_place(&missing, &tool_dir, staging.path()).unwrap_err();
        assert!(matches!(err, ToolInstallError::Io { .. }), "got {err:?}");
        assert_eq!(
            std::fs::read(tool_dir.join("tool.exe")).unwrap(),
            b"old-bytes",
            "previous install must be restored after a failed swap"
        );
    }

    /// Progress sink receives monotonic byte counts and the final count equals the
    /// streamed payload size. The mock source declares no total, so `total_bytes`
    /// must stay `None` (progress reporting never invents a total).
    #[tokio::test]
    async fn progress_callback_reports_monotonic_bytes() {
        let zip_bytes = build_zip(&[
            ("t/tool.exe", &b"MZtool"[..]),
            ("t/helper.dll", &b"MZdll"[..]),
        ])
        .into_inner();
        let spec = test_spec(None);
        let root = tempfile::tempdir().unwrap();

        let seen: Arc<std::sync::Mutex<Vec<DownloadProgress>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let opts = InstallOptions {
            install_root: root.path(),
            force: false,
            cancel: None,
            proxy: &ProxyConfig::Off,
            progress: Some(&|progress: DownloadProgress| {
                sink.lock().unwrap().push(progress);
            }),
        };
        let mut mock = MockSource::from_chunks(vec![
            vec![0x11; 16],
            zip_bytes[..zip_bytes.len() / 2].to_vec(),
            zip_bytes[zip_bytes.len() / 2..].to_vec(),
        ]);
        let outcome = install_from_source(&spec, &opts, &mut mock).await.unwrap();
        assert!(matches!(outcome, InstallOutcome::Installed(_)));

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            3,
            "one progress event per chunk (3 chunks here)"
        );
        assert!(
            seen.windows(2)
                .all(|pair| pair[0].bytes_downloaded < pair[1].bytes_downloaded),
            "byte counts must be strictly monotonic: {seen:?}"
        );
        let last = seen.last().unwrap();
        assert_eq!(
            last.bytes_downloaded,
            16 + zip_bytes.len() as u64,
            "final count must equal the streamed payload"
        );
        assert!(
            seen.iter().all(|p| p.total_bytes.is_none()),
            "mock source declares no total; reported total must stay None"
        );
    }
}
