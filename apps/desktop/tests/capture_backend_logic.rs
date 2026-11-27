//! Lane B regressions for `duthub_desktop::capture_backend`
//! (`docs/design/CAPTURE_STREAM_BACKENDS.md` §3.4/§4.2/§5.4/§5.7): idle clamping,
//! ffmpeg resolution precedence, PNG/byte artifact facts, the `MEDIA_*` code
//! table and the demand ledger. All time inputs are injected — no sleeping.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use duthub_core::model::CaptureSettings;
use duthub_desktop::capture_backend::{
    DEFAULT_EPHEMERAL_IDLE_SECS, DemandEntry, DemandKind, HOLD_SECS, IDLE_MAX_SECS, IDLE_MIN_SECS,
    MEDIA_BUSY, MEDIA_CAPTURE_ENDED, MEDIA_CAPTURE_FAILED, MEDIA_CAPTURE_UNAVAILABLE,
    MEDIA_ERROR_CODES, MEDIA_NOT_BOUND, MEDIA_OP_DROPPED, MEDIA_OP_TIMEOUT, MEDIA_RECORDING_ENDED,
    MEDIA_RECORDING_IN_PROGRESS, MEDIA_RECORDING_UNKNOWN, MEDIA_START_FAILED, ReclaimDecision,
    artifact_bytes, effective_idle, png_dimensions, resolved_capture_settings,
    resolved_capture_settings_in, validate_idle_secs,
};

const IDLE: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// tools.capture: idle clamp and validation
// ---------------------------------------------------------------------------

fn capture_with_idle(idle_secs: Option<u32>) -> CaptureSettings {
    CaptureSettings {
        idle_secs,
        ..Default::default()
    }
}

#[test]
fn effective_idle_falls_back_to_the_default_for_none_and_out_of_range() {
    let idle = |value| effective_idle(&capture_with_idle(value));
    assert_eq!(
        idle(None),
        Duration::from_secs(u64::from(DEFAULT_EPHEMERAL_IDLE_SECS))
    );
    assert_eq!(idle(Some(0)), Duration::from_secs(60));
    assert_eq!(idle(Some(IDLE_MIN_SECS)), Duration::from_secs(1));
    assert_eq!(idle(Some(90)), Duration::from_secs(90));
    assert_eq!(idle(Some(IDLE_MAX_SECS)), Duration::from_secs(3600));
    assert_eq!(idle(Some(IDLE_MAX_SECS + 1)), Duration::from_secs(60));
    assert_eq!(idle(Some(9999)), Duration::from_secs(60));
    assert_eq!(idle(Some(u32::MAX)), Duration::from_secs(60));
}

#[test]
fn validate_idle_secs_accepts_only_the_documented_range() {
    assert_eq!(validate_idle_secs(0), None);
    assert_eq!(validate_idle_secs(1), Some(1));
    assert_eq!(validate_idle_secs(3600), Some(3600));
    assert_eq!(validate_idle_secs(3601), None);
    assert_eq!(validate_idle_secs(u32::MAX), None);
}

