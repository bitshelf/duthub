//! Tool-download page regressions: install-center lifecycle basics, staging
//! sweep, kernel-error mapping and the progress display math — all through the
//! lib target (`duthub_desktop::tool_ui`), no GPUI.
//!
//! The kernel download pipeline itself is covered by `duthub-tooldown`'s tests
//! (including a loopback proxy behavior integration test); there is no network
//! access here, so `start()` is exercised only for its guard rails.

use std::path::PathBuf;

use duthub_desktop::tool_ui::{
    InstallFailure, InstallFailureKind, InstallFinishedOutcome, ToolInstallCenter, ToolPurpose,
    format_bytes, progress_percent, sweep_staging_leftovers, validate_custom_proxy_url,
};
use duthub_tooldown::{ProxyConfig, ToolInstallError};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-tool-ui-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Progress display math: real percentages only
// ---------------------------------------------------------------------------

#[test]
fn progress_percent_never_invents_a_total() {
    // Unknown total (chunked response) → no percentage, period.
    assert_eq!(progress_percent(12345, None), None);
    assert_eq!(progress_percent(0, Some(0)), None);
    // Known total: floor percent, clamped at 100.
    assert_eq!(progress_percent(1, Some(4)), Some(25));
    assert_eq!(progress_percent(3, Some(4)), Some(75));
    assert_eq!(progress_percent(4, Some(4)), Some(100));
    assert_eq!(progress_percent(8, Some(4)), Some(100));
}

#[test]
fn bytes_format_is_stable() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(54_449_330), "54.4 MB");
}

// ---------------------------------------------------------------------------
// Center guard rails (no network involved)
// ---------------------------------------------------------------------------

#[test]
fn idle_center_reports_nothing_and_cancel_is_noop() {
    let mut center = ToolInstallCenter::new();
    assert!(center.active("adb").is_none());
    assert!(!center.any_active());
    assert_eq!(center.progress("adb"), None);
    assert!(center.progress_snapshot().is_empty());
    assert!(
        !center.cancel_active("adb"),
        "cancel on idle center is a no-op"
    );
    assert!(center.drain().is_empty());
}

#[test]
fn purposes_map_to_force_correctly() {
    assert!(!ToolPurpose::Download.forced());
    assert!(ToolPurpose::Redownload.forced());
    assert!(ToolPurpose::Update.forced());
}

// ---------------------------------------------------------------------------
// Kernel error → display failure mapping
// ---------------------------------------------------------------------------

#[test]
fn kernel_errors_map_to_display_kinds_with_verbatim_detail() {
    let mismatch = ToolInstallError::ChecksumMismatch {
        expected: "aa".into(),
        actual: "bb".into(),
    };
    let failure = InstallFailure::from_error(&mismatch);
    assert_eq!(failure.kind, InstallFailureKind::ChecksumMismatch);
    assert!(
        failure.detail.contains("aa") && failure.detail.contains("bb"),
        "technical detail must carry the pinned/measured digests: {failure:?}"
    );

    // Rebuild the kernel error shape with the inner reqwest error extracted
    // from a real client-build failure (the URL parser rejects "http://[bad";
    // odd *schemes* like nope://x only fail at connect time, per kernel tests).
    let source = match ProxyConfig::Custom("http://[bad".into()).build_client() {
        Err(ToolInstallError::InvalidProxyUrl { source, .. }) => source,
        other => panic!("expected InvalidProxyUrl from kernel, got {other:?}"),
    };
    let invalid = ToolInstallError::InvalidProxyUrl {
        url: "http://[bad".into(),
        source,
    };
    let failure = InstallFailure::from_error(&invalid);
    assert_eq!(failure.kind, InstallFailureKind::ProxyInvalidUrl);
    assert!(!failure.detail.is_empty());
}

// ---------------------------------------------------------------------------
// Advisory proxy URL pre-check (kernel build stays authoritative)
// ---------------------------------------------------------------------------

#[test]
fn proxy_precheck_passes_kernel_plausible_urls() {
    for url in [
        "http://127.0.0.1:8080",
        "https://proxy.corp:8443",
        "socks5://127.0.0.1:1080",
    ] {
        assert_eq!(validate_custom_proxy_url(url), Ok(()));
        // Everything the pre-check accepts must also build a client (or, for
        // scheme-only oddities, fail at connect time — build-level acceptance
        // is the kernel's domain and stays untouched).
        ProxyConfig::Custom(url.to_string())
            .build_client()
            .unwrap_or_else(|err| panic!("kernel rejected {url:?}: {err}"));
    }
    // And the pre-check rejects the forms the kernel cannot parse at all,
    // before they ever reach a download attempt.
    assert!(validate_custom_proxy_url("not a url").is_err());
    assert!(validate_custom_proxy_url("http://").is_err());
}

// ---------------------------------------------------------------------------
// Staging leftovers sweep (crashed runs)
// ---------------------------------------------------------------------------

#[test]
fn sweep_removes_only_tooldown_staging_dirs() {
    let root = temp_dir("sweep");
    // Keep: a real tool install and an unrelated directory.
    std::fs::create_dir_all(root.join("adb")).unwrap();
    std::fs::write(root.join("adb").join("adb.exe"), b"MZ").unwrap();
    std::fs::create_dir_all(root.join("notes")).unwrap();
    std::fs::write(root.join("notes").join("readme.txt"), b"x").unwrap();
    // A staging leftover (dir) and a staging-named file (must survive: not a dir).
    let staged = root.join(".tooldown-abc123");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("tool.zip"), b"partial").unwrap();
    std::fs::write(root.join(".tooldown-not-a-dir"), b"file").unwrap();

    let removed = sweep_staging_leftovers(&root);
    assert_eq!(removed, 1, "exactly the staging dir is removed");
    assert!(root.join("adb").join("adb.exe").is_file());
    assert!(root.join("notes").join("readme.txt").is_file());
    assert!(!staged.exists());
    assert!(root.join(".tooldown-not-a-dir").is_file());

    // Idempotent: a second sweep finds nothing.
    assert_eq!(sweep_staging_leftovers(&root), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sweep_tolerates_missing_root() {
    let missing = temp_dir("missing");
    let _ = std::fs::remove_dir_all(&missing);
    assert_eq!(sweep_staging_leftovers(&missing), 0);
}

// ---------------------------------------------------------------------------
// Outcome display data (drain-side shape)
// ---------------------------------------------------------------------------

#[test]
fn finished_outcomes_carry_install_facts() {
    let outcome = InstallFinishedOutcome::Installed {
        executables: vec!["C:\\lab\\.duthub\\tools\\adb\\adb.exe".into()],
        installed_at: "2026-11-23T12:00:00Z".into(),
        checksum_verified: false,
    };
    let InstallFinishedOutcome::Installed {
        executables,
        installed_at,
        checksum_verified,
    } = outcome
    else {
        panic!("shape mismatch");
    };
    assert_eq!(executables.len(), 1);
    assert_eq!(installed_at, "2026-11-23T12:00:00Z");
    // Rolling assets stay unverified — the UI must render this honestly.
    assert!(!checksum_verified);
}
