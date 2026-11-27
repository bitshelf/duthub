//! Real serial I/O backed by Tokio serial under the real-hardware feature.
//!
//! A single task owns the UART, reads from it, and handles queued commands.
//! Feed received bytes directly into the terminal emulator.
//!   The device controls echo, prompts, the cursor, and ANSI modes.
//!   The UI renders terminal snapshots without synthesizing local TX output.
//! Record received bytes in the binary log and their UTF-8 text in the text log.
//!   The event bus also publishes the received chunks for MCP serial reads.
//!   Transmitted bytes go to the audit log alone.
//! Publish a disconnection event and then retry with exponential backoff.
//!   Keep the terminal screen across reconnects.
//! Changing baud closes the current handle and reopens it at the new speed.

use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::Instrument as _;

use duthub_core::ipc::ChunkStyle;

use crate::demo::{SessionCommand, SessionCtx, absorb_keystrokes};
use crate::session::{LineEnding, reconnect_backoff};
use crate::term::{self, TermGrid};

const READ_BUF: usize = 4096;
/// Continuous output shares a frame, while the first echo after idle publishes immediately.
const SNAPSHOT_TICK: Duration = Duration::from_millis(16);
const RECONNECT_BASE: Duration = Duration::from_millis(300);
const RECONNECT_MAX: Duration = Duration::from_secs(8);
pub async fn run_real_session(
    ctx: SessionCtx,
    mut cmd_rx: mpsc::Receiver<SessionCommand>,
    port: String,
    baud: u32,
    opened_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    scrollback_rows: usize,
    snapshot_max_rows: usize,
) {
    // Keep the terminal grid and its parser across reconnects.
    // A new session reads the configured history and snapshot limits. History covers at least the snapshot.
    let snapshot_max_rows = snapshot_max_rows.max(100);
    let mut term = term::create_term_with_scrollback(scrollback_rows.max(snapshot_max_rows));
    let mut parser = Processor::<StdSyncHandler>::new();
    let mut screen_dirty = false;

    let mut current_baud = baud;
    let mut ending = LineEnding::default();
    let mut attempt: u32 = 0;
    let mut ever_opened = false;

    'reconnect: loop {
        // Open the serial device.
        let mut stream =
            match tokio_serial::SerialStream::open(&tokio_serial::new(&port, current_baud)) {
                Ok(stream) => stream,
                Err(err) => {
                    ctx.emit_text(
                        &format!("[serial] 打开 {port} @ {current_baud} 失败: {err}"),
                        ChunkStyle::Error,
                    );
                    ctx.emit_disconnected(&format!("open failed: {err}"));
                    // Report the initial open failure once and exit.
                    // This avoids flooding the log when the port is already in use.
                    // After a successful connection, use backoff for a disconnect or failed reopen.
                    if !ever_opened {
                        return;
                    }
                    attempt += 1;
                    let backoff = reconnect_backoff(attempt, RECONNECT_BASE, RECONNECT_MAX);
                    ctx.emit_text(
                        &format!("[serial] 第 {attempt} 次重连 · {backoff:?} 后重试"),
                        ChunkStyle::Dim,
                    );
                    ctx.reconnect_wait(backoff).await;
                    continue 'reconnect;
                }
            };
        attempt = 0;
        ever_opened = true;
        opened_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        ctx.emit_text(
            &format!("[serial] {port} @ {current_baud} 已打开"),
            ChunkStyle::Dim,
        );

        // Handle serial reads and snapshot ticks, and dispatch pending commands in order.
        // Batch consecutive keyboard input that shares the same lease.
        // If batching pulls another command from the queue, dispatch it from the pending list.
        let mut buf = vec![0u8; READ_BUF];
        let mut snapshot_tick = tokio::time::interval(SNAPSHOT_TICK);
        snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_snapshot = None::<Instant>;
        let mut pending = std::collections::VecDeque::new();
        'event: loop {
            let cmd = match pending.pop_front() {
                Some(c) => c,
                None => {
                    tokio::select! {
                        read = stream.read(&mut buf) => match read {
                            Ok(0) => {
                                ctx.emit_disconnected("device EOF");
                                break 'event;
                            }
                            Ok(n) => {
                                let _span =
                                    tracing::info_span!("serial.receive", bytes = n).entered();
                                // Publish the raw RX block to the binary log, text log, and serial event bus.
                                ctx.emit_raw(&buf[..n]);
                                // Feed the bytes into the emulator for echo, cursor updates, and ANSI behavior.
                                parser.advance(&mut term, &buf[..n]);
                                screen_dirty = true;
                                let now = Instant::now();
                                if last_snapshot
                                    .is_none_or(|last| now.duration_since(last) >= SNAPSHOT_TICK)
                                    && parser.sync_timeout().sync_timeout().is_none()
                                {
                                    ctx.emit_screen(term::snapshot_screen_scrollback(&term, snapshot_max_rows));
                                    screen_dirty = false;
                                    last_snapshot = Some(now);
                                    snapshot_tick.reset();
                                }
                                continue 'event;
                            }
                            Err(err) => {
                                ctx.emit_disconnected(&format!("read error: {err}"));
                                break 'event;
                            }
                        },
                        cmd = cmd_rx.recv() => match cmd {
                            None | Some(SessionCommand::Close) => {
                                let _ = stream.shutdown().await;
                                return;
                            }
                            Some(c) => c,
                        },
                        _ = snapshot_tick.tick() => {
                            screen_dirty |=
                                term::flush_expired_sync(&mut term, &mut parser, Instant::now());
                            if screen_dirty && parser.sync_timeout().sync_timeout().is_none() {
                                screen_dirty = false;
                                ctx.emit_screen(term::snapshot_screen_scrollback(&term, snapshot_max_rows));
                                last_snapshot = Some(Instant::now());
                                snapshot_tick.reset();
                            }
                            continue 'event;
                        }
                    }
                }
            };
            match cmd {
                // A close command pulled during batching follows the ordinary shutdown path.
                SessionCommand::Close => {
                    let _ = stream.shutdown().await;
                    return;
                }
                SessionCommand::Write { text, lease }
                | SessionCommand::WriteHeartbeatBatch { text, lease, .. } => {
                    let _io = ctx.begin_actor_io().await;
                    // Confirm the lease generation immediately before writing to the UART.
                    // Record a rejected write without touching the serial handle.
                    if !ctx.admit_actor_write(&lease) {
                        continue 'event;
                    }
                    // Apply the session ending to line writes, including MCP and heartbeat commands.
                    let payload = ending.apply(&text);
                    match stream.write_all(payload.as_bytes()).await {
                        Ok(()) => {
                            // Store the transmitted bytes and the TX note in the audit logs.
                            // Only the device produces terminal echo.
                            ctx.log_tx(payload.as_bytes(), &text);
                        }
                        Err(err) => {
                            ctx.emit_text(&format!("[serial] 写入失败: {err}"), ChunkStyle::Error);
                        }
                    }
                    let _ = stream.flush().await;
                }
                SessionCommand::WriteBytes { bytes, lease } => {
                    // Batch consecutive input under one lease into a single write transaction.
                    // Each command already passed admission with the same lease context.
                    // Recheck that context once for the batch. Keep other leases and command types
                    // in the pending queue for ordered dispatch.
                    let (batch, deferred) = absorb_keystrokes(&mut cmd_rx, bytes, lease.as_ref());
                    if let Some(other) = deferred {
                        pending.push_back(other);
                    }
                    let merged = batch.concat();
                    let _io = ctx.begin_actor_io().await;
                    // Recheck the lease immediately before the combined write.
                    if !ctx.admit_actor_write(&lease) {
                        continue 'event;
                    }
                    // Write keyboard bytes as supplied, without an appended ending or local echo.
                    let write = stream
                        .write_all(&merged)
                        .instrument(tracing::info_span!(
                            "serial.key_write",
                            bytes = merged.len()
                        ))
                        .await;
                    match write {
                        Ok(()) => {
                            // Log original command boundaries even when the physical write is batched.
                            for piece in &batch {
                                ctx.log_keystrokes(piece);
                            }
                        }
                        Err(err) => {
                            ctx.emit_text(&format!("[serial] 写入失败: {err}"), ChunkStyle::Error);
                        }
                    }
                    let _ = stream.flush().await;
                }
                SessionCommand::Resize { cols, rows } => {
                    if cols > 0 && rows > 0 {
                        term.resize(TermGrid::new(cols, rows));
                        screen_dirty = true;
                    }
                }
                SessionCommand::ResetTerminal => {
                    term::reset_terminal(&mut term, &mut parser);
                    // The detection window describes the byte stream, not the
                    // screen; a manual reset starts a fresh judgement too.
                    ctx.reset_garble();
                    screen_dirty = false;
                    ctx.emit_screen(term::snapshot_screen_scrollback(&term, snapshot_max_rows));
                    last_snapshot = Some(Instant::now());
                    snapshot_tick.reset();
                }
                SessionCommand::ReadTerminalHistory { skip, reply } => {
                    if !reply.is_closed() {
                        let _ = reply.send(term::snapshot_terminal_history(&term, skip));
                    }
                }
                SessionCommand::Mark(label) => {
                    // Record a segment marker in the logs and terminal stream without writing to the UART.
                    ctx.emit_mark(&label);
                }
                SessionCommand::SetEnding(new_ending) => {
                    // Apply line ending changes to line writes. The keyboard supplies its own Enter byte.
                    ending = new_ending;
                    ctx.emit_text(
                        &format!("[serial] 行尾策略 → {}", new_ending.label()),
                        ChunkStyle::Dim,
                    );
                }
                SessionCommand::SetBaud(new_baud) => {
                    // Close the handle before reopening at the requested speed.
                    current_baud = new_baud;
                    // Bytes sampled at the old baud rate say nothing about the
                    // new one; clear the window before reconnecting.
                    ctx.reset_garble();
                    let _ = stream.shutdown().await;
                    continue 'reconnect;
                }
            }
        }
        // Retry after a disconnect. A manual reconnect can wake the backoff early.
        attempt += 1;
        let backoff = reconnect_backoff(attempt, RECONNECT_BASE, RECONNECT_MAX);
        ctx.emit_text(
            &format!("[serial] 第 {attempt} 次重连 · {backoff:?} 后重试"),
            ChunkStyle::Dim,
        );
        ctx.reconnect_wait(backoff).await;
    }
}

