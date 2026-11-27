//! Tool-download page support (settings → Tools download / Download proxy).
//! Pure logic plus
//! the background install center bridging the `duthub-tooldown` kernel to the
//! UI tick — no GPUI types in here, integration tests
//! (`apps/desktop/tests/tool_ui_logic.rs`) drive everything directly.
//!
//! Blocks:
//! - Progress math: real byte counts only (`progress_percent` returns `None`
//!   when the server declared no total — the UI must not invent progress).
//! - Proxy URL advisory pre-check (the download kernel's client build stays
//!   the authoritative validation at install time).
//! - Effective-path classification per tool row, sharing the resolver's
//!   shadowing predicate (`tool_resolver::downloads_shadow`) so the display
//!   can never claim a different priority order than the resolution applies.
//! - `ToolInstallCenter`: one download at a time, cooperative cancel via
//!   `AtomicBool`, progress + terminal results handed back through mutex
//!   slots drained by the 50ms UI tick (same shape as the model-test slot).
//! - `.tooldown-*` staging sweep for leftovers after a crashed/killed run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, atomic::AtomicBool};

use duthub_tooldown::{DownloadProgress, InstallOutcome, ToolInstallError, ToolSpec};

// ---------------------------------------------------------------------------
// Progress display math
// ---------------------------------------------------------------------------

/// Latest download progress as reported by the kernel (facts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    /// Bytes actually received so far.
    pub bytes: u64,
    /// Total payload size when the server declared one (`None` otherwise).
    pub total: Option<u64>,
}

/// Percent 0..=100, but only when a real total is known. `None` = unknown
/// total (chunked response) — callers must show an indeterminate state
/// instead of fabricating a percentage.
#[must_use]
pub fn progress_percent(bytes: u64, total: Option<u64>) -> Option<u8> {
    let total = total?;
    if total == 0 {
        return None;
    }
    Some(u8::try_from(((bytes.min(total)) * 100) / total).unwrap_or(100))
}

/// Decimal byte units (B/KB/MB/…) for "received X" style status lines.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    #[expect(
        clippy::cast_precision_loss,
        reason = "human-readable byte size display formatted to 1 decimal place"
    )]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Proxy URL advisory pre-check
// ---------------------------------------------------------------------------

/// Why the advisory pre-check rejected a custom proxy URL. The download
/// kernel's `ProxyConfig::build_client` remains the authoritative gate; this
/// only catches obvious mistakes before they are persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyUrlProblem {
    /// No `scheme://` prefix at all.
    NoScheme,
    /// A scheme other than http/https/socks5.
    UnsupportedScheme(String),
    /// Scheme present but no host (e.g. `http://`, `socks5:// `).
    MissingHost,
}

impl ProxyUrlProblem {
    /// Short reason text for `{reason}` interpolation in the localized toast.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            ProxyUrlProblem::NoScheme => {
                "http://, https:// or socks5:// prefix required".to_string()
            }
            ProxyUrlProblem::UnsupportedScheme(scheme) => {
                format!("unsupported scheme {scheme:?} (use http/https/socks5)")
            }
            ProxyUrlProblem::MissingHost => "host is missing".to_string(),
        }
    }
}

/// Advisory validation of a custom proxy URL: scheme must be http/https/socks5
/// (case-insensitive) and a host must be present. Deliberately *not* a full
/// URL parser — anything it passes still gets the kernel's build-time check.
pub fn validate_custom_proxy_url(url: &str) -> Result<(), ProxyUrlProblem> {
    let trimmed = url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Err(ProxyUrlProblem::NoScheme);
    };
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" | "socks5" => {}
        other => return Err(ProxyUrlProblem::UnsupportedScheme(other.to_string())),
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_part = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    // Bracketed IPv6 hosts: `[::1]` → check the inside; an unbalanced bracket
    // is rejected here (the kernel would reject it at build time anyway).
    let bare_host = if let Some(after) = host_part.strip_prefix('[') {
        match after.split_once(']') {
            Some((inside, _)) => inside,
            None => return Err(ProxyUrlProblem::MissingHost),
        }
    } else {
        host_part.split(':').next().unwrap_or("")
    };
    if bare_host.trim().is_empty() {
        Err(ProxyUrlProblem::MissingHost)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Effective path classification (per tool row)
// ---------------------------------------------------------------------------

/// How a tool's executable is resolved for the row's "current effective path"
/// display. Mirrors `tool_resolver`'s priority contract: explicit configured
/// path/command > downloaded install (bare canonical name only) > PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveToolPath {
    /// The settings value is explicit (path or non-canonical command) and is
    /// used verbatim — downloads never hijack it.
    Explicit(String),
    /// A downloaded install shadows PATH (empty/bare-canonical setting).
    Downloaded(PathBuf),
    /// No download shadowing; the bare name goes through PATH as before.
    PathFallback(String),
    /// The tool has no direct settings key here (goose): the Agent registry
    /// resolves it (external config > downloaded > bundled > PATH).
    ManagedElsewhere,
}