// ---------------------------------------------------------------------------
// ffmpeg resolution: reuse tool_resolver's precedence
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-capture-backend-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Fake a downloaded install: `manifest.json` plus one placeholder executable,
/// using the exact on-disk camelCase keys (same shape as `tool_resolver` tests).
fn fake_install(root: &Path, tool_id: &str, exe_name: &str) -> PathBuf {
    let dir = root.join(tool_id);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join(exe_name);
    std::fs::write(&exe, b"MZ placeholder").unwrap();
    let manifest = serde_json::json!({
        "tool": tool_id,
        "url": "https://example.invalid/tool.zip",
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

fn cleanup(dir: PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn explicit_ffmpeg_path_wins_and_resolution_is_idempotent() {
    let root = temp_dir("explicit");
    fake_install(&root, "ffmpeg", "ffmpeg.exe");
    let base = CaptureSettings {
        ffmpeg: r"C:\tools\ffmpeg.exe".into(),
        ffplay: r"C:\tools\ffplay.exe".into(),
        listen_ip: "127.0.0.1".into(),
        idle_secs: Some(90),
    };
    let once = resolved_capture_settings_in(&base, Some(&root));
    assert_eq!(once.ffmpeg, base.ffmpeg);
    // ffplay / listenIp / idleSecs are carried through untouched.
    assert_eq!(once.ffplay, base.ffplay);
    assert_eq!(once.listen_ip, base.listen_ip);
    assert_eq!(once.idle_secs, Some(90));
    // Feeding the resolved value back in changes nothing.
    let twice = resolved_capture_settings_in(&once, Some(&root));
    assert_eq!(twice, once);
    cleanup(root);
}

#[test]
fn env_var_ffmpeg_spec_is_left_for_media_rs_to_expand() {
    let root = temp_dir("envvar");
    fake_install(&root, "ffmpeg", "ffmpeg.exe");
    let base = CaptureSettings {
        ffmpeg: r"%FFMPEG_HOME%\bin\ffmpeg.exe".into(),
        ..Default::default()
    };
    let resolved = resolved_capture_settings_in(&base, Some(&root));
    assert_eq!(resolved.ffmpeg, base.ffmpeg);
    cleanup(root);
}

#[test]
fn empty_configured_ffmpeg_without_a_download_falls_back_to_the_bare_name() {
    let root = temp_dir("empty");
    let base = CaptureSettings {
        ffmpeg: String::new(),
        ..Default::default()
    };
    let resolved = resolved_capture_settings_in(&base, Some(&root));
    assert_eq!(resolved.ffmpeg, "ffmpeg");
    cleanup(root);
}

#[test]
fn downloaded_ffmpeg_shadows_only_the_canonical_bare_name() {
    let root = temp_dir("shadow");
    let exe = fake_install(&root, "ffmpeg", "ffmpeg.exe");
    let expected = exe.display().to_string();
    for configured in ["", "ffmpeg", "FFMPEG", "  ffmpeg  "] {
        let base = CaptureSettings {
            ffmpeg: configured.into(),
            ..Default::default()
        };
        assert_eq!(
            resolved_capture_settings_in(&base, Some(&root)).ffmpeg,
            expected,
            "configured = {configured:?}"
        );
    }
    for configured in [
        "my-ffmpeg",
        "ffprobe",
        r"C:\bin\ffmpeg.exe",
        r"%FFMPEG_HOME%\ffmpeg.exe",
    ] {
        let base = CaptureSettings {
            ffmpeg: configured.into(),
            ..Default::default()
        };
        assert_eq!(
            resolved_capture_settings_in(&base, Some(&root)).ffmpeg,
            configured.trim(),
            "configured = {configured:?}"
        );
    }
    cleanup(root);
}

#[test]
fn resolution_without_a_tools_root_keeps_explicit_specs() {
    let base = CaptureSettings {
        ffmpeg: r"C:\tools\ffmpeg.exe".into(),
        ffplay: "ffplay".into(),
        listen_ip: "10.0.0.2".into(),
        idle_secs: Some(5),
    };
    let resolved = resolved_capture_settings_in(&base, None);
    assert_eq!(resolved, base);
}

#[test]
fn config_anchored_resolution_also_keeps_explicit_specs() {
    // Exercises the wrapper; the download lookup is the process config dir,
    // but an explicit path can never be shadowed by it.
    let base = CaptureSettings {
        ffmpeg: r"D:\lab\bin\ffmpeg.exe".into(),
        ffplay: "ffplay".into(),
        listen_ip: "127.0.0.1".into(),
        idle_secs: None,
    };
    assert_eq!(resolved_capture_settings(&base), base);
}

// ---------------------------------------------------------------------------
// Artifact facts
// ---------------------------------------------------------------------------

/// Minimal PNG header: signature + IHDR length/type + dimensions + trailing
/// IHDR fields. No IDAT/CRC bytes are needed for header parsing.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes
}

#[test]
fn png_dimensions_reads_the_ihdr_header() {
    assert_eq!(png_dimensions(&png_header(1024, 600)), Some((1024, 600)));
    assert_eq!(png_dimensions(&png_header(1, 1)), Some((1, 1)));
}

#[test]
fn png_dimensions_rejects_truncated_non_png_and_zero_sized_headers() {
    let header = png_header(640, 360);
    assert_eq!(png_dimensions(&header[..23]), None);
    assert_eq!(png_dimensions(&[]), None);

    let mut bad_signature = header.clone();
    bad_signature[0] = 0;
    assert_eq!(png_dimensions(&bad_signature), None);

    let mut bad_first_chunk = header;
    bad_first_chunk[12..16].copy_from_slice(b"IDAT");
    assert_eq!(png_dimensions(&bad_first_chunk), None);

    assert_eq!(png_dimensions(&png_header(0, 360)), None);
    assert_eq!(png_dimensions(&png_header(640, 0)), None);
}

#[test]
fn artifact_bytes_reports_regular_files_only() {
    let dir = temp_dir("bytes");
    let file = dir.join("frame.png");
    std::fs::write(&file, b"hello").unwrap();
    assert_eq!(artifact_bytes(&file), Some(5));
    assert_eq!(artifact_bytes(&dir.join("missing.png")), None);
    assert_eq!(artifact_bytes(&dir), None);
    cleanup(dir);
}

// ---------------------------------------------------------------------------
// Error-code vocabulary
// ---------------------------------------------------------------------------

#[test]
fn media_error_codes_match_the_contract_and_are_unique() {
    let expected = [
        MEDIA_NOT_BOUND,
        MEDIA_CAPTURE_UNAVAILABLE,
        MEDIA_START_FAILED,
        MEDIA_CAPTURE_FAILED,
        MEDIA_CAPTURE_ENDED,
        MEDIA_RECORDING_ENDED,
        MEDIA_RECORDING_UNKNOWN,
        MEDIA_BUSY,
        MEDIA_RECORDING_IN_PROGRESS,
        MEDIA_OP_DROPPED,
        MEDIA_OP_TIMEOUT,
    ];
    assert_eq!(MEDIA_ERROR_CODES, expected);
    let unique: BTreeSet<&str> = MEDIA_ERROR_CODES.into_iter().collect();
    assert_eq!(unique.len(), MEDIA_ERROR_CODES.len());
    for code in MEDIA_ERROR_CODES {
        assert!(code.starts_with("MEDIA_"), "{code}");
    }
    // Deliberate absences from §5.7.
    assert!(!MEDIA_ERROR_CODES.contains(&"MEDIA_QUOTA"));
    assert!(!MEDIA_ERROR_CODES.contains(&"MEDIA_BACKEND_UNAVAILABLE"));
    assert!(!MEDIA_ERROR_CODES.contains(&"INVALID_CLIP_SECONDS"));
}

// ---------------------------------------------------------------------------
// Demand ledger
// ---------------------------------------------------------------------------

#[test]
fn demand_kind_pin_and_hold_classification_is_exact() {
    let kinds = [
        (DemandKind::Manual, true, false),
        (DemandKind::Monitor, false, false),
        (DemandKind::ExternalPreview, false, true),
        (DemandKind::CopyUrl, false, true),
        (DemandKind::AgentOp, false, false),
    ];
    for (kind, pin, hold) in kinds {
        assert_eq!(kind.is_pin(), pin, "{kind:?}");
        assert_eq!(kind.is_hold(), hold, "{kind:?}");
    }
}

#[test]
fn empty_entry_is_not_leased() {
    let entry = DemandEntry::default();
    assert!(entry.is_empty());
    assert!(!entry.has_pin());
    assert_eq!(
        entry.decide(Instant::now(), IDLE),
        ReclaimDecision::NotLeased
    );
}

#[test]
fn manual_lease_pins_forever() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(true);
    entry.acquire(1, DemandKind::Manual, t0);
    assert!(entry.has_pin());
    assert!(!entry.is_empty());
    assert_eq!(
        entry.decide(t0 + Duration::from_hours(24), IDLE),
        ReclaimDecision::Pinned
    );
    // Releasing the pin falls back to the ephemeral idle path.
    entry.release(1);
    assert!(!entry.has_pin());
    assert_eq!(entry.decide(t0, IDLE), ReclaimDecision::Keep);
}

#[test]
fn interactive_last_release_stops_immediately() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(false);
    entry.acquire(1, DemandKind::Monitor, t0);
    assert_eq!(entry.decide(t0, IDLE), ReclaimDecision::Keep);

    entry.release(1);
    assert_eq!(entry.decide(t0, IDLE), ReclaimDecision::StopNow);
    // Unknown / repeated releases are idempotent.
    entry.release(1);
    entry.release(99);
    assert!(entry.is_empty());
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(3600), IDLE),
        ReclaimDecision::StopNow
    );
}

