//! duthub-device：串口运行时（会话管理 + 事件总线 + 日志管道）。
//!
//! 门面 `DeviceService` 提供同步 API（GPUI/TUI 线程直接调用）；
//! 内部持有独立 tokio runtime，每个会话一个独占 task（读写同 task）。

pub mod boot_reference;
pub mod bus;
pub mod control;
pub mod demo;
pub mod garbled;
pub mod heartbeat;
pub mod logs;
#[cfg(feature = "real-hardware")]
pub mod real;
pub mod rec;
pub mod relay_watch;
pub mod session;
pub mod term;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use tokio::sync::broadcast;

use duthub_core::audit::AuditSink;
use duthub_core::ipc::EventEnvelope;
use duthub_core::lease::{ControlLeaseManager, LeaseHolder, WriteCtx, WriteIdentity};
use duthub_core::{DeviceError, DeviceResult, SerialPortInfo};

use crate::bus::EventBus;
use crate::logs::SessionLogWriter;
use crate::session::Registry;
pub use crate::session::{LineEnding, SessionCommand, SessionKind};

pub(crate) enum RuntimeOwner {
    Owned(tokio::runtime::Runtime),
    Shared(tokio::runtime::Handle),
}

impl RuntimeOwner {
    pub(crate) fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self {
            RuntimeOwner::Owned(rt) => rt.spawn(future),
            RuntimeOwner::Shared(handle) => handle.spawn(future),
        }
    }

    pub(crate) fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        match self {
            RuntimeOwner::Owned(rt) => rt.block_on(future),
            RuntimeOwner::Shared(handle) => handle.block_on(future),
        }
    }
}

pub struct DeviceService {
    runtime: RuntimeOwner,
    bus: Arc<EventBus>,
    registry: Arc<Mutex<Registry>>,
    logs_dir: PathBuf,
    archive_saving: Option<Arc<AtomicBool>>,
    artifact_quota: Option<Arc<duthub_core::dut_quota::DutQuota>>,
    demo_delay_ms: u64,
    /// Control lease manager shared with MCP and GUI layers.
    lease: Option<Arc<ControlLeaseManager>>,
    /// Audit sink recording write admission and denial events.
    audit: Option<Arc<AuditSink>>,
    /// Session log switch from serial settings; enabled by default.
    session_log_enabled: AtomicBool,
    /// Terminal scrollback capacity for newly opened sessions.
    scrollback_rows: AtomicUsize,
    /// Maximum rows carried in each screen snapshot.
    snapshot_max_rows: AtomicUsize,
    /// Per-DUT garbled-byte detection switch shared with active sessions.
    garble_enabled: Arc<Mutex<std::collections::HashMap<String, bool>>>,
}