/// Classify one tool's effective resolution. `configured` is the raw settings
/// value (`None` for tools without a settings key, i.e. goose).
#[must_use]
pub fn effective_tool_path(
    configured: Option<&str>,
    tool_id: &str,
    tools_root: Option<&Path>,
) -> EffectiveToolPath {
    let downloaded = |tools_root: Option<&Path>| -> Option<PathBuf> {
        tools_root.and_then(|root| crate::tool_resolver::downloaded_executable_in(root, tool_id))
    };
    let Some(configured) = configured else {
        return match downloaded(tools_root) {
            Some(path) => EffectiveToolPath::Downloaded(path),
            None => EffectiveToolPath::ManagedElsewhere,
        };
    };
    if crate::tool_resolver::downloads_shadow(configured, tool_id)
        && let Some(path) = downloaded(tools_root)
    {
        return EffectiveToolPath::Downloaded(path);
    }
    let trimmed = configured.trim();
    // Empty and bare-canonical values both resolve as PATH lookups (the
    // historical behavior); anything else is an explicit path/command.
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(tool_id) {
        EffectiveToolPath::PathFallback(tool_id.to_owned())
    } else {
        EffectiveToolPath::Explicit(trimmed.to_owned())
    }
}

// ---------------------------------------------------------------------------
// Tool row snapshot (settings page cache)
// ---------------------------------------------------------------------------

/// One tool row's on-disk facts, refreshed on page open / install finish.
/// `installed` mirrors the kernel's validated-install notion (manifest parses,
/// belongs to this tool, every executable still on disk).
#[derive(Debug, Clone)]
pub struct ToolRowSnapshot {
    pub spec_id: &'static str,
    pub installed: bool,
    /// First executable of a valid downloaded install.
    pub install_path: Option<PathBuf>,
    /// Manifest install time (RFC3339 UTC) when a manifest is present.
    pub installed_at: Option<String>,
    /// Manifest `checksumVerified` (when present). `false` for rolling assets
    /// like goose is displayed honestly, never restyled as verified.
    pub checksum_verified: Option<bool>,
    /// Manifest `pinned` (when present); `false` = rolling, unpinned asset.
    pub pinned: Option<bool>,
    pub effective: EffectiveToolPath,
}

/// Build the snapshot for one built-in spec against `tools_root`.
#[must_use]
pub fn tool_row_snapshot(
    spec: &ToolSpec,
    configured: Option<&str>,
    tools_root: Option<&Path>,
) -> ToolRowSnapshot {
    let install_path =
        tools_root.and_then(|root| crate::tool_resolver::downloaded_executable_in(root, spec.id));
    let manifest = tools_root.and_then(|root| {
        duthub_tooldown::read_manifest(&duthub_tooldown::installed_tool_dir(root, spec))
            .ok()
            .flatten()
    });
    // Only our own manifest counts as facts for this row.
    let manifest = manifest.filter(|m| m.tool == spec.id);
    ToolRowSnapshot {
        spec_id: spec.id,
        installed: install_path.is_some(),
        install_path,
        installed_at: manifest.as_ref().map(|m| m.installed_at.clone()),
        checksum_verified: manifest.as_ref().map(|m| m.checksum_verified),
        pinned: manifest.as_ref().map(|m| m.pinned),
        effective: effective_tool_path(configured, spec.id, tools_root),
    }
}

// ---------------------------------------------------------------------------
// Install outcomes / failures (kernel errors → display kinds)
// ---------------------------------------------------------------------------