#[test]
fn ephemeral_release_starts_idle_clock_then_reclaims() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(true);
    entry.acquire(1, DemandKind::AgentOp, t0);
    assert_eq!(entry.decide(t0, IDLE), ReclaimDecision::Keep);

    entry.release(1);
    // Release carries no clock; the next sweep stamps the idle start.
    assert_eq!(entry.decide(t0 + IDLE, IDLE), ReclaimDecision::Keep);
    entry.sweep_expired(t0);
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(59), IDLE),
        ReclaimDecision::Keep
    );
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(60), IDLE),
        ReclaimDecision::Reclaim
    );
}

#[test]
fn unused_ephemeral_entry_idles_out_after_the_first_sweep() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(true);
    assert_eq!(entry.decide(t0, IDLE), ReclaimDecision::Keep);
    entry.sweep_expired(t0);
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(59), IDLE),
        ReclaimDecision::Keep
    );
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(60), IDLE),
        ReclaimDecision::Reclaim
    );
}

#[test]
fn recording_mirror_pins_without_a_lease_and_restarts_idle_after_stop() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(true);
    entry.sweep_expired(t0);
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(60), IDLE),
        ReclaimDecision::Reclaim
    );

    entry.recording = true;
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(600), IDLE),
        ReclaimDecision::Pinned
    );
    // A running recording keeps the reclaim clock from advancing.
    entry.sweep_expired(t0 + Duration::from_secs(600));
    entry.recording = false;
    // The idle window restarts only after the recording is finished.
    assert_eq!(
        entry.decide(t0 + Duration::from_mins(11), IDLE),
        ReclaimDecision::Keep
    );
    entry.sweep_expired(t0 + Duration::from_mins(11));
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(719), IDLE),
        ReclaimDecision::Keep
    );
    assert_eq!(
        entry.decide(t0 + Duration::from_mins(12), IDLE),
        ReclaimDecision::Reclaim
    );
}

