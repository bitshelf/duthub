//! Live hardware keyboard-path latency and correctness test on `COM8`:
//! sends keys via `WriteBytes` (no line ending appended, no local echo) to
//! measure TX-to-echo and TX-to-snapshot latency, verifies space and backspace
//! behavior across both the echo stream and snapshot grid, and finishes with an
//! 8-backspace burst to verify TX batching drain behavior.
//!
//! Prerequisites: `COM8` = RK3576 console @ 115200 8N1 with the peer TTY in an
//! echoing state (shell or login prompt); the GUI must release `COM8` first.
//! If the peer is still printing boot logs, wait for quiet + prompt before
//! measuring (the TTY does not echo during boot).
//!
//! Run:
//! `cargo test -p duthub-device --features real-hardware --test com8_io_latency_live -- --ignored --nocapture`
//! Set `DUTHUB_SERIAL_TEST_PORT` to exercise a different shell console at 115200 8N1.
#![cfg(feature = "real-hardware")]

use std::time::{Duration, Instant};

use duthub_core::ipc::UiEvent;
use duthub_device::{DeviceService, SessionKind};

const PORT: &str = "COM8";
const BAUD: u32 = 115_200;
const DUT: &str = "rk3576";

/// Result of a single keystroke latency measurement in microseconds.
struct Latency {
    echo_us: u128,
    snapshot_us: Option<u128>,
}

/// Collects echoed text and snapshot cursor rows for keystroke assertions.
#[derive(Default)]
struct Collector {
    echoed: String,
    /// Cursor row text per snapshot event (`None` if the event has no cursor row).
    cursor_rows: Vec<Option<String>>,
}

impl Collector {
    fn drain(
        &mut self,
        rx: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
    ) {
        while let Ok(env) = rx.try_recv() {
            match &env.event {
                UiEvent::SerialChunk { text, .. } => self.echoed.push_str(text),
                UiEvent::SerialScreen { screen } => {
                    let row = screen
                        .cursor_row
                        .and_then(|idx| screen.rows.get(idx as usize).map(|r| r.text.clone()));
                    self.cursor_rows.push(row);
                }
                _ => {}
            }
        }
    }

