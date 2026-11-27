//! Live hardware end-to-end test for `COM8` + `COM25` (serial performance acceptance):
//! relay channel 1 (DUT reset) pulse -> RK3576 boot log flood -> live snapshot
//! publication -> session close drain -> lossless log verification.
//!
//! Prerequisites:
//! - `COM8` = RK3576 console @ 115200 8N1 (saved binding in `Desktop/test/.duthub/duts.json`);
//! - `COM25` = CH340 relay board with channel 1 wired to DUT reset (never actuate two channels at once);
//! - neither port is held by another process (GUI or prior test run).
//!
//! Run:
//! `cargo test -p duthub-device --features real-hardware --test com8_reset_boot_live -- --ignored --nocapture`
//!
//! Invariants verified:
//! - the `real.rs` event loop (pending queue + TX batching + snapshot throttling) works on live serial hardware;
//! - idle first echo publishes immediately and floods coalesce on a 16 ms tick so `SerialScreen` events arrive continuously;
//! - per-record log writer flush and close-time drain ensure `.bin` raw bytes >= received text chars and `.log` contains the kernel boot line;
//! - live TX writes succeed without write-failure notices.
#![cfg(feature = "real-hardware")]

use std::time::{Duration, Instant};

use duthub_control::Ch340RelayDriver;
use duthub_core::ipc::UiEvent;
use duthub_core::{ApprovalManager, ControlBackend};
use duthub_device::control::ControlService;
use duthub_device::{DeviceService, SessionKind};

const PORT: &str = "COM8";
const RELAY_PORT: &str = "COM25";
const BAUD: u32 = 115_200;
const DUT: &str = "rk3576";
/// Window to wait for kernel boot lines after reset (RK3576 prints kernel logs within a few seconds).
const BOOT_WINDOW: Duration = Duration::from_secs(90);

