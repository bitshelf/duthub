//! duthub-device：串口运行时（会话管理 + 事件总线 + 日志管道）。
//!
//! 门面 `DeviceService` 提供同步 API（GPUI/TUI 线程直接调用）；
//! 内部持有独立 tokio runtime，每个会话一个独占 task（读写同 task）。

pub mod bus;
pub mod control;
pub mod demo;
pub mod heartbeat;
pub mod logs;
#[cfg(feature = "real-hardware")]
pub mod real;
pub mod session;
pub mod term;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub use tokio::sync::broadcast;

use duthub_core::audit::AuditSink;
use duthub_core::ipc::EventEnvelope;
use duthub_core::lease::{ControlLeaseManager, LeaseHolder, WriteCtx, WriteIdentity};
use duthub_core::{DeviceError, DeviceResult, SerialPortInfo};

use crate::bus::EventBus;
use crate::logs::SessionLogWriter;
use crate::session::Registry;
pub use crate::session::{LineEnding, SessionCommand, SessionKind};

/// 关闭级联的观测日志（测试取消顺序；未来 Agent 侧插入第一步）。
pub const CLOSE_STEPS: [&str; 4] = [
    "notify_agent_placeholder",
    "notify_serial_task",
    "remove_handle",
    "release_port",
];

pub struct DeviceService {
    runtime: tokio::runtime::Runtime,
    bus: Arc<EventBus>,
    registry: Arc<Mutex<Registry>>,
    logs_dir: PathBuf,
    archive_saving: Option<Arc<AtomicBool>>,
    artifact_quota: Option<Arc<duthub_core::dut_quota::DutQuota>>,
    demo_delay_ms: u64,
    close_log: Arc<Mutex<Vec<&'static str>>>,
    /// 控制租约管理器（GUI 优先与 INV-01 的执行点；None = 无租约要求路径，
    /// 既有测试行为不变）。桌面端构造后也可替换/共享给 MCP 层。
    lease: Option<Arc<ControlLeaseManager>>,
    /// 审计接收端（write.admit/write.deny；与租约共用生命周期）。
    audit: Option<Arc<AuditSink>>,
}

impl DeviceService {
    /// 产品构造：日志目录 = 解析后工作目录 `logs/`（settings.json `workspaceDir` >
    /// 默认 exe 同级 `duthub-data/`；桌面端传 `--workdir` 时由 main 统一解析后走
    /// `custom`，此处的解析不含 CLI）。
    ///
    /// 接线租约与审计：TTL 取 settings.json `agentAccess.leaseTtlSecs`（默认 120s），
    /// auth 开关取 `agentAccess.enabled`（默认关），审计落 `<工作目录>/audit/`。
    /// 管理器实例经 [`Self::lease_manager`] 暴露——MCP 层四件套工具（P3）与
    /// GUI 面板（P4）必须复用同一实例，否则复核口径分裂。
    pub fn new() -> DeviceResult<Self> {
        let config_dir = duthub_core::store::data_dir()?;
        let settings = duthub_core::store::load_settings(&config_dir)
            .unwrap_or_else(|_| duthub_core::store::default_settings());
        let (work_dir, _) =
            duthub_core::store::ensure_work_dir(settings.workspace_dir.as_deref(), None);
        let service = Self::custom(1024, work_dir.join("logs"), 25)
            .with_log_archives(settings.terminal.save_logs.unwrap_or(true));
        let audit = AuditSink::at_dir(work_dir.join("audit"));
        // TTL 下限 1s（配置 0/异常值时不至于把租约变成立即过期）
        let ttl_secs = settings.agent_access.lease_ttl_secs.max(1);
        let lease = Arc::new(
            ControlLeaseManager::new(Some(audit.clone()))
                .with_ttl(std::time::Duration::from_secs(u64::from(ttl_secs)))
                .with_gui_session_lifetime(),
        );
        lease.set_auth_enabled(settings.agent_access.enabled);
        Ok(service.with_lease_authority(lease).with_audit_sink(audit))
    }