impl DeviceService {
    /// Construct a `DeviceService` with its own background Tokio runtime.
    pub fn custom(bus_capacity: usize, logs_dir: PathBuf, demo_delay_ms: u64) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        Self::with_runtime_owner(
            RuntimeOwner::Owned(runtime),
            bus_capacity,
            logs_dir,
            demo_delay_ms,
        )
    }

    /// Construct a `DeviceService` sharing an externally owned Tokio runtime handle.
    pub fn with_runtime(
        handle: tokio::runtime::Handle,
        bus_capacity: usize,
        logs_dir: PathBuf,
        demo_delay_ms: u64,
    ) -> Self {
        Self::with_runtime_owner(
            RuntimeOwner::Shared(handle),
            bus_capacity,
            logs_dir,
            demo_delay_ms,
        )
    }

    fn with_runtime_owner(
        runtime: RuntimeOwner,
        bus_capacity: usize,
        logs_dir: PathBuf,
        demo_delay_ms: u64,
    ) -> Self {
        Self {
            runtime,
            bus: Arc::new(EventBus::new(bus_capacity)),
            registry: Arc::new(Mutex::new(Registry::default())),
            logs_dir,
            archive_saving: None,
            artifact_quota: None,
            demo_delay_ms,
            lease: None,
            audit: None,
            session_log_enabled: AtomicBool::new(true),
            scrollback_rows: AtomicUsize::new(crate::term::SCROLLBACK_HISTORY),
            snapshot_max_rows: AtomicUsize::new(crate::term::SNAPSHOT_MAX_ROWS),
            garble_enabled: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    pub fn with_log_archives(mut self, enabled: bool) -> Self {
        self.archive_saving = Some(Arc::new(AtomicBool::new(enabled)));
        self
    }

    /// 会话日志开关（串口设置页 `serial.sessionLogDisabled` 反值）。
    pub fn with_session_logging(self, enabled: bool) -> Self {
        self.session_log_enabled.store(enabled, Ordering::Release);
        self
    }

    /// 终端回滚缓冲行数（串口设置页 `serial.scrollbackRows`；下限 100）。
    /// Retain the requested history and limit the size of each live frame.
    pub fn with_scrollback(self, rows: usize) -> Self {
        self.configure_scrollback(Some(rows));
        self
    }

    pub fn configure_scrollback(&self, rows: Option<usize>) {
        let (history, snapshot) = rows.map_or(
            (
                crate::term::SCROLLBACK_HISTORY,
                crate::term::SNAPSHOT_MAX_ROWS,
            ),
            |rows| (rows, rows.min(crate::term::LIVE_SNAPSHOT_ROW_CAP)),
        );
        self.set_terminal_buffer(history, snapshot);
    }

    /// 更新终端缓冲（串口设置页保存后调用）：history = Term 回滚容量，
    /// snapshot = 快照最大行数。影响后续新建的会话。
    pub fn set_terminal_buffer(&self, history: usize, snapshot: usize) {
        self.scrollback_rows.store(
            history.clamp(
                crate::term::MIN_SCROLLBACK_ROWS,
                crate::term::MAX_SCROLLBACK_ROWS,
            ),
            Ordering::Release,
        );
        self.snapshot_max_rows.store(
            snapshot.clamp(
                crate::term::MIN_SCROLLBACK_ROWS,
                crate::term::LIVE_SNAPSHOT_ROW_CAP,
            ),
            Ordering::Release,
        );
    }

    /// 当前终端缓冲设置 `(history, snapshot)`（已应用 100 下限）——串口设置页
    /// 回显与测试断言用。
    pub fn terminal_buffer(&self) -> (usize, usize) {
        (
            self.scrollback_rows.load(Ordering::Acquire),
            self.snapshot_max_rows.load(Ordering::Acquire),
        )
    }

    /// 会话日志开关即时生效：活动会话的写线程同步开关；新会话按构造值。
    /// 总线事件（MCP serial_read / UI 注记）不受影响——只关文件落盘。
    pub fn set_session_logging(&self, enabled: bool) {
        let writers: Vec<Arc<Mutex<SessionLogWriter>>> = {
            let registry = self.registry.lock().unwrap();
            registry
                .session_ids()
                .iter()
                .filter_map(|id| registry.get(id))
                .map(|handle| handle.logs.clone())
                .collect()
        };
        for logs in writers {
            if let Ok(mut writer) = logs.lock() {
                writer.set_enabled(enabled);
            }
        }
        self.session_log_enabled.store(enabled, Ordering::Release);
    }

    /// 每 DUT 乱码检测开关（连接管理表单；`true` 才入 map，`false` 移除条目）。
    /// 活动会话的 `SessionCtx` 持有同一 `Arc`，在下一个 RX 块/快照即生效；
    /// 新会话在 `open_named_session` 时注入同一 map。
    pub fn set_garbled_detection_for(&self, dut_id: &str, enabled: bool) {
        let mut switches = match self.garble_enabled.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if enabled {
            switches.insert(dut_id.to_string(), true);
        } else {
            switches.remove(dut_id);
        }
    }

    /// 当前每 DUT 开关状态（缺省关闭）。
    pub fn garbled_detection_enabled(&self, dut_id: &str) -> bool {
        match self.garble_enabled.lock() {
            Ok(switches) => switches.get(dut_id).copied().unwrap_or(false),
            Err(_) => false,
        }
    }

    pub fn with_portable_archives(
        mut self,
        root: PathBuf,
        enabled: bool,
        quota: Arc<duthub_core::dut_quota::DutQuota>,
    ) -> Self {
        self.logs_dir = root;
        self.artifact_quota = Some(quota);
        self.with_log_archives(enabled)
    }

    pub fn configure_serial_log(
        &self,
        dut_id: &str,
        options: SerialLogOptions<'_>,
    ) -> DeviceResult<()> {
        let (logs, sender) = {
            let registry = self.registry.lock().unwrap();
            let Some(handle) = registry.get(dut_id) else {
                return Ok(());
            };
            (handle.logs.clone(), handle.log_tx.clone())
        };
        let reference = options.load_reference(dut_id);
        if let Some(sender) = sender {
            sender
                .send(crate::demo::LogMsg::SetLogMode {
                    split: options.split,
                    reference,
                })
                .map_err(|_| DeviceError::Invalid("日志写线程已退出".into()))
        } else {
            logs.lock().unwrap().set_log_mode(options.split, reference)
        }
    }

    pub fn apply_boot_reference(&self, dut_id: &str, path: &std::path::Path) -> DeviceResult<()> {
        let reference = boot_reference::BootReference::load(path)
            .map_err(|error| DeviceError::Invalid(error.to_string()))?;
        let registry = self.registry.lock().unwrap();
        let Some(handle) = registry.get(dut_id) else {
            return Ok(());
        };
        if let Some(sender) = &handle.log_tx {
            sender
                .send(crate::demo::LogMsg::SetBootReference(reference))
                .map_err(|_| DeviceError::Invalid("日志写线程已退出".into()))
        } else {
            let mut writer = handle.logs.lock().unwrap();
            writer.flush_rx_prefix()?;
            writer.set_boot_reference(Some(reference));
            Ok(())
        }
    }

    pub fn log_control_boundary(&self, dut_id: &str, label: &str, split: bool) -> DeviceResult<()> {
        let (logs, log_tx) = {
            let registry = self.registry.lock().unwrap();
            let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
            (handle.logs.clone(), handle.log_tx.clone())
        };
        let line = format!("[mark] {label}");
        if split {
            // Complete the durable boundary after earlier RX work and before
            // the control service dispatches a power operation.
            match log_tx {
                Some(tx) => {
                    let (ack, rx) = std::sync::mpsc::channel();
                    if tx
                        .send(crate::demo::LogMsg::MarkSync {
                            line: line.clone(),
                            ack,
                        })
                        .is_ok()
                    {
                        rx.recv().map_err(|_| {
                            DeviceError::Invalid("日志写线程已退出，分段未确认".into())
                        })?
                    } else {
                        // Writer thread gone: inline mark + companion rec record.
                        let mut writer = logs.lock().unwrap();
                        writer.power_mark(&line)
                    }
                }
                None => {
                    let mut writer = logs.lock().unwrap();
                    writer.power_mark(&line)
                }
            }
        } else {
            let mut writer = logs.lock().unwrap();
            let result = writer.append_text(&line);
            let _ = writer.append_rec(crate::rec::RecKind::Note, line.as_bytes());
            result
        }
    }

    /// 接线控制租约管理器（builder）。接管写入通道 admit 与 GUI 会话自动租约。
    pub fn with_lease_authority(mut self, lease: Arc<ControlLeaseManager>) -> Self {
        self.lease = Some(lease);
        self
    }

    /// 接线审计接收端（写入通道 deny 审计；租约事件审计由管理器自带）。
    pub fn with_audit_sink(mut self, audit: Arc<AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// 共享的租约管理器（MCP 控制权四件套 / GUI 控制权面板必须复用此实例）。
    pub fn lease_manager(&self) -> Option<Arc<ControlLeaseManager>> {
        self.lease.clone()
    }

    pub fn audit_sink(&self) -> Option<Arc<AuditSink>> {
        self.audit.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.bus.subscribe()
    }

    pub fn read_log(&self, session_id: &str, since_seq: u64, limit: usize) -> Vec<EventEnvelope> {
        self.bus.read_log(session_id, since_seq, limit)
    }

    pub fn read_log_tail(&self, session_id: &str, limit: usize) -> Vec<EventEnvelope> {
        self.bus.read_log_tail(session_id, limit)
    }

    pub fn read_serial_page(
        &self,
        session_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> (u64, Vec<EventEnvelope>) {
        self.bus.read_serial_page(session_id, since_seq, limit)
    }

    /// 会话缓冲中的最新 seq（无会话/缓冲为空时 0）——心跳响应窗口的读取游标。
    pub fn session_last_seq(&self, dut_id: &str) -> u64 {
        self.bus.last_seq(&format!("sess-{dut_id}"))
    }

    /// Write a storage-only boundary. An unopened session returns NotFound.
    pub fn log_mark(&self, dut_id: &str, label: &str) -> DeviceResult<()> {
        if self.archive_saving.is_some() {
            return self.log_control_boundary(dut_id, label, true);
        }
        let registry = self.registry.lock().unwrap();
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        handle
            .cmd_tx
            .try_send(session::SessionCommand::Mark(label.to_string()))
            .map_err(|e| match e {
                tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
            })
    }

    /// 排空指定 DUT 会话的异步日志队列：阻塞到写线程把已入队消息全部落盘
    /// （日志读方 / 测试在会话未关闭时需要确定性的落盘点用）。
    pub fn flush_session_logs(&self, dut_id: &str) {
        let tx = {
            let registry = self.registry.lock().unwrap();
            registry.get(dut_id).and_then(|h| h.log_tx.clone())
        };
        if let Some(tx) = tx {
            let (ack, rx) = std::sync::mpsc::channel();
            if tx.send(crate::demo::LogMsg::Flush(ack)).is_ok() {
                let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
            }
        }
    }

    /// 枚举串口。真实硬件构建只报告系统发现的端口。
    pub fn list_ports(&self) -> Vec<SerialPortInfo> {
        #[cfg(feature = "real-hardware")]
        {
            real::available_ports().unwrap_or_else(|error| {
                tracing::warn!(%error, "serial port enumeration failed");
                Vec::new()
            })
        }
        #[cfg(not(feature = "real-hardware"))]
        {
            demo_ports()
        }
    }

    /// 打开会话；同端口互斥（Busy）。返回 session_id（`sess-<dut>`）。
    pub fn open_session(
        &self,
        dut_id: &str,
        port: &str,
        baud: u32,
        kind: SessionKind,
    ) -> DeviceResult<String> {
        self.open_named_session(dut_id, dut_id, port, baud, kind)
    }

    #[tracing::instrument(name = "serial.open", skip(self))]
    pub fn open_named_session(
        &self,
        dut_id: &str,
        name: &str,
        port: &str,
        baud: u32,
        kind: SessionKind,
    ) -> DeviceResult<String> {
        self.open_named_session_with_log_options(
            dut_id,
            name,
            port,
            baud,
            kind,
            SerialLogOptions {
                split: true,
                reference: None,
            },
        )
    }

    pub fn open_named_session_with_log_options(
        &self,
        dut_id: &str,
        name: &str,
        port: &str,
        baud: u32,
        kind: SessionKind,
        options: SerialLogOptions<'_>,
    ) -> DeviceResult<String> {
        let reference = options.load_reference(dut_id);
        let mut registry = self.registry.lock().unwrap();
        if registry.is_reserved(dut_id) {
            return Err(DeviceError::Busy);
        }
        registry.lock_port("local", port)?;
        let session_id = format!("sess-{dut_id}");
        let mut writer = if let Some(saving) = &self.archive_saving {
            if let Some(quota) = &self.artifact_quota {
                SessionLogWriter::portable_archive(
                    &self.logs_dir,
                    dut_id,
                    name,
                    saving.clone(),
                    quota.clone(),
                )
            } else {
                SessionLogWriter::archive(&self.logs_dir, dut_id, name, saving.clone())
            }
        } else {
            match SessionLogWriter::new(&self.logs_dir, &session_id) {
                Ok(writer) => writer,
                Err(err) => {
                    registry.unlock_port("local", port);
                    return Err(err);
                }
            }
        };
        if let Err(error) = writer.set_log_mode(options.split, reference) {
            registry.unlock_port("local", port);
            return Err(error);
        }
        let logs = Arc::new(Mutex::new(writer));
        // 会话日志开关（串口设置页）：按构造值初始化写线程开关
        if !self.session_log_enabled.load(Ordering::Acquire) {
            logs.lock().unwrap().set_enabled(false);
        }
        self.bus.drop_session(&session_id);
        let bus = self.bus.clone();
        let session_for_task = session_id.clone();
        let opened_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spec = session::SessionSpec {
            dut_id,
            session_id: &session_for_task,
            kind,
            port,
            baud,
            demo_line_delay_ms: self.demo_delay_ms,
            scrollback_rows: self.scrollback_rows.load(Ordering::Acquire),
            snapshot_max_rows: self.snapshot_max_rows.load(Ordering::Acquire),
            logs,
            opened_flag: Some(opened_flag.clone()),
            lease_authority: self
                .lease
                .clone()
                .map(|m| m.clone() as Arc<dyn duthub_core::lease::LeaseAuthority>),
            audit: self.audit.clone(),
            garble_enabled: Arc::clone(&self.garble_enabled),
        };
        let result = self
            .runtime
            .block_on(async move { session::spawn_session(bus, spec) });
        match result {
            Ok((mut handle, _cmd)) => {
                let join = handle
                    .join
                    .take()
                    .expect("spawned session has a join handle");
                let actor = join.id();
                let host = handle.host.clone();
                let port = handle.port.clone();
                let observed_session = session_id.clone();
                registry.insert(handle);
                if let Some(lease) = &self.lease {
                    let _ = lease.acquire(dut_id, &LeaseHolder::Gui, None);
                }
                drop(registry);

                let registry = self.registry.clone();
                let bus = self.bus.clone();
                let lease = self.lease.clone();
                let dut_id = dut_id.to_owned();
                self.runtime.spawn(async move {
                    let _ = join.await;
                    let final_seq = {
                        let mut registry = registry.lock().unwrap();
                        if registry.finish(&dut_id, actor).is_none() {
                            return;
                        }
                        if let Some(lease) = lease {
                            lease.release_for_holder(&dut_id, &LeaseHolder::Gui);
                        }
                        let final_seq = bus.last_seq(&observed_session);
                        registry.unlock_port(&host, &port);
                        final_seq
                    };
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    bus.expire_closed_session(&observed_session, final_seq);
                });
                Ok(session_id)
            }
            Err(err) => {
                registry.unlock_port("local", port);
                Err(err)
            }
        }
    }

    /// Request closure. The actor releases its port and lease after it exits.
    #[tracing::instrument(name = "serial.close", skip(self), fields(dut_id, ok))]
    pub fn close_session(&self, dut_id: &str) -> DeviceResult<()> {
        let mut registry = self.registry.lock().unwrap();
        let Some(handle) = registry.begin_close(dut_id) else {
            tracing::warn!(dut_id, "serial.close not found");
            return Err(DeviceError::NotFound);
        };
        let _ = handle.cmd_tx.try_send(session::SessionCommand::Close);
        let abort = handle.abort.clone();
        drop(handle);
        drop(registry);
        self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if !abort.is_finished() {
                abort.abort();
            }
        });
        Ok(())
    }

    /// 向会话写一行（自动补行尾）。返回错误类别直达 UI。
    ///
    /// 多行文本先归一为 CR 分隔（P2-034）：CRLF 折叠为 CR、单个 LF 替换为 CR，
    /// 裸 LF 直传会让设备 shell 行编辑回显错乱、.log `[TX]` 注记跨行续行无前缀。
    /// 行尾追加策略（[`LineEnding`]）只作用于末尾，不受影响。write_bytes 与
    /// GUI 键盘/粘贴路径不做此归一（粘贴路径在 UI 层自带归一）。
    ///
    /// `writer` 声明写入者身份，写入通道内租约复核（[D] 入口点，registry 锁内、
    /// try_send 前）：GUI 自动持有租约（人类在场）、Agent 凭 lease 通过、
    /// System（心跳）放行——例外见 [`duthub_core::lease::WriteIdentity`]。
    #[tracing::instrument(name = "serial.write", skip(self, text), fields(dut_id, len = text.len(), ok))]
    pub fn write_line(&self, dut_id: &str, writer: WriteIdentity, text: &str) -> DeviceResult<()> {
        // P2-034：CRLF → CR，再清掉残留的单个 LF（顺序保证不产生 \r\r\n 混排）
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        self.enqueue_line(dut_id, writer, |lease| SessionCommand::Write {
            text: normalized,
            lease,
        })
    }

    pub(crate) fn write_heartbeat_batch(
        &self,
        dut_id: &str,
        writer: WriteIdentity,
        batch: Arc<heartbeat::HeartbeatBatch>,
    ) -> DeviceResult<()> {
        self.enqueue_line(dut_id, writer, |lease| {
            SessionCommand::WriteHeartbeatBatch {
                text: batch.wire_text(),
                batch,
                lease,
            }
        })
    }

    fn enqueue_line(
        &self,
        dut_id: &str,
        writer: WriteIdentity,
        command: impl FnOnce(Option<WriteCtx>) -> SessionCommand,
    ) -> DeviceResult<()> {
        let registry = self.registry.lock().unwrap();
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        // [D] 入口复核（授权→租约→generation 的租约+代数部分；授权在 MCP 层）
        let lease = self.admit(dut_id, &writer)?;
        handle.cmd_tx.try_send(command(lease)).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
        })
    }

    #[tracing::instrument(name = "serial.set_baud", skip(self), fields(dut_id, baud, ok))]
    pub fn set_baud(&self, dut_id: &str, baud: u32) -> DeviceResult<()> {
        let registry = self.registry.lock().unwrap();
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        handle
            .cmd_tx
            .try_send(session::SessionCommand::SetBaud(baud))
            .map_err(|_| DeviceError::Disconnected)
    }

    /// 会话级行尾策略（H1.2）：无 / \n / \r\n，随后的写入按该策略生效。
    pub fn set_line_ending(&self, dut_id: &str, ending: LineEnding) -> DeviceResult<()> {
        let registry = self.registry.lock().unwrap();
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        handle
            .cmd_tx
            .try_send(session::SessionCommand::SetEnding(ending))
            .map_err(|_| DeviceError::Disconnected)
    }

    /// 原始字节直写（PuTTY 键盘路径）：不做行尾追加、不发 [TX] 回显。
    /// UI 键盘输入（字符 / Enter \r / Backspace 0x7f）走这条路径。
    /// 租约复核与 [`Self::write_line`] 同规则（[D] 入口 + [E] actor 双点）。
    pub fn write_bytes(
        &self,
        dut_id: &str,
        writer: WriteIdentity,
        bytes: &[u8],
    ) -> DeviceResult<()> {
        let registry = self.registry.lock().unwrap();
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        let lease = self.admit(dut_id, &writer)?;
        handle
            .cmd_tx
            .try_send(session::SessionCommand::WriteBytes {
                bytes: bytes.to_vec(),
                lease,
            })
            .map_err(|e| match e {
                tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
            })
    }

    /// [D] 入口租约复核（写入唯一通道，try_send 前、registry 锁内）。
    /// 未接线租约时返回 Ok(None)（既有路径零改动）。
    /// 拒绝 → 审计 write.deny + `DeviceError::Lease`（GUI 优先无条件生效）。
    fn admit(&self, dut_id: &str, writer: &WriteIdentity) -> Result<Option<WriteCtx>, DeviceError> {
        let Some(lease) = &self.lease else {
            return Ok(None);
        };
        match lease.admit_queued_write(dut_id, writer) {
            Ok(ctx) => {
                // GUI 逐键写入不进审计（刷屏）；Agent / 审批（P1-014 器件层前置，
                // 同型）写入审计 write.admit
                if matches!(
                    writer,
                    WriteIdentity::Agent { .. } | WriteIdentity::Approval
                ) && let Some(audit) = &self.audit
                {
                    let who = match writer {
                        WriteIdentity::Approval => "approval",
                        _ => "agent",
                    };
                    let detail = match &ctx {
                        Some(c) if !c.lease_id.is_empty() => {
                            format!("{who} 写入 lease={} gen={}", c.lease_id, c.generation)
                        }
                        Some(c) if who == "approval" => {
                            format!("{who} 写入（审批放行，纪元 fence gen={}）", c.generation)
                        }
                        _ => format!("{who} 写入（无租约，auth 关过渡）"),
                    };
                    audit.record(
                        "write.admit",
                        None,
                        None,
                        Some(dut_id),
                        duthub_core::audit::AuditOutcome::Ok,
                        Some(&detail),
                    );
                }
                Ok(ctx)
            }
            Err(err) => {
                if let Some(audit) = &self.audit {
                    audit.record(
                        "write.deny",
                        None,
                        None,
                        Some(dut_id),
                        duthub_core::audit::AuditOutcome::Denied,
                        Some(&err.message()),
                    );
                }
                Err(err.to_device_error())
            }
        }
    }

    /// 终端网格重设尺寸（窗口 resize → real 会话 term.resize(cols, rows)）。
    /// 会话不存在时静默忽略（会话打开后的下一帧会重发）。
    pub fn resize_terminal(&self, dut_id: &str, cols: usize, rows: usize) -> DeviceResult<()> {
        let registry = self.registry.lock().unwrap();
        let Some(handle) = registry.get(dut_id) else {
            return Err(DeviceError::NotFound);
        };
        handle
            .cmd_tx
            .try_send(session::SessionCommand::Resize { cols, rows })
            .map_err(|_| DeviceError::Disconnected)
    }

    /// 恢复当前会话的本地终端仿真状态。
    pub fn reset_terminal(&self, dut_id: &str) -> DeviceResult<()> {
        let registry = self.registry.lock().unwrap();
        let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
        handle
            .cmd_tx
            .try_send(SessionCommand::ResetTerminal)
            .map_err(|err| match err {
                tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
            })
    }

    pub fn read_terminal_history(
        &self,
        dut_id: &str,
        skip: usize,
    ) -> DeviceResult<tokio::sync::oneshot::Receiver<crate::term::TerminalHistoryPage>> {
        let registry = self.registry.lock().unwrap();
        let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
        let (reply, receiver) = tokio::sync::oneshot::channel();
        handle
            .cmd_tx
            .try_send(SessionCommand::ReadTerminalHistory { skip, reply })
            .map_err(|err| match err {
                tokio::sync::mpsc::error::TrySendError::Full(_) => DeviceError::Busy,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => DeviceError::Disconnected,
            })?;
        Ok(receiver)
    }

    /// 手动重连（H1.4）：唤醒退避等待立即重试。连接正常时为无害唤醒
    /// （notify_one 存许可，下一次断线退避立即返回一次）。
    pub fn manual_reconnect(&self, dut_id: &str) -> DeviceResult<()> {
        let registry = self.registry.lock().unwrap();
        let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
        handle.reconnect_notify.notify_one();
        Ok(())
    }

    /// 全部活跃会话摘要（UI tab 渲染用）。
    pub fn sessions(&self) -> Vec<SessionSummary> {
        let registry = self.registry.lock().unwrap();
        registry
            .session_ids()
            .into_iter()
            .filter_map(|dut| {
                registry.get(&dut).map(|h| SessionSummary {
                    dut_id: h.dut_id.clone(),
                    session_id: h.session_id.clone(),
                    kind: h.kind,
                    port: h.port.clone(),
                })
            })
            .collect()
    }

    pub fn session_source(&self, dut_id: &str) -> Option<&'static str> {
        match self.registry.lock().unwrap().get(dut_id).map(|h| h.kind) {
            Some(SessionKind::Demo) => Some("demo"),
            Some(SessionKind::Real) => Some("device"),
            None => None,
        }
    }

    pub fn real_session_opened(&self, dut_id: &str) -> bool {
        self.registry
            .lock()
            .unwrap()
            .get(dut_id)
            .is_some_and(|handle| {
                handle.kind == SessionKind::Real && handle.opened_flag.load(Ordering::Acquire)
            })
    }

    pub fn logs_dir(&self) -> &PathBuf {
        &self.logs_dir
    }

    /// `.rec` segments of a session's structured record log, oldest first.
    /// Normal mode: `logs_dir/sess-{dut_id}.rec(.N)`. Archive modes locate the
    /// DUT folder by stable dut_id (owner-file match — the folder name follows
    /// the archived DUT name and may differ from any display name), then
    /// collect both layouts (portable `logs/NAME-*.rec`, flat
    /// `dut-<hex>-*.rec`).
    pub fn session_rec_segments(&self, dut_id: &str) -> Vec<PathBuf> {
        if self.archive_saving.is_none() {
            return crate::rec::list_rec_segments(&self.logs_dir, &format!("sess-{dut_id}"));
        }
        let entries = match std::fs::read_dir(&self.logs_dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut folder = None;
        for entry in entries.flatten() {
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();
            // Owner-file match ties the folder to this stable dut_id regardless
            // of what the archived DUT is named.
            if duthub_core::dut_artifacts::open(&self.logs_dir, dut_id, &dir_name, false).is_ok() {
                folder = Some(entry.path());
                break;
            }
        }
        let Some(folder) = folder else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for dir in [folder.join("logs"), folder] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let prefix = duthub_core::log_archive::archive_prefix(dut_id);
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().to_string();
                let owned = file_name.ends_with(".rec")
                    && (dir.ends_with("logs") || file_name.starts_with(&prefix));
                if owned {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        found
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub dut_id: String,
    pub session_id: String,
    pub kind: SessionKind,
    pub port: String,
}

pub struct SerialLogOptions<'a> {
    pub split: bool,
    pub reference: Option<&'a std::path::Path>,
}

impl SerialLogOptions<'_> {
    fn load_reference(&self, dut_id: &str) -> Option<boot_reference::BootReference> {
        self.reference.filter(|_| self.split).and_then(|path| {
            match boot_reference::BootReference::load(path) {
                Ok(reference) => Some(reference),
                Err(error) => {
                    tracing::warn!(%dut_id, path = %path.display(), %error, "Could not read the boot log reference");
                    None
                }
            }
        })
    }
}

