//! 会话驱动抽象 + Demo 驱动（M1.1）。
//!
//! Demo 驱动回放 `tests/fixture-logs/` 样本（正常启动 + 内核 panic），随后进入
//! 交互式模拟 shell（移植原型 `response()` 行为），无硬件即可驱动全链路。
//! 真实驱动（M1.2）实现同一命令通道，走 tokio-serial + alacritty_terminal 仿真
//! （行重组/本地回显已废弃：回显与提示符由设备 shell 经仿真器渲染）。

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use tokio::sync::mpsc;

use std::time::Duration;

use tokio::sync::Notify;

use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::ipc::{ChunkStyle, EventEnvelope, UiEvent};
use duthub_core::lease::{LeaseAuthority, WriteCtx};
use duthub_core::{DeviceError, DeviceResult};

use crate::bus::EventBus;
use crate::logs::SessionLogWriter;
use crate::rec::RecKind;
use crate::session::LineEnding;

/// 键入突发合并上限（字节）：退格连击等突发合并为一次 USB 写事务，
/// 超限即落回逐条写（典型按键 ≤ 8 字节，256 ≈ 30+ 键足够覆盖一次连击）。
/// 逻辑与单测放在 feature-free 的本模块（real 模块仅真实驱动构建）。
#[cfg_attr(not(feature = "real-hardware"), allow(dead_code))]
pub(crate) const TX_MERGE_BYTES: usize = 256;

/// 吸收命令队列中连续的同租约 `WriteBytes`（键盘突发 → 一次写事务）。
/// 返回（字节批次，首个被拉出的异型命令——调用方退回队列按序执行）。
/// 每条原始命令保持独立元素：键入审计按原边界逐条落盘。同租约合并不
/// 改变 admit 语义（租约上下文逐命令已在入口复核，actor 复核对同 ctx
/// 批次等效）；异租约/异型命令不并入。
#[cfg_attr(not(feature = "real-hardware"), allow(dead_code))]
pub(crate) fn absorb_keystrokes(
    cmd_rx: &mut tokio::sync::mpsc::Receiver<SessionCommand>,
    first: Vec<u8>,
    lease: &Option<WriteCtx>,
) -> (Vec<Vec<u8>>, Option<SessionCommand>) {
    let total = |batch: &[Vec<u8>]| batch.iter().map(|b| b.len()).sum::<usize>();
    let mut batch = vec![first];
    let mut deferred = None;
    while total(&batch) < TX_MERGE_BYTES {
        match cmd_rx.try_recv() {
            Ok(SessionCommand::WriteBytes {
                bytes: more,
                lease: more_lease,
            }) if &more_lease == lease && total(&batch) + more.len() <= TX_MERGE_BYTES => {
                batch.push(more);
            }
            Ok(other) => {
                deferred = Some(other);
                break;
            }
            Err(_) => break,
        }
    }
    (batch, deferred)
}

/// 会话命令（单个 task 独占串口句柄，读写都进该 task，docs/ARCHITECTURE.md）。
pub enum SessionCommand {
    /// 用户输入的一行（已含或不含换行均可）。`lease` 为入口 admit 捕获的
    /// 租约上下文（None = 无租约要求路径：auth 关的 Agent 过渡写 / System）。
    Write {
        text: String,
        lease: Option<WriteCtx>,
    },
    /// 一次完整的多行心跳探测，保留原始脚本用于演示驱动。
    WriteHeartbeatBatch {
        text: String,
        batch: std::sync::Arc<crate::heartbeat::HeartbeatBatch>,
        lease: Option<WriteCtx>,
    },
    /// 原始字节直写（PuTTY 键盘路径：typed_char/Enter \r/Backspace 0x7f），
    /// 不做行尾追加、不发 [TX] 回显
    WriteBytes {
        bytes: Vec<u8>,
        lease: Option<WriteCtx>,
    },
    /// 重设波特率（真实驱动：断开重连协调）
    SetBaud(u32),
    /// 终端网格重设尺寸（窗口 resize → term.resize(cols, rows)）
    Resize { cols: usize, rows: usize },
    /// 重置本地终端仿真器。
    ResetTerminal,
    ReadTerminalHistory {
        skip: usize,
        reply: tokio::sync::oneshot::Sender<crate::term::TerminalHistoryPage>,
    },
    /// 会话级行尾策略（发送路径生效，见 LineEnding；仅 write_line 路径）
    SetEnding(LineEnding),
    /// 分段标记（日志分割）：写 `[mark] {label}` 行（Mark 样式），文本与索引同步
    Mark(String),
    /// 请求关闭
    Close,
}

/// 日志管道消息（异步写线程按入队顺序消费；字节与分段顺序严格保持）。
pub(crate) enum LogMsg {
    /// RX 原始字节块：.bin 记真字节（不转码），.log 记 utf8-lossy 文本（append）。
    Raw {
        bytes: Vec<u8>,
        text: String,
    },
    /// TX 线上字节 + [TX] 标记行（append；emit_tx / log_tx 共用）。
    Tx {
        wire: Vec<u8>,
        text: String,
    },
    /// 键入字节审计：仅 .bin（append_raw）。
    Keystrokes(Vec<u8>),
    /// 注记行（note）。
    Note(String),
    /// 分段标记（mark；失败由写线程补错误注记并广播）。
    Mark(String),
    /// 分段标记的同步等待版：供供电分段等需要立刻拿到成败的控制边界，
    /// Return the result after earlier log messages have drained.
    MarkSync {
        line: String,
        ack: std::sync::mpsc::Sender<DeviceResult<()>>,
    },
    SetBootReference(crate::boot_reference::BootReference),
    SetLogMode {
        split: bool,
        reference: Option<crate::boot_reference::BootReference>,
    },
    /// 排空屏障：此前入队的消息全部落盘后 ack（测试 / 排空等待用）。
    Flush(std::sync::mpsc::Sender<()>),
}

/// Complete the write after releasing the log mutex.
enum LogOutcome {
    Ok,
    Failed(DeviceError),
    SyncAck(
        std::sync::mpsc::Sender<DeviceResult<()>>,
        Option<DeviceError>,
    ),
}

/// Write one queued message, reporting storage failures outside the serial event stream.
fn apply_log_msg(
    logs: &Arc<Mutex<SessionLogWriter>>,
    msg: LogMsg,
    mut on_error: impl FnMut(&DeviceError),
) {
    let outcome = {
        let Ok(mut writer) = logs.lock() else {
            return;
        };
        if !matches!(
            &msg,
            LogMsg::Raw { .. } | LogMsg::MarkSync { .. } | LogMsg::SetLogMode { .. }
        ) && let Err(error) = writer.flush_rx_prefix()
        {
            tracing::warn!(%error, "Failed to flush the pending serial log prefix");
        }
        match msg {
            LogMsg::Raw { bytes, text } => writer
                .append_rx(bytes, &text)
                .map_or_else(LogOutcome::Failed, |()| LogOutcome::Ok),
            LogMsg::Tx { wire, text } => {
                let _ = writer.append(&wire, &text);
                let _ = writer.append_rec(RecKind::Tx, &wire);
                LogOutcome::Ok
            }
            LogMsg::Keystrokes(bytes) => {
                let _ = writer.append_raw(&bytes);
                let _ = writer.append_rec(RecKind::Keystrokes, &bytes);
                LogOutcome::Ok
            }
            LogMsg::Note(line) => {
                let _ = writer.note(&line);
                let _ = writer.append_rec(RecKind::Note, line.as_bytes());
                LogOutcome::Ok
            }
            LogMsg::Mark(line) => {
                // Rec first: `mark()` splits afterwards (normal mode), so the
                // mark record lands in the OLD segment — same placement as the
                // `.log` mark line. A rec error never turns a landed mark into
                // LogOutcome::Failed.
                let _ = writer.append_rec(RecKind::Mark, line.as_bytes());
                let result = writer.mark(&line);
                match result {
                    Ok(()) => LogOutcome::Ok,
                    Err(err) => LogOutcome::Failed(err),
                }
            }
            LogMsg::MarkSync { line, ack } => {
                let result = writer.power_mark(&line);
                match result {
                    Ok(()) => LogOutcome::SyncAck(ack, None),
                    Err(err) => LogOutcome::SyncAck(ack, Some(err)),
                }
            }
            LogMsg::SetBootReference(reference) => {
                writer.set_boot_reference(Some(reference));
                LogOutcome::Ok
            }
            LogMsg::SetLogMode { split, reference } => writer
                .set_log_mode(split, reference)
                .map_or_else(LogOutcome::Failed, |()| LogOutcome::Ok),
            LogMsg::Flush(ack) => {
                let _ = ack.send(());
                LogOutcome::Ok
            }
        }
    };
    match outcome {
        LogOutcome::Ok => {}
        LogOutcome::Failed(err) => on_error(&err),
        LogOutcome::SyncAck(ack, error) => {
            if let Some(err) = &error {
                on_error(err);
            }
            let _ = ack.send(match error {
                Some(err) => Err(err),
                None => Ok(()),
            });
        }
    }
}