#[test]
fn agent_recording_mirror_pins_until_cleared() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(true);
    entry.agent_recording = Some(42);
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(3600), IDLE),
        ReclaimDecision::Pinned
    );
    // The idle window must not run behind the Agent recording.
    entry.sweep_expired(t0 + Duration::from_secs(3600));
    entry.agent_recording = None;
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(3600), IDLE),
        ReclaimDecision::Keep
    );
    entry.sweep_expired(t0 + Duration::from_secs(3600));
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(3659), IDLE),
        ReclaimDecision::Keep
    );
    assert_eq!(
        entry.decide(t0 + Duration::from_mins(61), IDLE),
        ReclaimDecision::Reclaim
    );
}

#[test]
fn copy_url_hold_expires_without_release() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(false);
    entry.acquire_hold(7, DemandKind::CopyUrl, t0, HOLD_SECS);
    assert!(!entry.is_empty());
    assert_eq!(
        entry.decide((t0 + HOLD_SECS).checked_sub(Duration::from_millis(1)).unwrap(), IDLE),
        ReclaimDecision::Keep
    );
    // Expiry is visible to `decide` even before the sweep runs.
    assert_eq!(
        entry.decide(t0 + HOLD_SECS, IDLE),
        ReclaimDecision::NotLeased
    );
    // The sweep removes it; the interactive entry now owes an immediate stop.
    entry.sweep_expired(t0 + HOLD_SECS);
    assert!(entry.is_empty());
    assert_eq!(entry.decide(t0 + HOLD_SECS, IDLE), ReclaimDecision::StopNow);
}