#[cfg(not(feature = "real-hardware"))]
fn demo_ports() -> Vec<SerialPortInfo> {
    vec![
        SerialPortInfo {
            port: "COM3".into(),
            description: Some("USB-SERIAL CH340 (demo)".into()),
        },
        SerialPortInfo {
            port: "COM7".into(),
            description: Some("Silicon Labs CP2102 (demo)".into()),
        },
        SerialPortInfo {
            port: "COM12".into(),
            description: Some("FTDI FT232R (demo)".into()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::ipc::UiEvent;
    use std::time::Duration;

    fn test_service(tag: &str) -> DeviceService {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DeviceService::custom(64, dir, 0)
    }

    fn absorb_texts(rx: &mut broadcast::Receiver<EventEnvelope>) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(env) => {
                    if let UiEvent::SerialChunk { text, .. } = env.event {
                        out.push(text);
                    }
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        out
    }

    #[test]
    fn open_twice_on_same_port_is_busy() {
        let service = test_service("busy");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        let err = service
            .open_session("dut-b", "COM3", 115200, SessionKind::Demo)
            .unwrap_err();
        assert!(matches!(err, DeviceError::Busy));
        // 不同端口可开
        service
            .open_session("dut-b", "COM5", 115200, SessionKind::Demo)
            .unwrap();
    }

    #[test]
    fn same_dut_cannot_open_twice() {
        let service = test_service("dupdut");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        assert!(
            service
                .open_session("dut-a", "COM7", 115200, SessionKind::Demo)
                .is_err()
        );
    }

    #[test]
    fn close_releases_port_and_emits_disconnected() {
        let service = test_service("close");
        let _rx = service.subscribe();
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        service.close_session("dut-a").unwrap();
        // A different DUT can take the port only after the original actor exits.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match service.open_session("dut-b", "COM3", 115200, SessionKind::Demo) {
                Ok(_) => break,
                Err(DeviceError::Busy) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => panic!("expected eventual port release: {result:?}"),
            }
        }
        // task 结束后发 Disconnected
        let mut disconnected = false;
        for _ in 0..100 {
            disconnected = !service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .filter(|e| matches!(e.event, UiEvent::SerialDisconnected { .. }))
                .collect::<Vec<_>>()
                .is_empty();
            if disconnected {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(disconnected, "关闭后必须产生 Disconnected 事件");
    }

    #[test]
    fn write_line_reaches_demo_driver() {
        let service = test_service("write");
        let mut rx = service.subscribe();
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(120)); // 启动日志
        let _ = absorb_texts(&mut rx);
        service
            .write_line("dut-a", WriteIdentity::gui(), "uname -a")
            .unwrap();
        let mut found = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            let log = service.read_log("sess-dut-a", 0, usize::MAX);
            found = log.iter().any(|e| {
                matches!(&e.event, UiEvent::SerialChunk { text, .. } if text.contains("aarch64 GNU/Linux"))
            });
            if found {
                break;
            }
        }
        assert!(found, "写入应得到模拟 uname 响应");
        service.close_session("dut-a").ok();
    }

    #[test]
    fn logs_are_written_to_disk() {
        let service = test_service("logs");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        service
            .write_line("dut-a", WriteIdentity::gui(), "uptime")
            .unwrap();
        std::thread::sleep(Duration::from_millis(80));
        service.close_session("dut-a").ok();
        let text_file = service.logs_dir().join("sess-dut-a.log");
        let raw_file = service.logs_dir().join("sess-dut-a.bin");
        let text = std::fs::read_to_string(&text_file).expect("文本日志存在");
        assert!(text.contains("Started Serial Getty"));
        assert!(text.contains("uptime"));
        let raw = std::fs::read(&raw_file).expect("原始日志存在");
        assert!(std::str::from_utf8(&raw).unwrap().contains("Serial Getty"));
    }

    /// H1.1/H1.2：写入产生 TX 回显；行尾策略经会话级设置后在下一条写入生效。
    #[test]
    fn set_line_ending_and_tx_echo_flow_through_service() {
        let service = test_service("tx-echo");
        let _rx = service.subscribe();
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        service
            .write_line("dut-a", WriteIdentity::gui(), "probe-1")
            .unwrap();
        service.set_line_ending("dut-a", LineEnding::Crlf).unwrap();
        service
            .write_line("dut-a", WriteIdentity::gui(), "probe-2")
            .unwrap();
        let mut texts = Vec::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            texts = service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .filter_map(|e| match &e.event {
                    UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            let have_a = texts.iter().any(|t| t == "[TX] probe-1");
            let have_b = texts.iter().any(|t| t == "[TX] probe-2");
            if have_a && have_b {
                break;
            }
        }
        assert!(
            texts.iter().any(|t| t == "[TX] probe-1"),
            "写入必须有 TX 回显行"
        );
        assert!(
            texts.iter().any(|t| t == "[TX] probe-2"),
            "SetEnding 后写入仍须有 TX 回显"
        );
        // .bin 原始日志：第二条带 CRLF 行尾（发送路径生效的直接证据）
        service.flush_session_logs("dut-a");
        let raw = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        assert!(raw.contains("probe-2\r\n"), "CRLF 策略应写进原始日志");
        // 未设置策略的第一条保持默认 Lf
        assert!(raw.contains("probe-1\n"));
        assert!(!raw.contains("probe-1\r\n"), "默认策略不得写出 CRLF");
        service.close_session("dut-a").ok();
    }

    /// P2-034：多行 write_line 归一为 CR 分隔——线上 payload 的行分隔符为 CR
    /// （末尾行尾仍按行尾策略追加），.log 的 [TX] 注记单行化、无裸续行。
    #[test]
    fn write_line_normalizes_newlines_to_cr() {
        let service = test_service("multiline");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        service
            .write_line("dut-a", WriteIdentity::gui(), "echo C1\r\necho C2")
            .unwrap();
        let mut done = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            done = service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(&e.event, UiEvent::SerialChunk { text, .. } if text.contains("Executed (demo)"))
                });
            if done {
                break;
            }
        }
        assert!(done, "写入必须到达 demo shell");
        service.close_session("dut-a").ok();
        // .bin 线上 payload：行分隔符为 CR，末尾按默认 Lf 策略补 \n
        let raw = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        assert!(raw.contains("echo C1\recho C2\n"), "payload: {raw:?}");
        assert!(!raw.contains("echo C1\necho C2"), "不得裸 LF 直传：{raw:?}");
        assert!(!raw.contains("echo C1\r\n"), "CRLF 不得原样透传：{raw:?}");
        // .log：[TX] 注记单行（换行转义为字面 \r），无无前缀续行
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-a.log")).unwrap();
        assert!(
            text.lines().any(|line| line == "[TX] echo C1\\recho C2"),
            ".log 必须有单行 [TX] 注记：{text:?}"
        );
        assert!(
            !text.lines().any(|line| line == "echo C2"),
            ".log 不得出现无前缀续行：{text:?}"
        );
    }

    /// H1.4：手动重连——存在会话时无害唤醒（demo 驱动无等待者），不存在时 NotFound。
    #[test]
    fn manual_reconnect_is_wakeup_not_error() {
        let service = test_service("reconnect");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        service.manual_reconnect("dut-a").unwrap();
        std::thread::sleep(Duration::from_millis(60));
        // 会话仍在（demo 驱动不因唤醒退出），且能继续写入
        service
            .write_line("dut-a", WriteIdentity::gui(), "still-alive")
            .unwrap();
        assert!(matches!(
            service.manual_reconnect("dut-missing"),
            Err(duthub_core::DeviceError::NotFound)
        ));
        service.close_session("dut-a").ok();
    }

    #[test]
    fn log_mark_stays_on_disk_and_out_of_the_serial_stream() {
        let service = test_service("mark");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        assert!(matches!(
            service.log_mark("dut-missing", "x"),
            Err(DeviceError::NotFound)
        ));
        let path = service.logs_dir().join("sess-dut-a.log");
        let mark = "[mark] 继电器 power ON";
        service.log_mark("dut-a", "继电器 power ON").unwrap();
        for _ in 0..50 {
            if std::fs::read_to_string(&path).is_ok_and(|text| text.contains(mark)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(std::fs::read_to_string(&path).unwrap().contains(mark));
        assert!(
            std::fs::read(service.logs_dir().join("sess-dut-a.bin"))
                .is_ok_and(|raw| String::from_utf8_lossy(&raw).contains(mark))
        );
        assert!(
            !service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .any(|event| {
                    matches!(&event.event, UiEvent::SerialChunk { text, .. } if text.contains(mark))
                })
        );
        service.close_session("dut-a").unwrap();
    }

    /// 指定会话日志构造值的测试服务（其余与 [`test_service`] 相同）。
    fn test_service_with(tag: &str, session_logging: bool) -> DeviceService {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DeviceService::custom(64, dir, 0).with_session_logging(session_logging)
    }

    /// 写入一行并轮询等待 demo 响应（RX，emit_raw）落盘——flush 屏障保证
    /// 确定性的落盘点：响应入盘意味着此前入队的日志消息全部写完。
    fn write_until_on_disk(service: &DeviceService, dut: &str, line: &str, expect: &str) {
        service.write_line(dut, WriteIdentity::gui(), line).unwrap();
        for _ in 0..100 {
            service.flush_session_logs(dut);
            let path = service.logs_dir().join(format!("sess-{dut}.log"));
            if std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains(expect)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("写入内容未在 2s 内落盘: {expect}");
    }

    /// LOG-5 MarkSync 同步路径（排空语义）：RX 先入队 → log_control_boundary
    /// (split=true) 同步返回 → mark 行必须落在已入队 RX 之后，并切段开启新段。
    /// 同步返回由「unwrap 后立刻读盘、无 sleep」锁定。
    #[test]
    fn log_control_boundary_drains_pending_rx_before_mark() {
        let service = test_service("boundary-drain");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        // RX 先入队：demo shell 响应（emit_raw → 总线 + 写线程队列）
        write_until_on_disk(&service, "dut-a", "probe-before-mark", "Executed (demo)");
        // 分段边界：MarkSync ack 同步返回，此前入队内容必须已全部落盘
        service
            .log_control_boundary("dut-a", "POWER ON BOUNDARY", true)
            .unwrap();
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-a.log")).unwrap();
        let rx_pos = text.find("Executed (demo)").expect("RX 响应必须在盘上");
        let mark_pos = text
            .find("[mark] POWER ON BOUNDARY")
            .expect("mark 行必须落当前段");
        assert!(mark_pos > rx_pos, "分段必须落在已入队 RX 之后：{text:?}");
        assert!(
            service.logs_dir().join("sess-dut-a.log.1").exists(),
            "split=true 必须切段（新段 .log.1）"
        );
        service.close_session("dut-a").ok();
    }

    #[test]
    fn log_control_boundary_reports_sync_error_at_rotation_cap() {
        let service = test_service("boundary-cap");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        let mut attempts = 0usize;
        let err = loop {
            attempts += 1;
            assert!(attempts <= 64, "分段必须在上限处失败而不是无限成功");
            match service.log_control_boundary("dut-a", &format!("seg-{attempts}"), true) {
                Ok(()) => continue,
                Err(err) => break err,
            }
        };
        assert_eq!(
            attempts, 21,
            "每 mark 必须真实切段：第 21 次（MAX_ROTATIONS=20）才失败"
        );
        assert!(err.to_string().contains("分段"), "应报分段上限错误：{err}");
        assert!(
            service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .all(|event| {
                    !matches!(
                        &event.event,
                        UiEvent::SerialChunk { text, .. }
                            if text.contains("日志分段失败") || text.starts_with("[mark] seg-")
                    )
                })
        );
        service.close_session("dut-a").ok();
    }

    /// LOG-4 DeviceService 扇出接线（漏改时门禁不会变红的那半段）：开关同步
    /// 生效到全部活动会话写线程——关闭期间两会话 .log/.bin 逐字节不变而总线
    /// SerialChunk 照常；重开后同文件续写。
    #[test]
    fn session_logging_toggle_fans_out_to_active_sessions() {
        let service = test_service("log-fanout");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        service
            .open_session("dut-b", "COM5", 115200, SessionKind::Demo)
            .unwrap();
        write_until_on_disk(&service, "dut-a", "probe-on-a", "Executed (demo)");
        write_until_on_disk(&service, "dut-b", "probe-on-b", "Executed (demo)");
        let log_a_before = std::fs::read(service.logs_dir().join("sess-dut-a.log")).unwrap();
        let bin_a_before = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        let log_b_before = std::fs::read(service.logs_dir().join("sess-dut-b.log")).unwrap();
        let bin_b_before = std::fs::read(service.logs_dir().join("sess-dut-b.bin")).unwrap();

        // 关闭：扇出到两个活动会话——文件逐字节不变，总线事件照常
        service.set_session_logging(false);
        service
            .write_line("dut-a", WriteIdentity::gui(), "probe-off-a")
            .unwrap();
        service
            .write_line("dut-b", WriteIdentity::gui(), "probe-off-b")
            .unwrap();
        let mut bus_seen = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            bus_seen = service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(&e.event,
                        UiEvent::SerialChunk { text, .. } if text.contains("[TX] probe-off-a"))
                });
            if bus_seen {
                break;
            }
        }
        assert!(bus_seen, "关闭落盘后总线事件必须照常（只关文件写入）");
        service.flush_session_logs("dut-a");
        service.flush_session_logs("dut-b");
        assert_eq!(
            std::fs::read(service.logs_dir().join("sess-dut-a.log")).unwrap(),
            log_a_before,
            "关闭后 dut-a .log 不得新增内容"
        );
        assert_eq!(
            std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap(),
            bin_a_before,
            "关闭后 dut-a .bin 不得新增内容"
        );
        assert_eq!(
            std::fs::read(service.logs_dir().join("sess-dut-b.log")).unwrap(),
            log_b_before,
            "关闭后 dut-b .log 不得新增内容"
        );
        assert_eq!(
            std::fs::read(service.logs_dir().join("sess-dut-b.bin")).unwrap(),
            bin_b_before,
            "关闭后 dut-b .bin 不得新增内容"
        );

        // 重开：活动会话同文件续写（不换段、不重建文件）
        service.set_session_logging(true);
        write_until_on_disk(&service, "dut-a", "probe-resume-a", "Executed (demo)");
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-a.log")).unwrap();
        assert!(
            text.contains("[TX] probe-on-a") && text.contains("[TX] probe-resume-a"),
            "重开后必须同文件续写：{text:?}"
        );
        service.close_session("dut-a").ok();
        service.close_session("dut-b").ok();
    }

    /// LOG-4 新会话按构造值初始化：`with_session_logging(false)` 构造 → 会话
    /// 打开即关闭写线程（文件存在但零写入）；`set_session_logging(true)` 扇出
    /// 重开活动会话，之后新建的会话按 true 初始化并正常落盘。
    #[test]
    fn new_sessions_initialize_from_session_logging_flag() {
        let service = test_service_with("log-init", false);
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        service
            .write_line("dut-a", WriteIdentity::gui(), "probe-while-disabled")
            .unwrap();
        let mut seen = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            seen = service.read_log("sess-dut-a", 0, usize::MAX).iter().any(
                |e| {
                    matches!(&e.event,
                        UiEvent::SerialChunk { text, .. } if text.contains("[TX] probe-while-disabled"))
                },
            );
            if seen {
                break;
            }
        }
        assert!(seen, "关闭落盘不影响总线回显");
        service.flush_session_logs("dut-a");
        let log = std::fs::read(service.logs_dir().join("sess-dut-a.log")).unwrap();
        let bin = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        assert!(log.is_empty(), "关闭构造值的会话 .log 必须零写入：{log:?}");
        assert!(bin.is_empty(), "关闭构造值的会话 .bin 必须零写入：{bin:?}");
        // 全局重开：活动会话（dut-a）与后续新建会话（dut-b）都按 true 生效
        service.set_session_logging(true);
        service
            .open_session("dut-b", "COM5", 115200, SessionKind::Demo)
            .unwrap();
        write_until_on_disk(&service, "dut-b", "probe-after-enable", "Executed (demo)");
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-b.log")).unwrap();
        assert!(
            text.contains("[TX] probe-after-enable"),
            "新会话必须按重开的构造值落盘：{text:?}"
        );
        service.close_session("dut-a").ok();
        service.close_session("dut-b").ok();
    }

    /// Check the configured history and ensure it does not enlarge live frames.
    #[test]
    fn terminal_buffer_clamps_to_minimum_100() {
        let service = test_service("buf-clamp");
        assert_eq!(
            service.terminal_buffer(),
            (
                crate::term::SCROLLBACK_HISTORY,
                crate::term::SNAPSHOT_MAX_ROWS
            ),
            "默认保留 1,000,000 行历史，实时快照保持 400 行"
        );
        service.set_terminal_buffer(50, 50);
        assert_eq!(
            service.terminal_buffer(),
            (100, 100),
            "低于下限必须 clamp 到 100"
        );
        service.set_terminal_buffer(500, 300);
        assert_eq!(service.terminal_buffer(), (500, 300), "显式设置原样生效");
        service.configure_scrollback(Some(2_000_000));
        assert_eq!(
            service.terminal_buffer(),
            (2_000_000, crate::term::LIVE_SNAPSHOT_ROW_CAP)
        );
    }

    /// 每 DUT 乱码检测开关：默认全关；按 DUT 独立开启/关闭；关闭即移除条目
    /// （map 只存开启项，新会话注入同一 map）。
    #[test]
    fn garbled_detection_switch_is_per_dut() {
        let service = test_service("garble-switch");
        assert!(!service.garbled_detection_enabled("dut-a"));
        assert!(!service.garbled_detection_enabled("dut-b"));

        service.set_garbled_detection_for("dut-a", true);
        assert!(service.garbled_detection_enabled("dut-a"));
        assert!(!service.garbled_detection_enabled("dut-b"), "不得跨 DUT");

        service.set_garbled_detection_for("dut-a", false);
        assert!(!service.garbled_detection_enabled("dut-a"));

        // Idempotent writes stay consistent.
        service.set_garbled_detection_for("dut-b", true);
        service.set_garbled_detection_for("dut-b", true);
        assert!(service.garbled_detection_enabled("dut-b"));
    }
}