/// What the row was trying to do when it started the install run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPurpose {
    /// First download (nothing installed yet).
    Download,
    /// Re-download over the existing install (repair).
    Redownload,
    /// Force reinstall of the latest upstream build.
    Update,
}

impl ToolPurpose {
    /// Redownload/Update run the kernel with `force = true`; a plain Download
    /// is the idempotent variant that skips when a valid install exists.
    #[must_use]
    pub fn forced(self) -> bool {
        !matches!(self, ToolPurpose::Download)
    }
}

/// Terminal result of one install run (mirrors the kernel outcome).
#[derive(Debug, Clone)]
pub enum InstallFinishedOutcome {
    Installed {
        executables: Vec<String>,
        installed_at: String,
        checksum_verified: bool,
    },
    AlreadyInstalled {
        executables: Vec<String>,
    },
    Cancelled,
}

/// Failure display kind for a failed install run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallFailureKind {
    InvalidDownloadUrl,
    ProxyInvalidUrl,
    ClientBuild,
    Network,
    HttpStatus,
    FileSystem,
    UnsafeDestination,
    UnsafeArchiveEntry,
    ArchiveUnreadable,
    ArchiveRead,
    ArchiveBomb,
    MissingArchiveEntry,
    ChecksumMismatch,
    ManifestCorrupt,
    Internal,
}

/// A failed install: display kind + the kernel's technical message (kept
/// verbatim; it is data from the failure, the headline is localized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallFailure {
    pub kind: InstallFailureKind,
    pub detail: String,
}

impl InstallFailure {
    #[must_use]
    pub fn from_error(error: &ToolInstallError) -> Self {
        let kind = match error {
            ToolInstallError::InvalidDownloadUrl => InstallFailureKind::InvalidDownloadUrl,
            ToolInstallError::InvalidProxyUrl { .. } => InstallFailureKind::ProxyInvalidUrl,
            ToolInstallError::ClientBuild { .. } => InstallFailureKind::ClientBuild,
            ToolInstallError::Http { .. } => InstallFailureKind::Network,
            ToolInstallError::HttpStatus { .. } => InstallFailureKind::HttpStatus,
            ToolInstallError::Io { .. } => InstallFailureKind::FileSystem,
            ToolInstallError::UnsafePath { .. } => InstallFailureKind::UnsafeDestination,
            ToolInstallError::UnsafeZipEntry { .. } => InstallFailureKind::UnsafeArchiveEntry,
            ToolInstallError::Zip { .. } => InstallFailureKind::ArchiveUnreadable,
            ToolInstallError::ArchiveRead { .. } => InstallFailureKind::ArchiveRead,
            ToolInstallError::OversizedZipEntry { .. } => InstallFailureKind::ArchiveBomb,
            ToolInstallError::MissingArchiveEntry { .. } => InstallFailureKind::MissingArchiveEntry,
            ToolInstallError::ChecksumMismatch { .. } => InstallFailureKind::ChecksumMismatch,
            ToolInstallError::Manifest { .. } => InstallFailureKind::ManifestCorrupt,
            _ => InstallFailureKind::Internal,
        };
        InstallFailure {
            kind,
            detail: error.to_string(),
        }
    }
}

/// One finished install run, handed from the worker thread to the UI tick.
#[derive(Debug, Clone)]
pub struct InstallFinished {
    pub tool: &'static str,
    pub purpose: ToolPurpose,
    pub outcome: Result<InstallFinishedOutcome, InstallFailure>,
}

// ---------------------------------------------------------------------------
// Install center
// ---------------------------------------------------------------------------

/// The run currently in flight for one tool (UI-facing handle: id, wording,
/// cancel flag).
#[derive(Debug, Clone)]
pub struct ActiveInstall {
    pub tool: &'static str,
    pub purpose: ToolPurpose,
    pub cancel: Arc<AtomicBool>,
}

/// Why `ToolInstallCenter::start` refused to begin a new run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// The same tool is already installing (different tools run in parallel).
    AlreadyRunning,
}

