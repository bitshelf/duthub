//! 真实串口驱动（M1.2，`--features real-hardware`；D1：tokio-serial）。
//!
//! - 单 task 独占 UART 句柄，读写都经由命令通道（docs/ARCHITECTURE.md 线程边界）。
//! - 终端仿真（PuTTY 模式）：串口字节流直接进 alacritty_terminal `Term`
//!   （[crate::term] 模块），设备 shell 自己负责回显/提示符/光标/ANSI——本地
//!   不再伪造 [TX] 行、不做行重组；UI 渲染源是 Term 网格快照（SerialScreen）。
//! - 日志管道：RX 原始字节块进 .bin（真字节）+ .log（utf8-lossy 文本）+ 总线
//!   SerialChunk（MCP serial_read 审计源）；TX 走 `log_tx`/`log_keystrokes`
//!   仅审计不进终端流。
//! - 断线：`SerialDisconnected` + 指数退避重连（`reconnect_backoff`，上限可配）。
//!   仿真器跨重连保持（设备 shell 的屏幕状态不因链路抖动丢失）。
//! - SetBaud：按文档语义"断连协调"——关闭句柄后按新波特率重开。

use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

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
    // 终端仿真器（PuTTY 模式）：Term + vte Parser，跨重连保持。
    // 回滚/快照行数来自「串口」设置段（新会话生效）；Term 历史至少覆盖快照。
    let snapshot_max_rows = snapshot_max_rows.max(100);
    let mut term = term::create_term_with_scrollback(scrollback_rows.max(snapshot_max_rows));
    let mut parser = Processor::<StdSyncHandler>::new();
    let mut screen_dirty = false;

    let mut current_baud = baud;
    let mut ending = LineEnding::default();
    let mut attempt: u32 = 0;
    let mut ever_opened = false;

    'reconnect: loop {
        // 打开串口
        let mut stream =
            match tokio_serial::SerialStream::open(&tokio_serial::new(&port, current_baud)) {
                Ok(stream) => stream,
                Err(err) => {
                    ctx.emit_text(
                        &format!("[serial] 打开 {port} @ {current_baud} 失败: {err}"),
                        ChunkStyle::Error,
                    );
                    ctx.emit_disconnected(&format!("open failed: {err}"));
                    // 初次打开即失败（从未连上）：只报错一次并结束任务，
                    // 不进入重连循环——否则被占用的端口会无限刷屏错误。
                    // 曾连上后（断线/SetBaud 重开失败）仍走指数退避重试。
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

        // 事件循环：读串口 / 命令 / 快照节流。命令先收进 pending 队列再逐条
        // 分发——WriteBytes 会吸收队列中连续的同租约键入（TX 合并），被吸收
        // 途中拉出的异型命令退回队列按序执行，避免事件臂逻辑重复。
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
                                // 原始字节进日志管道（.bin 真字节 / .log 文本 / 总线 serial_read）
                                ctx.emit_raw(&buf[..n]);
                                // 终端仿真：回显/光标/ANSI 全部由仿真器处理（PuTTY 行为）
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
                // 合并期间从队列拉出的关闭命令：同关机语义
                SessionCommand::Close => {
                    let _ = stream.shutdown().await;
                    return;
                }
                SessionCommand::Write { text, lease }
                | SessionCommand::WriteHeartbeatBatch { text, lease, .. } => {
                    let _io = ctx.begin_actor_io().await;
                    // [E] actor 复核（SPEC §6.5.1）：写前最后一刻验证租约代数；
                    // 过期/被接管 → 丢弃写入（注记 + 审计），不触串口
                    if !ctx.admit_actor_write(&lease) {
                        continue 'event;
                    }
                    // write_line 路径（MCP serial_write / 心跳）：行尾策略生效
                    let payload = ending.apply(&text);
                    match stream.write_all(payload.as_bytes()).await {
                        Ok(()) => {
                            // 仅审计：.bin 记线上字节、.log 记 [TX] 行；
                            // 终端流不发本地回显（设备 shell 会回显）
                            ctx.log_tx(payload.as_bytes(), &text);
                        }
                        Err(err) => {
                            ctx.emit_text(&format!("[serial] 写入失败: {err}"), ChunkStyle::Error);
                        }
                    }
                    let _ = stream.flush().await;
                }
                SessionCommand::WriteBytes { bytes, lease } => {
                    // TX 合并：吸收队列中连续的同租约 WriteBytes（键盘突发合并为
                    // 一次 USB 写事务）。同租约合并不改变 admit 语义（租约上下文
                    // 逐命令已在入口复核，actor 复核对同 ctx 批次等效）；异租约/
                    // 异型命令退回 pending 按序执行。
                    let (batch, deferred) = absorb_keystrokes(&mut cmd_rx, bytes, &lease);
                    if let Some(other) = deferred {
                        pending.push_back(other);
                    }
                    let merged = batch.concat();
                    let _io = ctx.begin_actor_io().await;
                    // [E] actor 复核（同 Write 分支）
                    if !ctx.admit_actor_write(&lease) {
                        continue 'event;
                    }
                    // 键盘路径（PuTTY）：字节原样下发，无行尾追加、无本地回显
                    use tracing::Instrument as _;
                    let write = stream
                        .write_all(&merged)
                        .instrument(tracing::info_span!(
                            "serial.key_write",
                            bytes = merged.len()
                        ))
                        .await;
                    match write {
                        Ok(()) => {
                            // 键入审计按原始命令逐条记（.bin 字节与块边界都不变）
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
                    // 分段标记（日志分割）：不触串口，只进日志与终端流
                    ctx.emit_mark(&label);
                }
                SessionCommand::SetEnding(new_ending) => {
                    // API 保留；只影响 write_line 路径（键盘路径固定 \r）
                    ending = new_ending;
                    ctx.emit_text(
                        &format!("[serial] 行尾策略 → {}", new_ending.label()),
                        ChunkStyle::Dim,
                    );
                }
                SessionCommand::SetBaud(new_baud) => {
                    // 热切换语义：关闭句柄 → 按新波特率重开（H1.3）
                    current_baud = new_baud;
                    // Bytes sampled at the old baud rate say nothing about the
                    // new one; clear the window before reconnecting.
                    ctx.reset_garble();
                    let _ = stream.shutdown().await;
                    continue 'reconnect;
                }
            }
        }
        // 断线重连（等待期间可被手动重连提前唤醒，H1.4）
        attempt += 1;
        let backoff = reconnect_backoff(attempt, RECONNECT_BASE, RECONNECT_MAX);
        ctx.emit_text(
            &format!("[serial] 第 {attempt} 次重连 · {backoff:?} 后重试"),
            ChunkStyle::Dim,
        );
        ctx.reconnect_wait(backoff).await;
    }
}