    /// Waits until 2 seconds elapse with no new text (boot flood quieted).
    fn wait_quiet(
        &mut self,
        rx: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
    ) {
        let mut last_growth = Instant::now();
        let mut last_len = self.echoed.len();
        while last_growth.elapsed() < Duration::from_secs(2) {
            self.drain(rx);
            if self.echoed.len() != last_len {
                last_len = self.echoed.len();
                last_growth = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for a shell/login prompt (Android init can take several minutes;
    /// periodically sends CR to wake the prompt when `wake` is `Some`).
    fn wait_prompt(
        &mut self,
        rx: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
        wake: Option<&DeviceService>,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut last_wake = Instant::now();
        while Instant::now() < deadline {
            self.drain(rx);
            let tail: String = {
                let chars: Vec<char> = self.echoed.chars().collect();
                let start = chars.len().saturating_sub(80);
                chars[start..].iter().collect()
            };
            if tail.contains("# ")
                || tail.contains("$ ")
                || tail.contains("login:")
                || tail.contains("~#")
                || tail.contains("/ #")
            {
                return true;
            }
            if let Some(svc) = wake
                && last_wake.elapsed() >= Duration::from_secs(15)
            {
                let _ = svc.write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), b"\r");
                last_wake = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }
}

#[test]
#[ignore = "live hardware keyboard latency and correctness test: run explicitly with exclusive COM8 access and echoing peer"]
fn com8_io_latency_live() {
    let port = std::env::var("DUTHUB_SERIAL_TEST_PORT").unwrap_or_else(|_| PORT.to_owned());
    let dir = std::env::current_dir()
        .unwrap()
        .join("target/test-tmp/device/com8-io-latency")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    let service = DeviceService::custom(1024, dir, 0).with_scrollback(1_000_000);
    let mut rx = service.subscribe();

    service
        .open_session(DUT, &port, BAUD, SessionKind::Real)
        .expect("failed to start serial session");
    assert!(
        (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            service
                .read_log(&format!("sess-{DUT}"), 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(&e.event,
                    UiEvent::SerialChunk { text, .. } if text.contains("已打开"))
                })
        }),
        "expected {port} open notice within 5s (port may be busy)"
    );
    println!("Opened {port} at {BAUD} baud. Waiting for the peer");

    let mut col = Collector::default();

    // 0a) Wait for any boot flood to go quiet (2s silence); kernel boot output does not echo input.
    col.wait_quiet(&mut rx);
    service
        .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), b"\r")
        .expect("failed to wake prompt");
    // 0b) Console does not reprint its prompt on attach: wait for shell readiness (Android init
    //     may take several minutes) with periodic CR wakes.
    assert!(
        col.wait_prompt(&mut rx, Some(&service)),
        "no shell/login prompt within 300s; peer is not interactive"
    );
    // 0c) Send Ctrl-C to clear any leftover partial line, then drain again.
    service
        .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[0x03])
        .expect("failed to enqueue TX");
    std::thread::sleep(Duration::from_millis(400));
    col.drain(&mut rx);
    println!("The peer prompt appeared. Measuring keyboard latency");

    // 1) Per-key latency for "ab cd123" (including space on the same path).
    //    Drain the baseline before each key and match by echoed character.
    let keys: Vec<&[u8]> = vec![b"a", b"b", b" ", b"c", b"d", b"1", b"2", b"3"];
    let mut latencies: Vec<(String, Latency)> = Vec::new();
    for k in &keys {
        col.drain(&mut rx); // Drain baseline.
        let ch = String::from_utf8_lossy(k).to_string();
        let t0 = Instant::now();
        service
            .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), k)
            .expect("failed to enqueue TX");
        let base_echo = col.echoed.len();
        let base_snaps = col.cursor_rows.len();
        let mut got = Latency {
            echo_us: 0,
            snapshot_us: None,
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            col.drain(&mut rx);
            if got.echo_us == 0
                && col.echoed[base_echo.min(col.echoed.len())..].contains(ch.as_str())
            {
                got.echo_us = t0.elapsed().as_micros();
            }
            if got.snapshot_us.is_none() && col.cursor_rows.len() > base_snaps {
                got.snapshot_us = Some(t0.elapsed().as_micros());
            }
            if got.echo_us > 0 && got.snapshot_us.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            got.echo_us > 0,
            "no echo for {ch:?} within 3s (peer echo state changed?); tail: {:?}",
            {
                let chars: Vec<char> = col.echoed.chars().collect();
                let start = chars.len().saturating_sub(60);
                chars[start..].iter().collect::<String>()
            }
        );
        latencies.push((ch, got));
        std::thread::sleep(Duration::from_millis(120)); // Inter-key delay simulating typing.
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "microsecond latencies fit well within f64 mantissa"
    )]
    let us_to_ms = |us: u128| us as f64 / 1000.0;
    println!("\n=== Per-key latency (TX to echo | TX to snapshot)===");
    for (ch, l) in &latencies {
        println!(
            "  {ch:?}: {:>6.1} ms | {}",
            us_to_ms(l.echo_us),
            match l.snapshot_us {
                Some(us) => format!("{:>6.1} ms", us_to_ms(us)),
                None => "(no snapshot event)".to_string(),
            }
        );
    }
    let mut echo_vals: Vec<u128> = latencies.iter().map(|(_, l)| l.echo_us).collect();
    echo_vals.sort_unstable();
    let avg = echo_vals.iter().sum::<u128>() / echo_vals.len().max(1) as u128;
    let p95 = echo_vals[(echo_vals.len() * 95).div_ceil(100) - 1];
    println!(
        "Echo latency: min {:.1} / avg {:.1} / p95 {:.1} / max {:.1} ms",
        us_to_ms(echo_vals[0]),
        us_to_ms(avg),
        us_to_ms(p95),
        us_to_ms(echo_vals[echo_vals.len() - 1])
    );

    // 2) Space correctness (echo stream + snapshot grid):
    //    a) the echo stream must contain a space immediately after "ab";
    //    b) the snapshot cursor row must preserve the trailing space when the cursor
    //       rests on trailing whitespace (e.g. "# ab ").
    // Use `rfind` to pick the latest typed "ab" in case an earlier wake CR produced error text containing "ab".
    let ab_pos = col
        .echoed
        .rfind("ab")
        .expect("missing \"ab\" in echo stream");
    let after_ab = &col.echoed[ab_pos + 2..];
    assert!(
        after_ab.starts_with(' '),
        "missing space echo after \"ab\"; got {after_ab:?}"
    );
    let space_snap = col
        .cursor_rows
        .iter()
        .flatten()
        .any(|row| row.trim_end().ends_with("ab") && row.ends_with(' '));
    assert!(
        space_snap,
        "snapshot never showed trailing space in \"ab \" row: {:?}",
        col.cursor_rows.last()
    );
    println!("Verified the space in the echo and the trailing space in the snapshot");

    // 3) Backspace correctness: deleting the trailing '3' must remove exactly one
    //    character from the snapshot row while preserving the rest unchanged.
    let row_before_bs = col
        .cursor_rows
        .iter()
        .flatten()
        .rev()
        .find(|row| row.contains("ab"))
        .expect("missing cursor row before backspace")
        .clone();
    service
        .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[0x7f])
        .expect("failed to enqueue TX");
    let base_snaps = col.cursor_rows.len();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bs_echo = false;
    while Instant::now() < deadline {
        col.drain(&mut rx);
        if col.echoed[ab_pos..].contains('\u{8}') {
            bs_echo = true;
        }
        if bs_echo && col.cursor_rows.len() > base_snaps {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        bs_echo,
        "missing \\x08 echo after backspace (peer did not erase with BS-SP-BS)"
    );
    let after_bs = col
        .cursor_rows
        .iter()
        .flatten()
        .rev()
        .find(|row| row.contains("ab"))
        .expect("missing cursor row after backspace");
    assert_eq!(
        after_bs.trim_end(),
        row_before_bs
            .trim_end()
            .strip_suffix('3')
            .unwrap_or("")
            .trim_end(),
        "backspace must delete exactly one trailing char: {row_before_bs:?} -> {after_bs:?}"
    );
    println!(
        "Verified the backspace echo and deletion of exactly one character: {row_before_bs:?} -> {after_bs:?}"
    );

    // 4) Burst drain with TX batching: append 8 characters, then enqueue 8 backspaces back-to-back
    //    so `absorb_keystrokes` batches them into a single write, measuring total drain time.
    let fill: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h"];
    for k in &fill {
        service
            .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), k)
            .expect("failed to enqueue TX");
        std::thread::sleep(Duration::from_millis(120));
    }
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    for _ in 0..8 {
        service
            .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[0x7f])
            .expect("failed to enqueue TX");
    }
    let enqueued_us = t0.elapsed().as_micros();
    let base = col.echoed.len();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bs_count = 0usize;
    while Instant::now() < deadline {
        col.drain(&mut rx);
        bs_count = col.echoed[base.min(col.echoed.len())..]
            .matches('\u{8}')
            .count();
        if bs_count >= 8
            && col
                .cursor_rows
                .last()
                .and_then(Option::as_deref)
                .is_some_and(|row| row.ends_with("ab cd12"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        bs_count >= 8,
        "burst of 8 backspaces only produced {bs_count} erase echoes within 5s"
    );
    assert!(
        col.cursor_rows
            .last()
            .and_then(Option::as_deref)
            .is_some_and(|row| row.ends_with("ab cd12")),
        "did not return to expected input line after burst backspaces"
    );
    println!(
        "\n=== Burst drain with TX batching===\n  8 backspaces enqueued in {enqueued_us} µs, expected to batch into one USB write\n  All 8 erase echoes arrived in {} ms",
        us_to_ms(t0.elapsed().as_micros())
    );

    measure_repeated_edits(&service, &mut col, &mut rx);

    // 5) Cleanup: send Ctrl-C to reset the current input line, then close the session to drain logs.
    service
        .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[0x03])
        .expect("failed to enqueue TX");
    std::thread::sleep(Duration::from_millis(300));
    service.close_session(DUT).expect("failed to close session");
    println!("Session closed after the log writer drained");
}