/// Background install center. Tools download in parallel: every tool owns its
/// own active handle and progress entry, so each row can show progress and be
/// cancelled independently. Worker threads write the shared queue/progress
/// map; the UI (Workbench) drains both on its 50ms tick.
pub struct ToolInstallCenter {
    queue: Arc<Mutex<Vec<InstallFinished>>>,
    progress: Arc<Mutex<HashMap<&'static str, InstallProgress>>>,
    active: HashMap<&'static str, ActiveInstall>,
}

impl std::fmt::Debug for ToolInstallCenter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolInstallCenter")
            .field("active", &self.active.len())
            .finish_non_exhaustive()
    }
}

impl Default for ToolInstallCenter {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolInstallCenter {
    #[must_use]
    pub fn new() -> Self {
        ToolInstallCenter {
            queue: Arc::new(Mutex::new(Vec::new())),
            progress: Arc::new(Mutex::new(HashMap::new())),
            active: HashMap::new(),
        }
    }

    /// The run currently in flight for `tool`, if any.
    #[must_use]
    pub fn active(&self, tool: &str) -> Option<&ActiveInstall> {
        self.active.get(tool)
    }

    /// Whether any tool is downloading. Rows stay usable in parallel; this is
    /// only a diagnostic/view helper.
    #[must_use]
    pub fn any_active(&self) -> bool {
        !self.active.is_empty()
    }

    /// Latest kernel-reported progress for one tool, if a run is in flight.
    #[must_use]
    pub fn progress(&self, tool: &str) -> Option<InstallProgress> {
        self.progress.lock().unwrap().get(tool).copied()
    }

    /// Snapshot of every in-flight tool's progress (the tick compares it to the
    /// values the last frame showed).
    #[must_use]
    pub fn progress_snapshot(&self) -> HashMap<&'static str, InstallProgress> {
        self.progress.lock().unwrap().clone()
    }

    /// Start an install of `spec`. Each tool is independent: another tool's
    /// running install never blocks this one; starting the same tool twice is
    /// refused. The kernel runs on a dedicated thread with a current-thread
    /// tokio runtime (same shape as the model-test probe); results land in the
    /// slot queue for [`ToolInstallCenter::drain`].
    pub fn start(
        &mut self,
        spec: &'static ToolSpec,
        url: String,
        install_root: PathBuf,
        proxy: duthub_tooldown::ProxyConfig,
        purpose: ToolPurpose,
    ) -> Result<(), StartError> {
        if self.active.contains_key(spec.id) {
            return Err(StartError::AlreadyRunning);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.active.insert(
            spec.id,
            ActiveInstall {
                tool: spec.id,
                purpose,
                cancel: Arc::clone(&cancel),
            },
        );
        self.progress.lock().unwrap().remove(spec.id);

        let queue = Arc::clone(&self.queue);
        let progress_map = Arc::clone(&self.progress);
        let tool = spec.id;
        let force = purpose.forced();
        // Progress sink: keep the latest kernel-reported facts per tool; the
        // tick reads them — the thread only ever writes facts.
        let progress_cb = move |event: DownloadProgress| {
            progress_map.lock().unwrap().insert(
                tool,
                InstallProgress {
                    bytes: event.bytes_downloaded,
                    total: event.total_bytes,
                },
            );
        };
        std::thread::spawn(move || {
            let outcome = guard_worker(|| {
                run_install(
                    spec,
                    &url,
                    &install_root,
                    &proxy,
                    force,
                    &cancel,
                    &progress_cb,
                )
            });
            queue.lock().unwrap().push(InstallFinished {
                tool: spec.id,
                purpose,
                outcome,
            });
        });
        Ok(())
    }

    /// Arm the cancel flag of `tool`'s run; the kernel observes it between
    /// stream chunks and reports `Cancelled` (previous install stays usable).
    /// Returns whether that tool was actually running.
    pub fn cancel_active(&mut self, tool: &str) -> bool {
        let Some(active) = self.active.get(tool) else {
            return false;
        };
        active
            .cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        true
    }

    /// Take all finished results. A tool's result clears exactly that tool's
    /// active handle and progress entry; other downloads keep running.
    pub fn drain(&mut self) -> Vec<InstallFinished> {
        let drained: Vec<InstallFinished> = std::mem::take(&mut *self.queue.lock().unwrap());
        for finished in &drained {
            self.active.remove(finished.tool);
            self.progress.lock().unwrap().remove(finished.tool);
        }
        drained
    }
}

/// Run the worker body, converting a panic into an `Internal` failure so the
/// result queue always receives a terminal entry. A panicking worker must not
/// leave the center's `active` handle set forever — that would pin the UI on
/// "downloading" with a cancel that cannot take effect and `start` refusing
/// every later run.
fn guard_worker(
    body: impl FnOnce() -> Result<InstallFinishedOutcome, InstallFailure>,
) -> Result<InstallFinishedOutcome, InstallFailure> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_string());
        Err(InstallFailure {
            kind: InstallFailureKind::Internal,
            detail: format!("install worker panicked: {message}"),
        })
    })
}