/// 枚举本机串口（只读）。错误仅携带简讯。
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
                _ => None,
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

    /// 从订阅 + 会话缓冲收集 SerialChunk 文本（缓冲为准）。
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

    /// 轮询等待会话日志中 `needle` 出现次数 ≥ `min`（rounds × 100ms）。
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

    /// H1.3 波特率热切换真机验证（`cargo test -p duthub-device --features real-hardware -- --ignored --nocapture`）。
    ///
    /// 前置：对端为可执行 shell 的真实串口（实测 COM30 @ 115200，root shell）。
    /// 步骤：115200 打开 → 写 `echo` 得到设备执行输出 → SetBaud 9600（关闭重开
    /// 路径，期间不发送任何字节）→ 断言"@ 9600 已打开"→ SetBaud 115200 →
    /// 写 `echo` 再次得到设备输出（链路完整恢复）。
    ///
    /// 终端仿真重构后的断言形态：串口读侧按原始块进 SerialChunk（回显与执行
    /// 输出都在其中），不再有 [TX] 回显行——设备 echo 出现即证明写入成功。
    #[test]
    #[ignore = "真实串口占用 COM30，需人工执行（运行时必须没有其他进程占用该口）"]
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
            .expect("COM30 打开失败（口被占用或不存在）");

        // 1) 115200 打开确认
        assert!(
            wait_for(&service, &mut rx, "@ 115200 已打开", 1, 50),
            "未见到 115200 打开行"
        );
        // 2) 写入 → 对端 shell 本地回显命令行 + 执行输出（块文本）
        service
            .write_line(
                "com30-live",
                duthub_core::lease::WriteIdentity::gui(),
                "echo duthub-hotsw-1",
            )
            .unwrap();
        assert!(
            wait_for(&service, &mut rx, "duthub-hotsw-1", 2, 50),
            "对端未回显/未输出 echo 结果（需可执行 shell；无 [TX] 本地回显）"
        );
        // 3) 热切换 9600：关闭重开路径（期间不写任何字节，对端无噪声）
        service.set_baud("com30-live", 9600).unwrap();
        assert!(
            wait_for(&service, &mut rx, "@ 9600 已打开", 1, 60),
            "SetBaud 9600 后未见到重开确认行"
        );
        // 4) 切回 115200（会话日志中第二次出现 115200 打开行）并再写：链路完整恢复
        service.set_baud("com30-live", 115_200).unwrap();
        assert!(
            wait_for(&service, &mut rx, "@ 115200 已打开", 2, 60),
            "SetBaud 115200 后未见到第二次重开确认行"
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
            "切换回后对端未回显/未输出 echo 结果"
        );
        service.close_session("com30-live").ok();
    }
}