#[test]
fn external_preview_hold_refresh_rearms_the_deadline() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(false);
    entry.acquire_hold(9, DemandKind::ExternalPreview, t0, HOLD_SECS);
    // "Copy/refresh again" re-arms the same window from the new instant.
    entry.acquire_hold(
        9,
        DemandKind::ExternalPreview,
        t0 + Duration::from_secs(100),
        HOLD_SECS,
    );
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(399), IDLE),
        ReclaimDecision::Keep
    );
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(400), IDLE),
        ReclaimDecision::NotLeased
    );
    entry.sweep_expired(t0 + Duration::from_secs(400));
    assert!(entry.is_empty());
}

#[test]
fn sweep_keeps_holds_that_are_not_due() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(false);
    entry.acquire_hold(6, DemandKind::ExternalPreview, t0, HOLD_SECS);
    entry.sweep_expired(t0 + Duration::from_secs(299));
    assert!(!entry.is_empty());
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(299), IDLE),
        ReclaimDecision::Keep
    );
}

#[test]
fn expired_hold_does_not_release_a_manual_pin() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(false);
    entry.acquire(1, DemandKind::Manual, t0);
    entry.acquire_hold(2, DemandKind::ExternalPreview, t0, Duration::from_secs(10));
    entry.sweep_expired(t0 + Duration::from_secs(10));
    assert!(!entry.is_empty());
    assert!(entry.has_pin());
    assert_eq!(
        entry.decide(t0 + Duration::from_hours(24), IDLE),
        ReclaimDecision::Pinned
    );
}

#[test]
fn expired_hold_starts_the_ephemeral_idle_clock() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(true);
    entry.acquire_hold(5, DemandKind::CopyUrl, t0, Duration::from_secs(10));
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(10), IDLE),
        ReclaimDecision::Keep
    );
    entry.sweep_expired(t0 + Duration::from_secs(10));
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(69), IDLE),
        ReclaimDecision::Keep
    );
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(70), IDLE),
        ReclaimDecision::Reclaim
    );
}

#[test]
fn repeated_acquire_and_release_are_idempotent() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(false);
    entry.acquire(3, DemandKind::Monitor, t0);
    entry.acquire(3, DemandKind::Monitor, t0);
    assert!(!entry.is_empty());
    entry.release(3);
    entry.release(3);
    entry.release(1234);
    assert!(entry.is_empty());
    assert_eq!(entry.decide(t0, IDLE), ReclaimDecision::StopNow);
}

#[test]
fn reacquiring_clears_the_idle_clock_and_replaces_the_kind() {
    let t0 = Instant::now();
    let mut entry = DemandEntry::new(true);
    entry.sweep_expired(t0);
    assert_eq!(entry.decide(t0 + IDLE, IDLE), ReclaimDecision::Reclaim);

    // Acquire at the reclaim instant: the idle clock is cleared.
    entry.acquire(2, DemandKind::Monitor, t0 + IDLE);
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(120), IDLE),
        ReclaimDecision::Keep
    );

    // Re-acquiring the same id as Manual replaces the non-pin kind.
    entry.acquire(2, DemandKind::Manual, t0 + Duration::from_secs(120));
    assert!(entry.has_pin());
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(180), IDLE),
        ReclaimDecision::Pinned
    );
    entry.release(2);
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(180), IDLE),
        ReclaimDecision::Keep
    );
    entry.sweep_expired(t0 + Duration::from_secs(180));
    assert_eq!(
        entry.decide(t0 + Duration::from_secs(240), IDLE),
        ReclaimDecision::Reclaim
    );
}