/// Run the kernel install on the worker thread; kernel errors are mapped to
/// display failures here so the UI never sees kernel types.
fn run_install(
    spec: &'static ToolSpec,
    url: &str,
    install_root: &Path,
    proxy: &duthub_tooldown::ProxyConfig,
    force: bool,
    cancel: &AtomicBool,
    progress_cb: &(dyn Fn(DownloadProgress) + Send + Sync),
) -> Result<InstallFinishedOutcome, InstallFailure> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            return Err(InstallFailure {
                kind: InstallFailureKind::Internal,
                detail: format!("async runtime: {error}"),
            });
        }
    };
    let opts = duthub_tooldown::InstallOptions {
        install_root,
        force,
        cancel: Some(cancel),
        proxy,
        progress: Some(progress_cb),
    };
    match runtime.block_on(duthub_tooldown::install_tool_from_url(spec, url, &opts)) {
        Ok(InstallOutcome::Installed(manifest)) => Ok(InstallFinishedOutcome::Installed {
            executables: manifest.executables,
            installed_at: manifest.installed_at,
            checksum_verified: manifest.checksum_verified,
        }),
        Ok(InstallOutcome::AlreadyInstalled(manifest)) => {
            Ok(InstallFinishedOutcome::AlreadyInstalled {
                executables: manifest.executables,
            })
        }
        Ok(InstallOutcome::Cancelled) => Ok(InstallFinishedOutcome::Cancelled),
        Err(error) => Err(InstallFailure::from_error(&error)),
    }
}

// ---------------------------------------------------------------------------
// Staging leftovers sweep
// ---------------------------------------------------------------------------