/// 日志写线程：与今天完全相同的逐条 write + flush 语义（实时落盘、零丢失），
/// 只是移出 RX/写命令路径——磁盘 I/O 不再卡回显显示。recv 返回 Err 表示
/// 通道关闭且队列已排空，此时才退出（会话关闭后一字节不少）。
fn run_log_writer(
    logs: Arc<Mutex<SessionLogWriter>>,
    rx: std::sync::mpsc::Receiver<LogMsg>,
    session_id: String,
    dut_id: String,
    finished: Arc<(Mutex<bool>, Condvar)>,
) {
    let mut last_error = None;
    let mut on_error = |err: &DeviceError| {
        let error = err.to_string();
        if last_error.as_deref() != Some(error.as_str()) {
            tracing::warn!(%session_id, %dut_id, %error, "Serial log write failed");
            last_error = Some(error);
        }
    };
    while let Ok(msg) = rx.recv() {
        apply_log_msg(&logs, msg, &mut on_error);
    }
    let pending_error = logs
        .lock()
        .ok()
        .and_then(|mut writer| writer.flush_rx_prefix().err());
    if let Some(error) = pending_error {
        on_error(&error);
    }
    let (lock, cvar) = &*finished;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
}

/// 发射器：原始字节+文本行 → 日志管道；事件 → 总线 + 会话缓冲。
pub struct SessionCtx {
    pub dut_id: String,
    pub session_id: String,
    bus: Arc<EventBus>,
    logs: Option<Arc<Mutex<SessionLogWriter>>>,
    /// 日志写通道：RX/TX/注记全部异步落盘（None = 未挂日志或写线程未启动）。
    log_tx: Option<std::sync::mpsc::Sender<LogMsg>>,
    /// 写线程排空完成信号（Drop 关闭通道后等待，兜底超时）。
    log_finished: Option<Arc<(Mutex<bool>, Condvar)>>,
    reconnect_notify: Option<std::sync::Arc<Notify>>,
    /// 租约复核（[E] actor 复核用；None = 无租约要求路径）
    lease_authority: Option<Arc<dyn LeaseAuthority>>,
    /// 审计（actor 拒绝写入记 write.deny）
    audit: Option<Arc<AuditSink>>,
    /// Per-DUT garbled-detection switches shared with `DeviceService`.
    /// `None` = the session never enables detection (demo/tests).
    garble_enabled: Option<Arc<Mutex<HashMap<String, bool>>>>,
    /// Raw-byte garble meter (baud-rate mismatch heuristic; RX path only).
    garble: Mutex<crate::garbled::GarbleMeter>,
}