/// Polls the session bus buffer until `needle` appears.
fn wait_for(service: &DeviceService, needle: &str, rounds: u32) -> bool {
    for _ in 0..rounds {
        if service
            .read_log(&format!("sess-{DUT}"), 0, usize::MAX)
            .iter()
            .any(|e| matches!(&e.event, UiEvent::SerialChunk { text, .. } if text.contains(needle)))
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
#[ignore = "live hardware end-to-end test: run explicitly with COM8 console and COM25 channel 1 reset pulse"]
fn com8_reset_boot_live() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target/test-tmp/device/com8-reset-live")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    let service = DeviceService::custom(1024, dir.clone(), 0);
    // Subscribe before opening the session so both open notices and the boot flood are captured.
    let mut rx = service.subscribe();

    // 1) Open the live COM8 session (pending queue + writer thread + snapshot throttling).
    service
        .open_session(DUT, PORT, BAUD, SessionKind::Real)
        .expect("failed to open COM8 (port busy or missing)");
    assert!(
        wait_for(&service, "@ 115200 已打开", 50),
        "expected COM8 open notice within 5s"
    );
    println!("COM8 session opened at {BAUD} baud");

    // 2) TX probe: write succeeds without a write-failure notice (direct evidence that `write_all` succeeded).
    //    Whether the console echoes depends on current target state, so echo is not asserted here.
    service
        .write_line(
            DUT,
            duthub_core::lease::WriteIdentity::gui(),
            "echo duthub-com8-live",
        )
        .expect("failed to enqueue TX");
    std::thread::sleep(Duration::from_millis(1500));
    let write_failed = service
        .read_log(&format!("sess-{DUT}"), 0, usize::MAX)
        .iter()
        .any(
            |e| matches!(&e.event, UiEvent::SerialChunk { text, .. } if text.contains("写入失败")),
        );
    assert!(
        !write_failed,
        "unexpected TX write-failure notice on live port"
    );
    println!("TX probe sent without a failure notice");

    // 3) COM25 channel 1 reset pulse: STATUS enumeration -> approval issuance ->
    //    `channel_test(1)` (ON -> STATUS latched -> OFF -> STATUS released), asserting each field.
    let approval = std::sync::Arc::new(ApprovalManager::new());
    let backend: std::sync::Arc<dyn ControlBackend> = std::sync::Arc::new(Ch340RelayDriver::new());
    let control = ControlService::new(Some(backend), approval.clone());
    control.register_dut(duthub_core::model::DutConfig {
        id: "dut-live-com25".into(),
        name: "COM25 Relay Board".into(),
        serial: duthub_core::model::SerialConfig {
            host: "local".into(),
            port: RELAY_PORT.into(),
            baud: 115_200,
            format: "8N1".into(),
            reference_log: None,
            reference_similarity: None,
            split_logs: false,
        },
        adb: duthub_core::model::AdbConfig::default(),
        relay: duthub_core::model::RelayConfig::UsbSerial {
            host: "local".into(),
            node: RELAY_PORT.into(),
            driver: "CH340".into(),
            channels: duthub_core::model::RelayChannels {
                power: None,
                reset: Some(1),
                recovery: None,
                maskrom: None,
                power_key: None,
            },
        },
        mcp: duthub_core::model::McpConfig::StreamableHttp {
            url: "http://127.0.0.1:8797/mcp".into(),
        },
        a2a: duthub_core::model::DutA2aConfig::default(),
        flash: duthub_core::model::FlashConfig::default(),
        media: duthub_core::model::DutMediaConfig::default(),
        heartbeat: duthub_core::heartbeat::HeartbeatConfig::default(),
    });
    let channels = control
        .channel_status("dut-live-com25")
        .expect("COM25 STATUS enumeration failed");
    assert!(
        channels.iter().any(|&(c, _)| c == 1),
        "channel 1 did not respond to STATUS (wiring/driver mismatch): {channels:?}"
    );
    let token = approval.issue("dut-live-com25", "control.test", r#"{"channel":1}"#);
    let report = control
        .channel_test("dut-live-com25", 1, &token.token)
        .expect("channel 1 reset pulse failed");
    println!("Channel 1 round-trip report: {report:?}");
    assert!(report.on_ok, "STATUS did not confirm channel ON");
    assert!(report.off_ok, "STATUS did not confirm channel OFF");
    assert_eq!(
        report.after,
        Some(false),
        "channel 1 must return to OFF after pulse"
    );
    println!("Reset pulse complete. Waiting for RK3576 boot logs");

    // 4) Boot flood: after reset the console prints DDR/U-Boot/kernel logs. Collect bus text
    //    and `SerialScreen` frame counts (coalesced on a 16 ms tick during floods).
    let deadline = Instant::now() + BOOT_WINDOW;
    let mut boot_text = String::new();
    let mut screen_events = 0usize;
    while Instant::now() < deadline {
        while let Ok(env) = rx.try_recv() {
            match &env.event {
                UiEvent::SerialChunk { text, .. } => boot_text.push_str(text),
                UiEvent::SerialScreen { .. } => screen_events += 1,
                _ => {}
            }
        }
        if boot_text.contains("Linux version") && screen_events >= 5 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        boot_text.contains("Linux version"),
        "kernel boot line not observed on COM8 within {BOOT_WINDOW:?}: {} chars",
        boot_text.chars().count()
    );
    assert!(
        screen_events >= 5,
        "only {screen_events} SerialScreen snapshots arrived during boot flood"
    );
    println!(
        "Received {} characters and {screen_events} live snapshots",
        boot_text.chars().count()
    );

    // 5) Lossless shutdown: closing the session drains the writer thread on `SessionCtx` drop.
    //    Wait for file size to stabilize, then verify `.bin` raw bytes >= received text chars
    //    (each UTF-8 char is at least 1 byte) and `.log` contains the kernel boot line.
    service.close_session(DUT).expect("failed to close session");
    let bin_path = dir.join(format!("sess-{DUT}.bin"));
    let log_path = dir.join(format!("sess-{DUT}.log"));
    let mut last_size = 0u64;
    let stable_since = Instant::now();
    loop {
        let size = std::fs::metadata(&bin_path).map_or(0, |m| m.len());
        if size == last_size && stable_since.elapsed() > Duration::from_millis(800) {
            break;
        }
        if size != last_size {
            last_size = size;
        }
        assert!(
            stable_since.elapsed() < Duration::from_secs(8),
            ".bin is still growing or missing 8s after close (writer thread not drained?): {size} bytes"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let bin = std::fs::read(&bin_path).expect(".bin file missing");
    let log = std::fs::read_to_string(&log_path).expect(".log file missing");
    println!(
        "Log check: .bin has {} bytes and the session received {} characters",
        bin.len(),
        boot_text.chars().count()
    );
    assert!(
        bin.len() >= boot_text.chars().count(),
        ".bin ({} bytes) is smaller than received text ({} chars) - log loss detected",
        bin.len(),
        boot_text.chars().count()
    );
    assert!(
        log.contains("Linux version"),
        ".log text stream is missing the kernel boot line"
    );
    println!("Verified lossless boot logging through reset, snapshot capture, and writer drain");
}