/// Remove `.tooldown-*` staging directories under `tools_root` left behind by
/// a crashed/killed download run (the kernel's `TempDir` cleanup is best-effort
/// while a file is locked by a running process on Windows). Best-effort:
/// per-directory failures are skipped and retried on the next sweep. Returns
/// how many leftovers were removed.
#[must_use]
pub fn sweep_staging_leftovers(tools_root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(tools_root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_staging_dir = name.to_string_lossy().starts_with(".tooldown-")
            && entry.file_type().is_ok_and(|t| t.is_dir());
        if is_staging_dir && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- progress math ------------------------------------------------------

    #[test]
    fn percent_needs_a_real_total() {
        assert_eq!(progress_percent(0, Some(200)), Some(0));
        assert_eq!(progress_percent(1, Some(200)), Some(0));
        assert_eq!(progress_percent(100, Some(200)), Some(50));
        assert_eq!(progress_percent(199, Some(200)), Some(99));
        assert_eq!(progress_percent(200, Some(200)), Some(100));
        // Over-reporting bytes clamps instead of overflowing the bar.
        assert_eq!(progress_percent(400, Some(200)), Some(100));
        // Unknown total and zero-length payload must not invent a percentage.
        assert_eq!(progress_percent(500, None), None);
        assert_eq!(progress_percent(0, Some(0)), None);
    }

    #[test]
    fn bytes_format_in_decimal_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1000), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1000 * 1000), "5.0 MB");
        assert_eq!(format_bytes(54_449_330), "54.4 MB");
        assert_eq!(format_bytes(1_000_000_000), "1.0 GB");
        // Beyond the unit table: stays on the largest unit.
        assert_eq!(format_bytes(2_000_000_000_000_000), "2000.0 TB");
    }

    // -- proxy URL advisory check -------------------------------------------

    #[test]
    fn proxy_url_accepts_supported_schemes_with_host() {
        for url in [
            "http://127.0.0.1:8080",
            "https://proxy.example:8443",
            "socks5://127.0.0.1:1080",
            "HTTP://Proxy.Example",
            "socks5://[::1]:1080",
            "http://user:pass@127.0.0.1:8080",
            "  https://proxy.example  ",
        ] {
            assert_eq!(
                validate_custom_proxy_url(url),
                Ok(()),
                "{url:?} should pass the advisory check"
            );
        }
    }

    #[test]
    fn proxy_url_rejects_obvious_mistakes() {
        assert_eq!(
            validate_custom_proxy_url(""),
            Err(ProxyUrlProblem::NoScheme)
        );
        assert_eq!(
            validate_custom_proxy_url("127.0.0.1:8080"),
            Err(ProxyUrlProblem::NoScheme)
        );
        assert_eq!(
            validate_custom_proxy_url("ftp://127.0.0.1"),
            Err(ProxyUrlProblem::UnsupportedScheme("ftp".into()))
        );
        assert_eq!(
            validate_custom_proxy_url("http://"),
            Err(ProxyUrlProblem::MissingHost)
        );
        assert_eq!(
            validate_custom_proxy_url("socks5://   "),
            Err(ProxyUrlProblem::MissingHost)
        );
        // Unbalanced bracket (kernel build would reject it too).
        assert_eq!(
            validate_custom_proxy_url("http://[bad"),
            Err(ProxyUrlProblem::MissingHost)
        );
    }

    // -- install center bookkeeping ----------------------------------------

    #[test]
    fn install_center_runs_tools_independently() {
        let mut center = ToolInstallCenter::new();
        let adb_cancel = Arc::new(AtomicBool::new(false));
        let ffmpeg_cancel = Arc::new(AtomicBool::new(false));
        center.active.insert(
            "adb",
            ActiveInstall {
                tool: "adb",
                purpose: ToolPurpose::Download,
                cancel: Arc::clone(&adb_cancel),
            },
        );
        center.active.insert(
            "ffmpeg",
            ActiveInstall {
                tool: "ffmpeg",
                purpose: ToolPurpose::Update,
                cancel: Arc::clone(&ffmpeg_cancel),
            },
        );
        center.progress.lock().unwrap().insert(
            "ffmpeg",
            InstallProgress {
                bytes: 5,
                total: Some(10),
            },
        );

        assert!(center.active("adb").is_some());
        assert!(center.active("ffmpeg").is_some());
        assert!(center.any_active());
        assert_eq!(
            center.progress("ffmpeg"),
            Some(InstallProgress {
                bytes: 5,
                total: Some(10)
            })
        );
        assert_eq!(center.progress("adb"), None);
        assert_eq!(center.progress_snapshot().len(), 1);

        // Cancelling one tool only arms that tool's flag.
        assert!(center.cancel_active("adb"));
        assert!(adb_cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!ffmpeg_cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!center.cancel_active("goose"));

        // One finished result clears exactly that tool's slots.
        center.queue.lock().unwrap().push(InstallFinished {
            tool: "adb",
            purpose: ToolPurpose::Download,
            outcome: Ok(InstallFinishedOutcome::Cancelled),
        });
        let drained = center.drain();
        assert_eq!(drained.len(), 1);
        assert!(center.active("adb").is_none());
        assert!(center.progress("adb").is_none());
        assert!(center.active("ffmpeg").is_some());
        assert!(center.progress("ffmpeg").is_some());
    }

    // -- worker panic guard -------------------------------------------------

    #[test]
    fn panicking_worker_becomes_internal_failure() {
        let outcome = guard_worker(|| panic!("boom in worker"));
        let failure = outcome.expect_err("a panic must surface as a failure");
        assert_eq!(failure.kind, InstallFailureKind::Internal);
        assert!(
            failure.detail.contains("panicked"),
            "detail should name the panic: {}",
            failure.detail
        );
    }

    #[test]
    fn worker_result_passes_through_unchanged() {
        let outcome = guard_worker(|| Ok(InstallFinishedOutcome::Cancelled));
        assert!(matches!(outcome, Ok(InstallFinishedOutcome::Cancelled)));
        let outcome = guard_worker(|| {
            Err(InstallFailure {
                kind: InstallFailureKind::Network,
                detail: "offline".to_string(),
            })
        });
        let failure = outcome.expect_err("errors pass through");
        assert_eq!(failure.kind, InstallFailureKind::Network);
    }

    // -- effective path classification ---------------------------------------

    /// Fake a downloaded install under `root` (manifest + placeholder exe),
    /// mirroring the on-disk camelCase contract.
    fn fake_install(root: &Path, tool_id: &str, exe_name: &str) -> PathBuf {
        let dir = root.join(tool_id);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(exe_name);
        std::fs::write(&exe, b"MZ placeholder").unwrap();
        let manifest = serde_json::json!({
            "tool": tool_id,
            "url": format!("https://example.invalid/{tool_id}.zip"),
            "sha256": "0".repeat(64),
            "pinned": true,
            "checksumVerified": true,
            "installedAt": "2026-11-23T00:00:00Z",
            "executables": [exe.display().to_string()],
        });
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        exe
    }

    #[test]
    fn effective_path_follows_resolver_priority() {
        let root = std::env::temp_dir().join(format!("duthub-tool-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let exe = fake_install(&root, "adb", "adb.exe");

        // Downloaded install shadows the canonical bare name (and the default).
        assert_eq!(
            effective_tool_path(Some("adb"), "adb", Some(&root)),
            EffectiveToolPath::Downloaded(exe.clone())
        );
        assert_eq!(
            effective_tool_path(Some(""), "adb", Some(&root)),
            EffectiveToolPath::Downloaded(exe.clone())
        );
        // Explicit path / other command / env-var spec are never hijacked.
        assert_eq!(
            effective_tool_path(Some(r"C:\other\adb.exe"), "adb", Some(&root)),
            EffectiveToolPath::Explicit(r"C:\other\adb.exe".to_string())
        );
        assert_eq!(
            effective_tool_path(Some("my-adb"), "adb", Some(&root)),
            EffectiveToolPath::Explicit("my-adb".to_string())
        );
        assert_eq!(
            effective_tool_path(Some("%ADB_HOME%/adb.exe"), "adb", Some(&root)),
            EffectiveToolPath::Explicit("%ADB_HOME%/adb.exe".to_string())
        );
        // No download → historical bare-name PATH fallback.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            effective_tool_path(Some("adb"), "adb", Some(&empty)),
            EffectiveToolPath::PathFallback("adb".to_string())
        );
        // Tools without a settings key: downloaded when present, otherwise
        // "managed elsewhere" (the Agent registry resolves goose).
        assert_eq!(
            effective_tool_path(None, "goose", Some(&root)),
            EffectiveToolPath::ManagedElsewhere
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn row_snapshot_reports_install_facts() {
        let root = std::env::temp_dir().join(format!("duthub-tool-ui-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let spec = duthub_tooldown::spec_by_id("adb").unwrap();

        let missing = tool_row_snapshot(spec, Some("adb"), Some(&root));
        assert!(!missing.installed);
        assert_eq!(missing.install_path, None);
        assert_eq!(missing.installed_at, None);

        let exe = fake_install(&root, "adb", "adb.exe");
        let snap = tool_row_snapshot(spec, Some("adb"), Some(&root));
        assert!(snap.installed);
        assert_eq!(snap.install_path, Some(exe));
        assert_eq!(snap.installed_at.as_deref(), Some("2026-11-23T00:00:00Z"));
        assert_eq!(snap.checksum_verified, Some(true));
        assert_eq!(snap.pinned, Some(true));

        // A manifest of another tool must not be mistaken for this row's facts:
        // goose has a foreign manifest and no executable → not installed.
        let goose_dir = root.join("goose");
        std::fs::create_dir_all(&goose_dir).unwrap();
        std::fs::write(
            goose_dir.join("manifest.json"),
            format!(r#"{{"tool":"other-tool","url":"https://x","sha256":"{}","pinned":false,"checksumVerified":false,"installedAt":"t","executables":[]}}"#, "0".repeat(64)),
        )
        .unwrap();
        let goose_spec = duthub_tooldown::spec_by_id("goose").unwrap();
        let goose_snap = tool_row_snapshot(goose_spec, None, Some(&root));
        assert!(!goose_snap.installed);
        let _ = std::fs::remove_dir_all(&root);
    }
}