    /// 测试构造：总线容量 / 日志目录 / demo 播放节奏可控（无租约复核，行为与
    /// 既有版本一致；需要租约用例时再链 [`Self::with_lease_authority`]）。
    pub fn custom(bus_capacity: usize, logs_dir: PathBuf, demo_delay_ms: u64) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        Self {
            runtime,
            bus: Arc::new(EventBus::new(bus_capacity)),
            registry: Arc::new(Mutex::new(Registry::default())),
            logs_dir,
            archive_saving: None,
            artifact_quota: None,
            demo_delay_ms,
            close_log: Arc::new(Mutex::new(Vec::new())),
            lease: None,
            audit: None,
        }
    }

    pub fn with_log_archives(mut self, enabled: bool) -> Self {
        self.archive_saving = Some(Arc::new(AtomicBool::new(enabled)));
        self
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

    pub fn set_log_saving(&self, enabled: bool) {
        if let Some(saving) = &self.archive_saving {
            saving.store(enabled, Ordering::Release);
        }
    }

    pub fn log_control_boundary(&self, dut_id: &str, label: &str, split: bool) -> DeviceResult<()> {
        let (session_id, logs) = {
            let registry = self.registry.lock().unwrap();
            let handle = registry.get(dut_id).ok_or(DeviceError::NotFound)?;
            (handle.session_id.clone(), handle.logs.clone())
        };
        let line = format!("[mark] {label}");
        let result = if split {
            logs.lock().unwrap().mark(&line)
        } else {
            logs.lock().unwrap().append_text(&line)
        };
        self.bus.publish(
            &session_id,
            dut_id,
            crate::bus::chunk(line, duthub_core::ChunkStyle::Mark),
        );
        result
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

    /// 向会话日志写入分段标记行（日志分割）：`[mark] {label}`（Mark 样式），
    /// 终端流 / .log 文本 / .bin 索引同步。未打开会话报 NotFound。
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

    pub fn close_log(&self) -> Vec<&'static str> {
        self.close_log.lock().unwrap().clone()
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
        let mut registry = self.registry.lock().unwrap();
        if registry.get(dut_id).is_some() {
            return Err(DeviceError::Busy);
        }
        registry.lock_port("local", port)?;
        let session_id = format!("sess-{dut_id}");
        let writer = if let Some(saving) = &self.archive_saving {
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
        let logs = Arc::new(Mutex::new(writer));
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
            logs,
            opened_flag: Some(opened_flag.clone()),
            lease_authority: self
                .lease
                .clone()
                .map(|m| m.clone() as Arc<dyn duthub_core::lease::LeaseAuthority>),
            audit: self.audit.clone(),
        };
        let result = self
            .runtime
            .block_on(async move { session::spawn_session(bus, spec) });
        match result {
            Ok((mut handle, _cmd)) => {
                // 首开失败回收：real task 结束时若从未成功打开（flag=false），
                // 说明该会话已死且无人再发 Disconnected 之外的动作——回收句柄与端口锁，
                // 避免"重连中"假象、假成功重连与同端口 Busy。
                let join = handle.join.take();
                let opened_flag = handle.opened_flag.clone();
                registry.insert(handle);
                drop(registry);
                // GUI 自动租约（人类在场，方案 §7.1）：打开会话即持有；Agent 在控时
                // 打开仍成功（读不受限），GUI 写入在 admit 处被拒并提示控制方。
                if let Some(lease) = &self.lease {
                    let _ = lease.acquire(dut_id, &LeaseHolder::Gui, None);
                }
                if join.is_some() {
                    let registry = self.registry.clone();
                    let lease = self.lease.clone();
                    let dut = dut_id.to_string();
                    let host = "local".to_string();
                    let port = port.to_string();
                    self.runtime.block_on(async move {
                        tokio::spawn(async move {
                            let _ = join.unwrap().await;
                            if !opened_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                registry
                                    .lock()
                                    .unwrap()
                                    .reap_never_opened(&dut, &host, &port);
                                // 首开失败回收时同步释放 GUI 自动租约（避免残留占用）
                                if let Some(lease) = lease {
                                    lease.release_for_holder(&dut, &LeaseHolder::Gui);
                                }
                            }
                        });
                    });
                }
                Ok(session_id)
            }
            Err(err) => {
                registry.unlock_port("local", port);
                Err(err)
            }
        }
    }

    /// 关闭会话（级联顺序见 CLOSE_STEPS）：通知 task → 移除句柄 → 释放端口。
    #[tracing::instrument(name = "serial.close", skip(self), fields(dut_id, ok))]
    pub fn close_session(&self, dut_id: &str) -> DeviceResult<()> {
        let mut registry = self.registry.lock().unwrap();
        let Some(handle) = registry.get(dut_id) else {
            tracing::warn!(dut_id, "serial.close not found");
            return Err(DeviceError::NotFound);
        };
        {
            let mut log = self.close_log.lock().unwrap();
            log.push(CLOSE_STEPS[0]); // Agent 取消占位（M5 接入真实步骤）
            log.push(CLOSE_STEPS[1]);
        }
        // 1) 通知会话 task 退出（它负责发 SerialDisconnected 并结束）
        let cmd_result = handle.cmd_tx.try_send(session::SessionCommand::Close);
        let port = handle.port.clone();
        let host = handle.host.clone();
        let session_id = handle.session_id.clone();
        let abort = handle.abort.clone();
        // 2) 从注册表移除（句柄离开注册表即视为释放）
        registry.remove(dut_id);
        {
            let mut log = self.close_log.lock().unwrap();
            log.push(CLOSE_STEPS[2]);
        }
        // 3) 释放端口锁
        registry.unlock_port(&host, &port);
        {
            let mut log = self.close_log.lock().unwrap();
            log.push(CLOSE_STEPS[3]);
        }
        // 兜底：task 未响应 Close 时由 abort 收尾（异步，不阻塞 UI 线程）
        let bus = self.bus.clone();
        self.runtime.spawn(async move {
            let _ = cmd_result;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if !abort.is_finished() {
                abort.abort();
            }
            bus.drop_session(&session_id);
        });
        drop(registry);
        // GUI 会话关闭 → 释放自动租约（人类离场；方案 §7.1）。
        // 放在注册表锁外：租约管理器是独立锁，且释放可能触发队列授予。
        if let Some(lease) = &self.lease {
            lease.release_for_holder(dut_id, &LeaseHolder::Gui);
        }
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

    pub fn logs_dir(&self) -> &PathBuf {
        &self.logs_dir
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub dut_id: String,
    pub session_id: String,
    pub kind: SessionKind,
    pub port: String,
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
        // 端口锁立即释放
        service
            .open_session("dut-b", "COM3", 115200, SessionKind::Demo)
            .unwrap();
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
        // 级联顺序记录
        assert_eq!(service.close_log(), CLOSE_STEPS.to_vec());
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

    /// 日志分割 B1：log_mark 写入 `[mark] …` 行（Mark 样式），终端流 / .log / .bin 同步。
    #[test]
    fn log_mark_reaches_bus_and_disk_with_mark_style() {
        use duthub_core::ipc::ChunkStyle;
        let service = test_service("mark");
        service
            .open_session("dut-a", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        // 未打开会话 → NotFound
        assert!(matches!(
            service.log_mark("dut-missing", "x"),
            Err(duthub_core::DeviceError::NotFound)
        ));
        service.log_mark("dut-a", "继电器 power ON").unwrap();
        let mut found = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            found = service
                .read_log("sess-dut-a", 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(
                        &e.event,
                        UiEvent::SerialChunk { text, style }
                            if text == "[mark] 继电器 power ON" && *style == ChunkStyle::Mark
                    )
                });
            if found {
                break;
            }
        }
        assert!(found, "终端流必须出现 Mark 样式的分段标记行");
        service.close_session("dut-a").ok();
        // 文本日志与原始索引同步落盘
        let text = std::fs::read_to_string(service.logs_dir().join("sess-dut-a.log")).unwrap();
        assert!(text.contains("[mark] 继电器 power ON"));
        let raw = std::fs::read(service.logs_dir().join("sess-dut-a.bin")).unwrap();
        assert!(
            std::str::from_utf8(&raw)
                .unwrap()
                .contains("[mark] 继电器 power ON")
        );
    }
}