fn measure_repeated_edits(
    service: &DeviceService,
    collector: &mut Collector,
    rx: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
) {
    const PAIRS: usize = 120;
    collector.drain(rx);
    let mut samples = Vec::with_capacity(2 * PAIRS);
    for _ in 0..PAIRS {
        for (key, echo, expected) in [(b' ', ' ', "ab cd12 "), (0x7f, '\u{8}', "ab cd12")] {
            let echo_start = collector.echoed.len();
            let frame_start = collector.cursor_rows.len();
            let started = Instant::now();
            service
                .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[key])
                .expect("failed to enqueue keystroke");
            let mut echo_at = None;
            let mut frame_at = None;
            while started.elapsed() < Duration::from_secs(2) {
                collector.drain(rx);
                if echo_at.is_none() && collector.echoed[echo_start..].contains(echo) {
                    echo_at = Some(started.elapsed());
                }
                if frame_at.is_none()
                    && collector.cursor_rows[frame_start..]
                        .iter()
                        .flatten()
                        .any(|row| row.ends_with(expected))
                {
                    frame_at = Some(started.elapsed());
                }
                if echo_at.is_some() && frame_at.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let (Some(echo_at), Some(frame_at)) = (echo_at, frame_at) else {
                panic!(
                    "Missing echo or snapshot for key {key:#04x}: expected line ending {expected:?}, got {:?}",
                    collector.cursor_rows.last()
                );
            };
            samples.push((key, echo_at, frame_at));
            std::thread::sleep(Duration::from_millis(18));
        }
    }
    for (key, label) in [(b' ', "space"), (0x7f, "backspace")] {
        let mut echo = Vec::with_capacity(PAIRS);
        let mut frame = Vec::with_capacity(PAIRS);
        for (_, echo_at, frame_at) in samples.iter().filter(|(k, _, _)| *k == key) {
            echo.push(*echo_at);
            frame.push(*frame_at);
        }
        echo.sort_unstable();
        frame.sort_unstable();
        let p95 = (PAIRS * 95).div_ceil(100) - 1;
        println!(
            "{label}, {PAIRS} samples: echo p95 {:.1} ms, snapshot p95 {:.1} ms, snapshot max {:.1} ms",
            echo[p95].as_secs_f64() * 1000.0,
            frame[p95].as_secs_f64() * 1000.0,
            frame[PAIRS - 1].as_secs_f64() * 1000.0
        );
    }
}