impl SessionCtx {
    pub async fn begin_actor_io(&self) -> Option<tokio::sync::RwLockReadGuard<'_, ()>> {
        match self.lease_authority.as_ref() {
            Some(authority) => Some(authority.serial_io().read().await),
            None => None,
        }
    }
    pub fn new(dut_id: &str, session_id: &str, bus: Arc<EventBus>) -> Self {
        Self {
            dut_id: dut_id.to_string(),
            session_id: session_id.to_string(),
            bus,
            logs: None,
            log_tx: None,
            log_finished: None,
            reconnect_notify: None,
            lease_authority: None,
            audit: None,
            garble_enabled: None,
            garble: Mutex::new(crate::garbled::GarbleMeter::default()),
        }
    }

    /// 挂接每 DUT 乱码检测开关（与 `DeviceService` 共享同一 map：运行中切换
    /// 对活动会话即时生效）。
    pub fn with_garble(mut self, map: Arc<Mutex<HashMap<String, bool>>>) -> Self {
        self.garble_enabled = Some(map);
        self
    }

    /// Reads the per-DUT switch. The map lock is always released before the
    /// meter lock is taken (single lock order, no nesting).
    fn garble_on(&self) -> bool {
        let Some(map) = &self.garble_enabled else {
            return false;
        };
        match map.lock() {
            Ok(guard) => guard.get(&self.dut_id).copied().unwrap_or(false),
            // A poisoned switch map means a writer panicked; fail closed.
            Err(_) => false,
        }
    }

    fn with_garble_meter<R>(
        &self,
        update: impl FnOnce(&mut crate::garbled::GarbleMeter) -> R,
    ) -> R {
        match self.garble.lock() {
            Ok(mut meter) => update(&mut meter),
            // A poisoned meter must not break the RX path; recover and keep
            // counting instead of panicking on every block.
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    /// Current garble statistics for the next snapshot (also applies a runtime
    /// switch flip: turning detection off resets the meter to `Clean`).
    pub fn garble_stats(&self) -> duthub_core::ipc::GarbledStats {
        let enabled = self.garble_on();
        self.with_garble_meter(|meter| {
            meter.set_enabled(enabled);
            meter.stats()
        })
    }

    /// Clears the detection window (terminal reset / baud-rate change).
    pub fn reset_garble(&self) {
        self.with_garble_meter(crate::garbled::GarbleMeter::reset);
    }

    /// 挂接日志管道（原始字节副本 + utf8-lossy 文本，M1.5）。
    /// 同时启动专用写线程：逐条 write + flush 语义与同步路径完全一致
    /// （实时落盘、零丢失），仅把磁盘 I/O 移出 RX 回显关键路径。
    pub fn with_logs(mut self, logs: Arc<Mutex<SessionLogWriter>>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<LogMsg>();
        let finished = Arc::new((Mutex::new(false), Condvar::new()));
        let spawned = std::thread::Builder::new()
            .name(format!("serial-log-{}", self.session_id))
            .spawn({
                let logs_for_thread = Arc::clone(&logs);
                let session_id = self.session_id.clone();
                let dut_id = self.dut_id.clone();
                let finished = Arc::clone(&finished);
                move || run_log_writer(logs_for_thread, rx, session_id, dut_id, finished)
            });
        if spawned.is_ok() {
            self.log_tx = Some(tx);
            self.log_finished = Some(finished);
        }
        self.logs = Some(logs);
        self
    }

    /// 写线程的发送端克隆（供电分段等控制边界同步落盘时用）。
    pub(crate) fn log_sender(&self) -> Option<std::sync::mpsc::Sender<LogMsg>> {
        self.log_tx.clone()
    }

    /// 入队一条日志消息。写线程不可用时内联落盘兜底（保证零丢失）。
    fn enqueue_log(&self, msg: LogMsg) {
        let msg = match &self.log_tx {
            Some(tx) => match tx.send(msg) {
                Ok(()) => return,
                Err(std::sync::mpsc::SendError(undelivered)) => undelivered,
            },
            None => msg,
        };
        if let Some(logs) = &self.logs {
            apply_log_msg(logs, msg, |error| {
                tracing::warn!(
                    session_id = %self.session_id, dut_id = %self.dut_id,
                    %error, "Serial log write failed"
                );
            });
        }
    }

    /// 排空屏障：阻塞直到此前入队的日志全部落盘（测试与排空等待用）。
    pub fn flush_logs(&self) {
        if let Some(tx) = &self.log_tx {
            let (ack, rx) = std::sync::mpsc::channel();
            if tx.send(LogMsg::Flush(ack)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_secs(3));
            }
        }
    }

    /// 挂接手动重连唤醒（H1.4：退避等待期间可被提前唤醒）。
    pub fn with_reconnect_notify(mut self, notify: std::sync::Arc<Notify>) -> Self {
        self.reconnect_notify = Some(notify);
        self
    }

    /// 挂接租约复核（[E] actor 复核：SPEC §6.5.1 唯一 Actor 写前验证代数）。
    pub fn with_lease_authority(mut self, authority: Option<Arc<dyn LeaseAuthority>>) -> Self {
        self.lease_authority = authority;
        self
    }

    /// 挂接审计接收端。
    pub fn with_audit(mut self, audit: Option<Arc<AuditSink>>) -> Self {
        self.audit = audit;
        self
    }

    /// [E] actor 复核：命令携带租约上下文时，写串口前最后一刻验证。
    /// 失败 → 注记行（Error 样式）+ 审计 write.deny + 返回 false（调用方丢弃写入）。
    pub fn admit_actor_write(&self, lease: &Option<WriteCtx>) -> bool {
        let Some(ctx) = lease else {
            return true; // 无租约要求路径（既有测试 / 过渡放行）
        };
        let Some(authority) = &self.lease_authority else {
            return true;
        };
        match authority.validate_ctx(&self.dut_id, ctx) {
            Ok(()) => true,
            Err(err) => {
                let note = format!("[lease] 过期写入已拦截：{}", err.message());
                self.emit_text(&note, ChunkStyle::Error);
                if let Some(audit) = &self.audit {
                    audit.record(
                        "write.deny",
                        None,
                        None,
                        Some(&self.dut_id),
                        AuditOutcome::Denied,
                        Some(&format!(
                            "actor 复核拒绝 lease={} gen={}：{}",
                            ctx.lease_id,
                            ctx.generation,
                            err.message()
                        )),
                    );
                }
                false
            }
        }
    }

    fn write_log(&self, line: &str) {
        self.enqueue_log(LogMsg::Note(line.to_string()));
    }

    pub fn emit_text(&self, text: &str, style: ChunkStyle) -> EventEnvelope {
        self.write_log(text);
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(text, style),
        )
    }

    pub fn emit_disconnected(&self, reason: &str) -> EventEnvelope {
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            UiEvent::SerialDisconnected {
                reason: reason.to_string(),
            },
        )
    }

    /// 发送回显（H1.1，demo 驱动专用）：TX 标记行进终端流（Good 配色区分收发）；
    /// .bin 原始日志记真实线上字节（含行尾），.log 文本记标记行。
    pub fn emit_tx(&self, wire_payload: &str, display_text: &str) {
        let display = single_line_tx(display_text);
        self.enqueue_log(LogMsg::Tx {
            wire: wire_payload.as_bytes().to_vec(),
            text: format!("[TX] {display}"),
        });
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(format!("[TX] {display}"), ChunkStyle::Good),
        );
    }

    /// TX 仅审计（real 驱动 write_line 路径）：.bin 记真实线上字节、.log 记
    /// `[TX]` 标记行，但不再进终端流/总线（PuTTY 行为：无本地回显，屏幕内容
    /// 由设备 shell 经仿真器渲染）。
    pub fn log_tx(&self, wire_payload: &[u8], display_text: &str) {
        let display = single_line_tx(display_text);
        self.enqueue_log(LogMsg::Tx {
            wire: wire_payload.to_vec(),
            text: format!("[TX] {display}"),
        });
    }

    /// RX 原始字节块：.bin 记真字节（不转码），.log 记 utf8-lossy 文本，总线发
    /// SerialChunk（MCP serial_read / 录制 / 审计的文本来源；终端 UI 渲染走
    /// SerialScreen 网格快照，不重复显示该文本）。
    pub fn emit_raw(&self, bytes: &[u8]) {
        // Garble detection samples the raw bytes before any lossy decoding and
        // before the terminal emulator (which swallows most control bytes).
        // Meter state only: no disk I/O and no extra bus event here.
        let garble_on = self.garble_on();
        self.with_garble_meter(|meter| {
            meter.set_enabled(garble_on);
            meter.observe(bytes);
        });
        let text = String::from_utf8_lossy(bytes).to_string();
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(text.clone(), ChunkStyle::Normal),
        );
        self.enqueue_log(LogMsg::Raw {
            bytes: bytes.to_vec(),
            text,
        });
    }

    /// 键入字节审计（real 驱动 WriteBytes 路径）：只进 .bin 原始流；.log 与终端流
    /// 由随后的设备回显承担（避免逐键 [TX] 刷屏）。
    pub fn log_keystrokes(&self, bytes: &[u8]) {
        self.enqueue_log(LogMsg::Keystrokes(bytes.to_vec()));
    }

    /// 终端网格快照 → 总线（real 驱动专用；UI 整体替换渲染源）。
    /// 收所有权（快照是新建值，无共享需求），避免事件侧深拷贝。
    /// The garble statistics are stamped here so every snapshot carries the
    /// latest raw-byte verdict without changing the `term` snapshot builder.
    pub fn emit_screen(&self, mut screen: duthub_core::ipc::ScreenSnapshot) {
        screen.garbled = self.garble_stats();
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            UiEvent::SerialScreen {
                screen: std::sync::Arc::new(screen),
            },
        );
    }

    /// Queue a storage-only log boundary.
    pub fn emit_mark(&self, label: &str) {
        self.enqueue_log(LogMsg::Mark(format!("[mark] {label}")));
    }

    /// 断线重连退避等待（H1.4）：定时到期或手动重连通知（先到先返）。
    pub async fn reconnect_wait(&self, backoff: Duration) {
        match &self.reconnect_notify {
            Some(notify) => {
                let notified = notify.notified();
                tokio::pin!(notified);
                tokio::select! {
                    _ = &mut notified => {}
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
            None => tokio::time::sleep(backoff).await,
        }
    }
}

impl Drop for SessionCtx {
    fn drop(&mut self) {
        // 关闭日志通道：写线程把残余消息全部落盘后才退出（recv Err），
        // 会话结束 = .log/.bin 完整收尾，一字节不丢。
        self.log_tx = None;
        if let Some(finished) = &self.log_finished {
            let (lock, cvar) = &**finished;
            let guard = lock.lock().unwrap();
            let _ = cvar
                .wait_timeout_while(guard, Duration::from_secs(3), |done| !*done)
                .unwrap();
        }
    }
}

/// [TX] 注记单行化（P2-034）：display_text 中的换行转义为字面 `\r` / `\n`
/// 两个字符，保证 .log 与终端流不出现无前缀续行——按行解析 .log 的下游工具
/// 不会把多行写入的后续行误读为设备输出。wire 原始字节不经此处理（.bin 保留
/// 真实线上字节）。
fn single_line_tx(text: &str) -> String {
    text.replace('\r', "\\r").replace('\n', "\\n")
}

/// 内核行分类：错误行（panic/oops/BUG/failed）标红、prompt 标亮，其余常规。
pub fn classify_line(text: &str) -> ChunkStyle {
    let lower = text.to_ascii_lowercase();
    if lower.contains("panic")
        || lower.contains("oops")
        || lower.contains("bug:")
        || lower.contains("call trace")
        || lower.contains("error")
        || lower.contains("failed")
    {
        return ChunkStyle::Error;
    }
    let trimmed_end = text.trim_end();
    if trimmed_end.ends_with('#') || trimmed_end.ends_with('$') || trimmed_end.ends_with("login:") {
        return ChunkStyle::Prompt;
    }
    ChunkStyle::Normal
}

// NOTE：手写 LineAssembler（\n 分行 + 半行冲刷）已随终端仿真重构移除 ——
// real 会话字节流直接进 alacritty_terminal 仿真器，回显/提示符/光标由设备
// shell 负责（PuTTY 行为），文本流按读取块原样进日志管道。

// ---------------------------------------------------------------------------
// Demo 驱动
// ---------------------------------------------------------------------------

const BOOT_FIXTURE: &str = include_str!("../../../tests/fixture-logs/rk3576-boot.log");
const PANIC_FIXTURE: &str = include_str!("../../../tests/fixture-logs/kernel-panic.log");

/// fixture 文本内容（panic 样本供测试与“模拟 panic”命令使用）。
pub fn boot_fixture() -> Vec<String> {
    BOOT_FIXTURE.lines().map(str::to_string).collect()
}

pub fn panic_fixture() -> Vec<String> {
    PANIC_FIXTURE.lines().map(str::to_string).collect()
}

/// Demo 驱动：回放启动日志 → 交互 shell → Close/取消退出。
pub async fn run_demo_session(
    ctx: SessionCtx,
    mut cmd_rx: mpsc::Receiver<SessionCommand>,
    line_delay_ms: u64,
) {
    let mut ending = LineEnding::default();
    for line in boot_fixture() {
        ctx.emit_text(&line, classify_line(&line));
        tokio::time::sleep(std::time::Duration::from_millis(line_delay_ms)).await;
    }
    ctx.emit_text("RK3576 login: root", ChunkStyle::Normal);
    ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCommand::Write { text, lease } => {
                // [E] actor 复核（SPEC §6.5.1）：写前最后一刻验证租约代数；
                // 过期/被接管 → 丢弃写入（注记 + 审计），不触串口
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                // 发送回显与真实驱动一致：Good 配色 + [TX] 标记（演示 shell 的
                // 提示符行随后模拟设备侧本地回显）
                ctx.emit_tx(&ending.apply(&text), &text);
                let command = text.trim();
                if command.is_empty() {
                    ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
                    continue;
                }
                ctx.emit_text(&format!("root@rk3576:~# {command}"), ChunkStyle::Prompt);
                emit_demo_response(&ctx, command);
                ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
            }
            SessionCommand::WriteHeartbeatBatch { text, batch, lease } => {
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                ctx.emit_tx(&ending.apply(&text), &text);
                ctx.emit_text(&batch.begin_marker(), ChunkStyle::Normal);
                for command in batch
                    .script
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                {
                    emit_demo_response(&ctx, command);
                }
                ctx.emit_text(&batch.end_marker(), ChunkStyle::Normal);
            }
            SessionCommand::SetBaud(_) => {
                ctx.emit_text("(demo) 波特率变更已模拟", ChunkStyle::Dim);
            }
            SessionCommand::Mark(label) => {
                ctx.emit_mark(&label);
            }
            SessionCommand::SetEnding(new_ending) => {
                ending = new_ending;
                ctx.emit_text(
                    &format!("(demo) 行尾策略 → {}", new_ending.label()),
                    ChunkStyle::Dim,
                );
            }
            SessionCommand::WriteBytes { bytes, lease } => {
                // [E] actor 复核（同 Write 分支）
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                // demo 无仿真器：按键字节按 utf8-lossy 走通用回显路径（行为可见即可）
                let command = String::from_utf8_lossy(&bytes).trim().to_string();
                if !command.is_empty() {
                    ctx.emit_text(&format!("Executed (demo): {command}"), ChunkStyle::Normal);
                }
                ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
            }
            SessionCommand::Resize { .. }
            | SessionCommand::ResetTerminal
            | SessionCommand::ReadTerminalHistory { .. } => {
                // demo 无终端网格，忽略
            }
            SessionCommand::Close => break,
        }
    }
    ctx.emit_disconnected("session closed");
}