/// Enumerate local serial ports and report a short error on failure.
pub fn available_ports() -> duthub_core::DeviceResult<Vec<duthub_core::SerialPortInfo>> {
    let ports = tokio_serial::available_ports()
        .map_err(|e| duthub_core::DeviceError::Invalid(e.to_string()))?;
    Ok(ports
        .into_iter()
        .map(|p| duthub_core::SerialPortInfo {
            port: p.port_name,
            description: match p.port_type {
                tokio_serial::SerialPortType::UsbPort(usb) => Some(
                    format!(
                        "USB {:04x}:{:04x} {}",
                        usb.vid,
                        usb.pid,
                        usb.product.clone().unwrap_or_default()
                    )
                    .trim_end()
                    .to_string(),
                ),
                tokio_serial::SerialPortType::BluetoothPort => Some("Bluetooth".into()),
                tokio_serial::SerialPortType::PciPort => Some("PCI".into()),
                tokio_serial::SerialPortType::Unknown => None,
            },
        })
        .collect())
}

#[cfg(all(test, feature = "real-hardware"))]
mod live_tests {
    use super::*;
    use duthub_core::ipc::EventEnvelope;

    #[test]
    #[ignore = "requires exclusive access to DUTHUB_TEST_SERIAL_PORT"]
    fn enumerate_and_open_configured_serial_port() {
        let port = std::env::var("DUTHUB_TEST_SERIAL_PORT").unwrap();
        let dir = std::env::temp_dir().join(format!("duthub-serial-probe-{}", std::process::id()));
        let service = crate::DeviceService::custom(256, dir, 0).with_log_archives(false);
        assert!(
            service
                .list_ports()
                .iter()
                .any(|candidate| candidate.port.eq_ignore_ascii_case(&port))
        );
        let session = service
            .open_session("serial-probe", &port, 115_200, crate::SessionKind::Real)
            .unwrap();
        let expected = format!("{port} @ 115200 已打开");
        let started = (0..30).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            service.read_log(&session, 0, 256).iter().any(|event| {
                matches!(&event.event, duthub_core::UiEvent::SerialChunk { text, .. } if text.contains(&expected))
            })
        });
        service.close_session("serial-probe").unwrap();
        assert!(started, "{port} was enumerated but did not open");
    }

    /// Collect serial text from live events and the session buffer, using the buffer as the source of truth.
    fn collect(
        service: &crate::DeviceService,
        rx: &mut crate::broadcast::Receiver<EventEnvelope>,
    ) -> Vec<String> {
        while rx.try_recv().is_ok() {}
        service
            .read_log("sess-com30-live", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                duthub_core::ipc::UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Poll the session log for the requested number of occurrences, every 100 ms.
    fn wait_for(
        service: &crate::DeviceService,
        rx: &mut crate::broadcast::Receiver<EventEnvelope>,
        needle: &str,
        min: usize,
        rounds: u32,
    ) -> bool {
        for _ in 0..rounds {
            if collect(service, rx)
                .iter()
                .filter(|t| t.contains(needle))
                .count()
                >= min
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Exercise baud switching on a real serial device.
    ///
    /// Use a serial shell on a real DUT. The original check used a root shell on COM30 at 115,200 baud.
    /// Open at 115,200 and confirm an echo command reached the device.
    /// Reopen at 9,600 without sending bytes and confirm the connection reports the new baud.
    /// Reopen at 115,200 and confirm the shell responds again.
    ///
    /// The serial chunk stream contains both the device echo and the command output.
    /// A response from the device verifies the write without relying on local TX echo.
    #[test]
    #[ignore = "live serial test on COM30: run manually with exclusive port access"]
    fn com30_baud_hotswitch_live() {
        const PORT: &str = "COM30";
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device/live-hotswitch")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let service = crate::DeviceService::custom(256, dir, 0);
        let mut rx = service.subscribe();
        service
            .open_session("com30-live", PORT, 115_200, crate::SessionKind::Real)
            .expect("failed to open COM30 (port busy or missing)");

        // Verify the initial 115,200-baud connection.
        assert!(
            wait_for(&service, &mut rx, "@ 115200 已打开", 1, 50),
            "did not observe 115200 open notice"
        );
        // Check the target shell's echo and output.
        service
            .write_line(
                "com30-live",
                duthub_core::lease::WriteIdentity::gui(),
                "echo duthub-hotsw-1",
            )
            .unwrap();
        assert!(
            wait_for(&service, &mut rx, "duthub-hotsw-1", 2, 50),
            "peer did not echo/output command result (requires interactive shell; no local TX echo)"
        );
        // Reopen at 9,600 without writing to the target.
        service.set_baud("com30-live", 9600).unwrap();
        assert!(
            wait_for(&service, &mut rx, "@ 9600 已打开", 1, 60),
            "did not observe reopen notice after SetBaud 9600"
        );
        // Verify the second 115,200-baud open and confirm the target responds again.
        service.set_baud("com30-live", 115_200).unwrap();
        assert!(
            wait_for(&service, &mut rx, "@ 115200 已打开", 2, 60),
            "did not observe second reopen notice after SetBaud 115200"
        );
        service
            .write_line(
                "com30-live",
                duthub_core::lease::WriteIdentity::gui(),
                "echo duthub-hotsw-2",
            )
            .unwrap();
        assert!(
            wait_for(&service, &mut rx, "duthub-hotsw-2", 2, 50),
            "peer did not echo/output command result after switching baud back"
        );
        service.close_session("com30-live").ok();
    }
}