fn emit_demo_response(ctx: &SessionCtx, command: &str) {
    match command {
        "pwd" => {
            ctx.emit_text("/root", ChunkStyle::Normal);
        }
        "whoami" => {
            ctx.emit_text("root", ChunkStyle::Normal);
        }
        "uname -a" => {
            ctx.emit_text(
                "Linux rk3576 6.1.118 #1 SMP PREEMPT aarch64 GNU/Linux",
                ChunkStyle::Normal,
            );
        }
        "ls" => {
            ctx.emit_text(
                "bin  boot  dev  etc  home  lib  media  mnt  opt  proc  root  run  sys  tmp  usr  var",
                ChunkStyle::Normal,
            );
        }
        "uptime" => {
            ctx.emit_text(
                " 00:12:40 up 4 days,  2 users,  load average: 0.05, 0.07, 0.06",
                ChunkStyle::Normal,
            );
        }
        c if c.starts_with("dmesg") => {
            for line in panic_fixture() {
                ctx.emit_text(&line, classify_line(&line));
            }
        }
        "clear" => {
            // UI 端 clear 在 M1.6 由本地处理；demo 只给提示。
            ctx.emit_text("(demo) 屏幕已清空（本地终端处理 clear）", ChunkStyle::Dim);
        }
        "help" => {
            ctx.emit_text(
                "Demo commands: uname -a, whoami, pwd, ls, uptime, dmesg, clear",
                ChunkStyle::Dim,
            );
        }
        "reboot" => {
            ctx.emit_text("Reboot requested (simulation only)", ChunkStyle::Normal);
        }
        other => {
            ctx.emit_text(&format!("Executed (demo): {other}"), ChunkStyle::Normal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_flags_kernel_errors() {
        assert_eq!(
            classify_line("Kernel panic - not syncing: Fatal"),
            ChunkStyle::Error
        );
        assert_eq!(classify_line("[   42.90]Oops: 96000047"), ChunkStyle::Error);
        assert_eq!(
            classify_line("[   3.512] systemd[1]: Started Serial Getty."),
            ChunkStyle::Normal
        );
        assert_eq!(classify_line("root@rk3576:~# "), ChunkStyle::Prompt);
    }

    #[tokio::test]
    async fn demo_session_replays_boot_and_answers_uname() {
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let ctx = SessionCtx::new("dut-demo", "sess-demo", bus.clone());
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        // 等启动日志播完
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        tx.send(SessionCommand::Write {
            text: "uname -a\n".into(),
            lease: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let log: Vec<String> = bus
            .read_log("sess-demo", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let boot_len = boot_fixture().len();
        assert!(
            log.len() >= boot_len + 3,
            "启动行 + 输入回显 + 响应 + 提示符"
        );
        assert!(log.iter().any(|l| l.contains("aarch64 GNU/Linux")));
        assert!(log.iter().any(|l| l.ends_with("root@rk3576:~# uname -a")));
        drop(tx);
        let _ = handle.await;
        // 关闭后最后一条应是 Disconnected，且不再增长
        let after_close = bus.last_seq("sess-demo");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(after_close, bus.last_seq("sess-demo"), "退出后不应再有事件");
    }

    #[tokio::test]
    async fn write_produces_tx_echo_and_setending_takes_effect() {
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let ctx = SessionCtx::new("dut-tx", "sess-tx", bus.clone());
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        tx.send(SessionCommand::Write {
            text: "echo-a".into(),
            lease: None,
        })
        .await
        .unwrap();
        tx.send(SessionCommand::SetEnding(LineEnding::Crlf))
            .await
            .unwrap();
        tx.send(SessionCommand::Write {
            text: "echo-b".into(),
            lease: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let lines: Vec<String> = bus
            .read_log("sess-tx", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|l| l == "[TX] echo-a"),
            "写入必须有 TX 回显行，实际 {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "[TX] echo-b"),
            "SetEnding 后的写入仍要有 TX 回显"
        );
        drop(tx);
        let _ = handle.await;
    }

    /// [E] actor 复核（SPEC §6.5.1）：命令携带过期/不存在的租约上下文 →
    /// 写入在串口前丢弃 + 注记行，绝不产生 TX 回显（INV-04 actor 侧）。
    #[tokio::test]
    async fn actor_recheck_drops_stale_generation_write() {
        use duthub_core::audit::AuditSink;
        use duthub_core::lease::{ControlLeaseManager, WriteCtx};
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let authority: Arc<dyn LeaseAuthority> =
            Arc::new(ControlLeaseManager::new(Some(AuditSink::memory())));
        let audit = AuditSink::memory();
        let ctx = SessionCtx::new("dut-e", "sess-e", bus.clone())
            .with_lease_authority(Some(authority.clone()))
            .with_audit(Some(audit.clone()));
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        // 无人持有该租约（管理器内不存在）：句柄任意 → validate 拒 → 丢弃
        let stale = Some(WriteCtx {
            lease_id: "lease-stale".into(),
            generation: 7,
        });
        tx.send(SessionCommand::Write {
            text: "stale-cmd".into(),
            lease: stale,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let lines: Vec<String> = bus
            .read_log("sess-e", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|l| l.contains("过期写入已拦截")),
            "必须有拦截注记行：{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l == "[TX] stale-cmd"),
            "过期写入不得触串口/产生 TX 回显：{lines:?}"
        );
        // 审计 write.deny
        let denied = audit
            .recent(50)
            .iter()
            .any(|e| e.action == "write.deny" && e.dut.as_deref() == Some("dut-e"));
        assert!(denied, "actor 拒绝必须落审计");
        drop(tx);
        let _ = handle.await;
    }

    #[test]
    fn fixtures_contain_panic_evidence() {
        let panic = panic_fixture().join("\n");
        assert!(panic.contains("Kernel panic - not syncing"));
        assert!(panic.contains("Call trace:"));
        assert!(
            boot_fixture()
                .iter()
                .any(|l| l.contains("Started Serial Getty"))
        );
    }

    /// P2-034b：[TX] 注记单行化——emit_tx（终端流）与 log_tx（.log 审计流）
    /// 把 display_text 的换行转义为字面 `\n` / `\r`，不出现无前缀续行；
    /// wire 原始字节（.bin）保留真实线上字节。
    #[test]
    fn tx_annotation_is_single_line_for_multiline_text() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/tx-single-line")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-txml").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-txml", "sess-txml", bus.clone()).with_logs(logs);
        let expected = "[TX] echo C1\\necho C2";
        // emit_tx：终端流（总线）必须单行
        ctx.emit_tx("echo C1\necho C2\n", "echo C1\necho C2");
        let chunks: Vec<String> = bus
            .read_log("sess-txml", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            chunks.iter().any(|t| t == expected),
            "[TX] 总线注记必须单行（换行转义）：{chunks:?}"
        );
        // log_tx：.log 审计流同样单行
        ctx.log_tx(b"echo C1\recho C2\r\n", "echo C1\r\necho C2");
        ctx.flush_logs();
        let text = std::fs::read_to_string(dir.join("sess-txml.log")).unwrap();
        assert!(
            text.lines().any(|line| line == "[TX] echo C1\\r\\necho C2"),
            ".log 必须有单行 [TX] 注记：{text:?}"
        );
        assert!(
            !text.lines().any(|line| line == "echo C2"),
            ".log 不得出现 [TX] 的无前缀续行：{text:?}"
        );
        // .bin 保留真实线上字节（不经转义）
        let raw = std::fs::read(dir.join("sess-txml.bin")).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&raw),
            "echo C1\necho C2\necho C1\recho C2\r\n"
        );
    }

    /// 日志写线程按入队顺序落盘（零丢失红线）：RX/TX/键入/注记/分段全部
    /// 异步化后，.log 与 .bin 的字节顺序必须与调用顺序一致。
    #[test]
    fn log_writer_preserves_enqueue_order_and_drains() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log-order")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-logorder").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-lo", "sess-logorder", bus.clone()).with_logs(logs);
        ctx.emit_raw(b"rx-1");
        ctx.log_tx(b"tx-1", "tx-1");
        ctx.log_keystrokes(b"key-1");
        ctx.emit_text("[note] hello", ChunkStyle::Dim);
        ctx.emit_mark("seg-a");
        ctx.emit_raw(b"rx-2");
        ctx.flush_logs();

        let text = std::fs::read_to_string(dir.join("sess-logorder.log")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let pos = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!(".log 缺少 {needle}：{text:?}"))
        };
        assert_eq!(pos("rx-1"), 0, "RX 文本在最前");
        assert!(pos("[TX] tx-1") > pos("rx-1"));
        assert!(pos("[note] hello") > pos("[TX] tx-1"));
        assert!(pos("[mark] seg-a") > pos("[note] hello"));
        // 分段（mark）把后续输出切进新段文件：rx-2 必须出现在任一段文件里
        let seg_files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.to_string_lossy();
                name.contains("sess-logorder") && name.contains(".log")
            })
            .collect();
        assert!(
            seg_files.iter().any(|p| {
                std::fs::read_to_string(p)
                    .unwrap_or_default()
                    .contains("rx-2")
            }),
            "分段后的 RX 必须落在新段：{:?}",
            seg_files
        );
        let raw = std::fs::read(dir.join("sess-logorder.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        assert!(raw.contains("rx-1tx-1key-1"), "字节顺序：{raw:?}");
    }

    /// Drop 排空：不调用 flush 直接销毁 SessionCtx，写线程必须把残余消息
    /// 全部落盘后才退出（会话关闭 = 日志完整收尾，一字节不丢）。
    #[test]
    fn ctx_drop_drains_pending_log_messages() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log-drop")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-logdrop").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-ld", "sess-logdrop", bus).with_logs(logs);
        for i in 0..64 {
            ctx.emit_raw(format!("drop-{i}").as_bytes());
        }
        drop(ctx); // 不 flush，直接销毁
        let text = std::fs::read_to_string(dir.join("sess-logdrop.log")).unwrap();
        assert!(text.contains("drop-63"), "最后一条必须在：{text:?}");
        assert!(text.contains("drop-0"), "第一条必须在：{text:?}");
    }

    // ---- TX 合并（absorb_keystrokes）----

    fn push_writes(
        tx: &tokio::sync::mpsc::Sender<SessionCommand>,
        lease: Option<WriteCtx>,
        payloads: &[&[u8]],
    ) {
        for p in payloads {
            tx.try_send(SessionCommand::WriteBytes {
                bytes: p.to_vec(),
                lease: lease.clone(),
            })
            .unwrap();
        }
    }

    /// 同租约连续键入合并为一个批次，字节按入队顺序拼接，
    /// 每条原始命令保留独立元素（键入审计按原边界落盘）。
    #[test]
    fn consecutive_same_lease_writes_merge_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-1".into(),
            generation: 3,
        });
        push_writes(&tx, lease.clone(), &[b"a", b"bc", b"def"]);
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"x".to_vec(), &lease);
        assert!(deferred.is_none(), "无异型命令残留");
        assert_eq!(batch.concat(), b"xabcdef".to_vec(), "顺序必须保持");
        assert_eq!(batch.len(), 4, "逐条原始边界保留");
    }

    /// 异租约键入不合并；异型命令立即截断并交回。
    #[test]
    fn foreign_lease_and_other_commands_are_deferred() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let gui = Some(WriteCtx {
            lease_id: "gui-1".into(),
            generation: 1,
        });
        let agent = Some(WriteCtx {
            lease_id: "agent-9".into(),
            generation: 7,
        });
        push_writes(&tx, agent.clone(), &[b"agent"]);
        tx.try_send(SessionCommand::Resize { cols: 80, rows: 24 })
            .unwrap();
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"gui".to_vec(), &gui);
        assert_eq!(batch, vec![b"gui".to_vec()], "异租约不得并入");
        let Some(SessionCommand::WriteBytes { bytes, lease }) = deferred else {
            panic!("异租约键入必须交回队列");
        };
        assert_eq!(bytes, b"agent".to_vec());
        assert_eq!(lease, agent);
        // 再吸收一次：Resize 必须原样交回
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"z".to_vec(), &agent);
        assert_eq!(batch, vec![b"z".to_vec()], "同租约但队列中已无键入");
        let Some(SessionCommand::Resize { cols, rows }) = deferred else {
            panic!("Resize 必须原样交回");
        };
        assert_eq!((cols, rows), (80, 24));
    }

    /// 超过合并上限即截断：超限的键入整条留给下一轮，不拆分。
    #[test]
    fn merge_stops_at_byte_cap() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-1".into(),
            generation: 2,
        });
        let big = vec![b'k'; 200];
        let big2 = vec![b'm'; 100];
        tx.try_send(SessionCommand::WriteBytes {
            bytes: big2.clone(),
            lease: lease.clone(),
        })
        .unwrap();
        let (batch, deferred) = absorb_keystrokes(&mut rx, big, &lease);
        assert_eq!(batch.iter().map(|b| b.len()).sum::<usize>(), 200);
        let Some(SessionCommand::WriteBytes { bytes, lease: l }) = deferred else {
            panic!("超限键入必须整条留给下一轮");
        };
        assert_eq!(bytes, big2, "超限键入不拆分");
        assert_eq!(l, lease);
    }

    /// 无租约路径（lease = None）与空队列：只返回首条。
    #[test]
    fn empty_queue_returns_first_only() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        drop(tx);
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"only".to_vec(), &None);
        assert_eq!(batch, vec![b"only".to_vec()]);
        assert!(deferred.is_none());
    }

    // ---- TX 合并边界（PERF-1：cap 两侧的运行时行为）----

    /// PERF-1 边界①：100 + 156 = 256 恰好触顶——`<=` 允许恰好填满上限的那条
    /// 并入，触顶后循环条件退出，后续同租约键入整条留在队列给下一轮，既不
    /// 拆分也不丢失（不改 absorb 的进退规则，cap 语义即可回归）。
    #[test]
    fn merge_accumulates_to_exact_cap_and_leaves_next_queued() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-cap".into(),
            generation: 1,
        });
        let hundred = vec![b'a'; 100];
        let one_fiftysix = vec![b'b'; 156];
        let ten = vec![b'c'; 10];
        push_writes(
            &tx,
            lease.clone(),
            &[one_fiftysix.as_slice(), ten.as_slice()],
        );
        let (batch, deferred) = absorb_keystrokes(&mut rx, hundred, &lease);
        assert_eq!(batch.len(), 2, "恰好触顶的那条必须并入");
        assert_eq!(
            batch.iter().map(|b| b.len()).sum::<usize>(),
            TX_MERGE_BYTES,
            "合并后恰好等于上限"
        );
        assert!(deferred.is_none(), "触顶退出时不得拉出后续命令");
        let SessionCommand::WriteBytes { bytes, lease: l } = rx.try_recv().unwrap() else {
            panic!("队列剩余的应是键入命令");
        };
        assert_eq!(bytes, ten, "超限键入整条留在队列（不拆分）");
        assert_eq!(l, lease);
        assert!(rx.try_recv().is_err(), "队列应已排空");
    }

    /// PERF-1 边界②：首条键入恰好等于上限——不再吸收任何命令（连同租约的
    /// 也留在队列），批次只含首条且整条不拆分。
    #[test]
    fn first_command_at_cap_merges_nothing() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-cap2".into(),
            generation: 1,
        });
        let at_cap = vec![b'x'; TX_MERGE_BYTES];
        let small = vec![b'y'; 5];
        push_writes(&tx, lease.clone(), &[small.as_slice()]);
        let (batch, deferred) = absorb_keystrokes(&mut rx, at_cap.clone(), &lease);
        assert_eq!(batch, vec![at_cap], "已达上限：批次只含首条，整条不拆分");
        assert!(deferred.is_none());
        let SessionCommand::WriteBytes { bytes, lease: l } = rx.try_recv().unwrap() else {
            panic!("同租约键入应整条留在队列等下一轮");
        };
        assert_eq!(bytes, small);
        assert_eq!(l, lease);
    }

    /// PERF-1 持续突发：10 × 60B 同租约（600B > 256B cap）分 3 轮吸收
    /// （4+4+2），每轮 ≤ cap、每条原始命令整条保留；deferred 原样退回后作为
    /// 下一轮 first——三轮拼接与入队顺序严格一致，一字节不丢、不重排。
    #[test]
    fn sustained_burst_merges_in_order_without_splitting_commands() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-burst".into(),
            generation: 4,
        });
        let cmds: Vec<Vec<u8>> = (0..10u8).map(|i| vec![b'0' + i; 60]).collect();
        let rest: Vec<&[u8]> = cmds[1..].iter().map(|c| c.as_slice()).collect();
        push_writes(&tx, lease.clone(), &rest);
        let (b1, d1) = absorb_keystrokes(&mut rx, cmds[0].clone(), &lease);
        let Some(SessionCommand::WriteBytes { bytes, lease: l }) = d1 else {
            panic!("第 5 条必须整条退回");
        };
        assert_eq!(bytes, cmds[4]);
        assert_eq!(l, lease);
        let (b2, d2) = absorb_keystrokes(&mut rx, bytes, &lease);
        let Some(SessionCommand::WriteBytes { bytes, lease: l }) = d2 else {
            panic!("第 9 条必须整条退回");
        };
        assert_eq!(bytes, cmds[8]);
        assert_eq!(l, lease);
        let (b3, d3) = absorb_keystrokes(&mut rx, bytes, &lease);
        assert!(d3.is_none(), "突发应全部消化");
        for batch in [&b1, &b2, &b3] {
            assert!(
                batch.iter().map(|b| b.len()).sum::<usize>() <= TX_MERGE_BYTES,
                "每轮批次不得超过合并上限"
            );
        }
        assert_eq!(b1.len(), 4);
        assert_eq!(b2.len(), 4);
        assert_eq!(b3.len(), 2);
        let merged: Vec<&Vec<u8>> = b1.iter().chain(b2.iter()).chain(b3.iter()).collect();
        let expected: Vec<&Vec<u8>> = cmds.iter().collect();
        assert_eq!(merged, expected, "跨轮合并不得重排或拆分任何命令");
    }

    /// PERF-1 异租约截断：gui 突发中间混入 agent 键入——吸收立即截断（agent
    /// 之后的同租约键入不得被跳过提前合并），异租约命令原样交回；按退回顺序
    /// 逐轮执行后全局顺序与入队顺序一致。
    #[test]
    fn interleaved_lease_burst_truncates_at_foreign_command() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let gui = Some(WriteCtx {
            lease_id: "gui".into(),
            generation: 1,
        });
        let agent = Some(WriteCtx {
            lease_id: "agent".into(),
            generation: 2,
        });
        let c0 = vec![b'a'; 50];
        let c1 = vec![b'b'; 50];
        let c2 = vec![b'c'; 50];
        let c3 = vec![b'd'; 50];
        push_writes(&tx, agent.clone(), &[c1.as_slice()]);
        push_writes(&tx, gui.clone(), &[c2.as_slice(), c3.as_slice()]);
        // 轮 1：c0(gui) 后紧跟异租约 c1 → 立即截断：批次只含首条，c1 原样交回
        // （若 absorb 跳过异租约继续吸收，b1 会变成 [c0, c2, c3]）
        let (b1, d1) = absorb_keystrokes(&mut rx, c0.clone(), &gui);
        assert_eq!(b1, vec![c0.clone()], "异租约前的批次只含首条");
        let Some(SessionCommand::WriteBytes {
            bytes: deferred1,
            lease: l1,
        }) = d1
        else {
            panic!("异租约键入必须原样交回");
        };
        assert_eq!(deferred1, c1);
        assert_eq!(l1, agent);
        // 轮 2：退回的 c1(agent) 后是 gui 的 c2 → 再次截断。deferred2 == c2
        // 同时证明轮 1 未跳过/丢弃异租约后面的同租约键入（c2 仍在 c3 之前）。
        let (b2, d2) = absorb_keystrokes(&mut rx, deferred1, &agent);
        assert_eq!(b2, vec![c1.clone()]);
        let Some(SessionCommand::WriteBytes {
            bytes: deferred2,
            lease: l2,
        }) = d2
        else {
            panic!("异租约键入必须原样交回");
        };
        assert_eq!(deferred2, c2);
        assert_eq!(l2, gui);
        // 轮 3：c2 与 c3 同租约 → 合并，突发全部消化
        let (b3, d3) = absorb_keystrokes(&mut rx, deferred2, &gui);
        assert!(d3.is_none());
        assert_eq!(b3, vec![c2.clone(), c3.clone()]);
    }

    // ---- RX 日志管道（PERF-2：总线同步发布 + 磁盘 I/O 只在写线程）----

    /// PERF-2：emit_raw 必须先同步把 SerialChunk 发上总线（MCP serial_read /
    /// 审计源不变），且调用路径零磁盘 I/O。用「测试线程持锁卡住写线程」的
    /// 手法确定性证明（代替会闪断的时限断言）：写线程被挡住期间 .log/.bin
    /// 必然为空——若有同步写盘，文件早就非空；放锁 + 排空屏障后逐条落齐。
    #[test]
    fn emit_raw_publishes_to_bus_synchronously_and_defers_disk_to_writer() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/perf2-bus-sync")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-busio").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-busio", "sess-busio", bus.clone()).with_logs(logs.clone());
        // ① 持有日志互斥量：写线程收到消息后会卡在 lock 上，期间磁盘必然无写入
        let guard = logs.lock().unwrap();
        ctx.emit_raw(b"rx-bus-1");
        ctx.emit_raw(b"rx-bus-2");
        ctx.log_tx(b"tx-bus", "tx-bus");
        // ② emit_raw 返回即能在总线读到（同步发布，MCP serial_read 源不变）
        let chunks: Vec<String> = bus
            .read_log("sess-busio", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            chunks,
            vec!["rx-bus-1".to_string(), "rx-bus-2".to_string()],
            "RX 块必须同步上总线"
        );
        // ③ 写线程被锁挡住 → 两个日志文件必须仍为空：调用线程没碰过磁盘
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-busio.log")).unwrap(),
            "",
            "RX 调用路径零磁盘 I/O：写线程未放行前 .log 必须为空"
        );
        assert!(
            std::fs::read(dir.join("sess-busio.bin"))
                .unwrap()
                .is_empty(),
            "RX 调用路径零磁盘 I/O：写线程未放行前 .bin 必须为空"
        );
        drop(guard);
        // ④ 排空屏障 ack = 此前入队消息全部按旧同步语义逐条 write+flush 落盘
        ctx.flush_logs();
        let text = std::fs::read_to_string(dir.join("sess-busio.log")).unwrap();
        assert_eq!(
            text, "rx-bus-1\nrx-bus-2\n[TX] tx-bus\n",
            "放锁后写线程必须按入队顺序落齐"
        );
        assert_eq!(
            String::from_utf8_lossy(&std::fs::read(dir.join("sess-busio.bin")).unwrap()),
            "rx-bus-1rx-bus-2tx-bus",
            ".bin 字节顺序与入队顺序一致"
        );
    }

    // ---- 会话关闭收尾（LOG-6：Drop 排空 + 字节级无损）----

    /// LOG-6：不 flush 直接 drop（会话关闭路径），写线程必须把残余消息全部
    /// 落盘后才退出（Drop 的 condvar 等待）——drop 返回后 .log/.bin 立即可读，
    /// 且 .bin 逐字节等于入队顺序拼接（RX/TX/键入/注记不重排、不丢一字节，
    /// 含多字节 UTF-8、\r\n 线上字节与退格 0x7f）。
    #[test]
    fn ctx_drop_drains_mixed_stream_byte_exact_in_order() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log6-drop-order")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-drop6").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-drop6", "sess-drop6", bus).with_logs(logs);
        ctx.emit_raw(b"R1");
        ctx.log_tx(b"T1", "T1");
        ctx.log_keystrokes(b"K1");
        ctx.emit_text("[note] N1", ChunkStyle::Dim);
        ctx.emit_raw("R2-中文".as_bytes());
        ctx.log_tx(b"T2\r\n", "T2");
        ctx.log_keystrokes(&[b'K', b'2', 0x7f]);
        ctx.emit_text("[note] N2", ChunkStyle::Dim);
        drop(ctx); // 不 flush，直接销毁：Drop 关通道 → 写线程排空后才退出
        let mut expected = Vec::new();
        expected.extend_from_slice(b"R1");
        expected.extend_from_slice(b"T1");
        expected.extend_from_slice(b"K1");
        expected.extend_from_slice(b"[note] N1");
        expected.extend_from_slice("R2-中文".as_bytes());
        expected.extend_from_slice(b"T2\r\n");
        expected.extend_from_slice(&[b'K', b'2', 0x7f]);
        expected.extend_from_slice(b"[note] N2");
        let raw = std::fs::read(dir.join("sess-drop6.bin")).unwrap();
        assert_eq!(
            raw, expected,
            ".bin 必须逐字节等于入队顺序拼接（一字节不丢、不重排）"
        );
        let text = std::fs::read_to_string(dir.join("sess-drop6.log")).unwrap();
        assert_eq!(
            text, "R1\n[TX] T1\n[note] N1\nR2-中文\n[TX] T2\n[note] N2\n",
            ".log 行序必须与入队顺序一致"
        );
    }

    /// LOG-7：`.rec` 结构化记录流与 .log/.bin 同步零丢失——同一混合流不 flush
    /// 直接 drop，排空后 .rec 按 kind 逐一对应入队顺序；Raw/Tx/Keystrokes 载荷
    /// 与 .bin 分段一致（多字节 UTF-8、\r\n、0x7f 原样），Note 行文本独立成
    /// 记录；zerocopy 零拷贝解析回读全程不丢不重排。
    #[test]
    fn ctx_drop_drains_rec_records_in_order() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log7-rec-order")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-drop7").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-drop7", "sess-drop7", bus).with_logs(logs);
        ctx.emit_raw(b"R1");
        ctx.log_tx(b"T1", "T1");
        ctx.log_keystrokes(b"K1");
        ctx.emit_text("[note] N1", ChunkStyle::Dim);
        ctx.emit_raw("R2-中文".as_bytes());
        ctx.log_tx(b"T2\r\n", "T2");
        ctx.log_keystrokes(&[b'K', b'2', 0x7f]);
        ctx.emit_text("[note] N2", ChunkStyle::Dim);
        ctx.emit_mark("供电边界");
        drop(ctx);
        let raw = std::fs::read(dir.join("sess-drop7.rec")).unwrap();
        let batch = crate::rec::parse_records(&raw);
        let kinds: Vec<crate::rec::RecKind> =
            batch.records.iter().map(|record| record.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecKind::Raw,
                RecKind::Tx,
                RecKind::Keystrokes,
                RecKind::Note,
                RecKind::Raw,
                RecKind::Tx,
                RecKind::Keystrokes,
                RecKind::Note,
                RecKind::Mark,
            ],
            ".rec 记录顺序必须与入队顺序一致"
        );
        let payloads: Vec<&[u8]> = batch.records.iter().map(|record| record.payload).collect();
        assert_eq!(
            payloads,
            vec![
                b"R1".as_slice(),
                b"T1".as_slice(),
                b"K1".as_slice(),
                b"[note] N1".as_slice(),
                "R2-中文".as_bytes(),
                b"T2\r\n".as_slice(),
                &[b'K', b'2', 0x7f].as_slice(),
                b"[note] N2".as_slice(),
                "[mark] 供电边界".as_bytes(),
            ],
            ".rec 载荷必须与入队内容逐字节一致"
        );
        assert_eq!(batch.valid_bytes, raw.len(), "排空后不允许有残余尾记录");
        // 时间戳按消费序单调不减（单消费者 = 入队序）
        let stamps: Vec<u64> = batch
            .records
            .iter()
            .map(|record| record.ts_unix_ns)
            .collect();
        assert!(
            stamps.windows(2).all(|pair| pair[0] <= pair[1]),
            "时间戳必须单调不减: {stamps:?}"
        );
    }

    /// LOG-12：portable 归档模式（桌面产品实际装配形状）下写线程链路零丢失
    /// ——saving 开启时首条消息触发段目录创建，drop 排空后 txt/bin/rec 三流
    /// 同段落盘。名字用合法形状（COM 口名是 Windows 保留名，直传会被
    /// name_component 拒绝且错误被写线程静默吞掉——桌面侧由
    /// `archive_safe_name` 守卫兜底）。
    #[test]
    fn ctx_portable_archive_drains_all_three_streams() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/log12-portable")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let saving = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let quota = duthub_core::dut_quota::DutQuota::new(64 * 1024 * 1024);
        let logs = Arc::new(std::sync::Mutex::new(SessionLogWriter::portable_archive(
            &root, "dut-com8", "dut-com8", saving, quota,
        )));
        let bus = Arc::new(EventBus::new(64));
        let ctx = SessionCtx::new("dut-com8", "sess-com8", bus).with_logs(logs);
        ctx.log_keystrokes(b"k1");
        ctx.emit_raw(b"R1");
        drop(ctx);
        let logs_dir = root.join("dut-com8").join("logs");
        let entries: Vec<String> = std::fs::read_dir(&logs_dir)
            .expect("portable 段目录必须被首条消息创建")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != ".log-sequence")
            .collect();
        assert_eq!(entries.len(), 3, "三条流各一段: {entries:?}");
        assert!(entries.iter().all(|name| name.starts_with("dut-com8-001_")));
        assert!(entries.iter().any(|name| name.ends_with(".txt")));
        assert!(entries.iter().any(|name| name.ends_with(".bin")));
        assert!(entries.iter().any(|name| name.ends_with(".rec")));
        std::fs::remove_dir_all(&root).ok();
    }

    /// PERF-1 审计侧：合并批次的每条原始命令逐条喂键入审计（仅 .bin），中间
    /// 夹设备 RX 块——.bin 必须按调用顺序逐字节落盘（键入→RX→键入→RX→键入），
    /// 证明「每条原始命令保持独立元素」最终体现在审计流按原边界有序、一字节
    /// 不丢；键入审计只进 .bin，.log 不受影响。
    #[test]
    fn merged_batch_keystroke_audit_lands_per_command_in_order() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/perf1-audit")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let lease = Some(WriteCtx {
            lease_id: "gui-audit".into(),
            generation: 1,
        });
        let second = b"s\r".to_vec();
        let backspace = vec![0x7f_u8];
        push_writes(
            &tx,
            lease.clone(),
            &[second.as_slice(), backspace.as_slice()],
        );
        let (batch, deferred) = absorb_keystrokes(&mut rx, b"l".to_vec(), &lease);
        assert!(deferred.is_none());
        assert_eq!(
            batch,
            vec![b"l".to_vec(), second.clone(), backspace.clone()],
            "合并批次必须逐元素保留原命令"
        );
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-kaudit").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-ka", "sess-kaudit", bus).with_logs(logs);
        for element in &batch {
            ctx.log_keystrokes(element);
            ctx.emit_raw(b"<echo>");
        }
        ctx.flush_logs();
        let mut expected = Vec::new();
        for element in &batch {
            expected.extend_from_slice(element);
            expected.extend_from_slice(b"<echo>");
        }
        let raw = std::fs::read(dir.join("sess-kaudit.bin")).unwrap();
        assert_eq!(raw, expected, ".bin 必须按原始命令边界逐条有序落盘");
        let text = std::fs::read_to_string(dir.join("sess-kaudit.log")).unwrap();
        assert_eq!(
            text, "<echo>\n<echo>\n<echo>\n",
            "键入审计只进 .bin，不进 .log"
        );
    }

    /// 乱码检测接线：开关来自共享的每 DUT map，原始字节在 emit_raw 采样，
    /// 结论随快照发布；关闭即复位为 Clean。SerialChunk 文本/顺序不受影响。
    #[test]
    fn garble_reaches_the_snapshot_only_when_enabled() {
        let bus = Arc::new(EventBus::new(64));
        let switch = Arc::new(Mutex::new(HashMap::new()));
        switch
            .lock()
            .unwrap()
            .insert("dut-garble".to_string(), true);
        let ctx =
            SessionCtx::new("dut-garble", "sess-garble", bus.clone()).with_garble(switch.clone());

        // 512 counted NUL bytes (>= MIN_WINDOW_BYTES) force the Garbled level.
        ctx.emit_raw(&[0x00u8; 512]);
        ctx.emit_raw(b"ok\r\n");
        ctx.emit_screen(duthub_core::ipc::ScreenSnapshot::default());

        let events = bus.read_log("sess-garble", 0, usize::MAX);
        let chunks: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            chunks,
            vec!["\0".repeat(512), "ok\r\n".to_string()],
            "检测不得改变 SerialChunk 的文本与顺序"
        );
        let screen = events
            .iter()
            .rev()
            .find_map(|e| match &e.event {
                UiEvent::SerialScreen { screen } => Some(screen.clone()),
                _ => None,
            })
            .expect("快照必须上总线");
        assert_eq!(screen.garbled.level, duthub_core::ipc::GarbleLevel::Garbled);
        assert!(screen.garbled.control_permille > 0);
        assert_eq!(screen.garbled.window_bytes, 516);

        // Runtime switch flip: detection off resets the meter, snapshot Clean.
        switch
            .lock()
            .unwrap()
            .insert("dut-garble".to_string(), false);
        ctx.emit_screen(duthub_core::ipc::ScreenSnapshot::default());
        let screen = bus
            .read_log("sess-garble", 0, usize::MAX)
            .into_iter()
            .rev()
            .find_map(|e| match e.event {
                UiEvent::SerialScreen { screen } => Some(screen),
                _ => None,
            })
            .expect("快照必须上总线");
        assert_eq!(screen.garbled.level, duthub_core::ipc::GarbleLevel::Clean);
        assert_eq!(screen.garbled.control_permille, 0);
        assert_eq!(screen.garbled.window_bytes, 0);
    }
}
