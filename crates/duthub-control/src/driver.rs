//! RelayDriver trait + CH340 类继电器驱动实现。
//!
//! - **单 task 独占串口句柄**：每个继电器串口一个专用 tokio task，所有读写都经由
//!   命令通道派发（与 duthub-device 会话模型一致，docs/ARCHITECTURE.md 线程边界）；
//! - 错误统一映射 [`DeviceError`]（NotFound/Busy/PermissionDenied/Timeout/
//!   Disconnected/Invalid）；
//! - 实现 [`duthub_core::ControlBackend`]：共用入口
//!   （duthub-device::control::ControlService）完成授权/绑定/能力校验后把已解析的
//!   目标直接派发到本驱动，驱动不接触审批令牌（审核 P1-3）；
//! - 按审核建议不做后端选择层：桌面端启动时直接构造本驱动注入入口（当前唯一实现）。
//!
//! 串口打开函数（[`Opener`]）可注入：生产为 tokio-serial（`real-hardware` 特性），
//! 测试注入 `tokio::io::duplex` 模拟板卡。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use duthub_core::backend::{ChannelTestReport, ControlBackend, ControlTarget};
use duthub_core::error::{DeviceError, DeviceResult};

use crate::ch340::{self, EchoCheck};

/// 命令回读等待：9600 波特下一帧回显约 4ms，300ms 足以覆盖板卡处理延迟；
/// 静默板卡（不回显）的命令额外付出一次超时等待。
const ECHO_TIMEOUT: Duration = Duration::from_millis(300);
/// Maximum time to wait for the port task to start a queued command.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(30);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// 脉冲时长上限：防止误用把独占 task 长期占死。
const PULSE_MAX_MS: u64 = 60_000;

/// 继电器串口字节流抽象：生产实现为 tokio-serial 的 `SerialStream`，
/// 测试注入 `tokio::io::duplex` 的一端。
pub trait RelayStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> RelayStream for T {}

/// 装箱的串口字节流。
pub type BoxStream = Box<dyn RelayStream>;

/// 打开串口句柄的函数（地址 + 波特率 → 独占流）。打开失败映射 [`DeviceError`]。
pub type Opener = Arc<dyn Fn(&str, u32) -> DeviceResult<BoxStream> + Send + Sync>;

/// 脉冲时长（ms）配置。继电器脉冲 = 吸合 → 保持 → 自动释放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PulseTiming {
    /// power_key 短按（默认 300ms）。
    pub power_key_ms: u64,
    /// power_key 长按（默认 3000ms）。
    pub power_key_long_ms: u64,
    /// reset 脉冲（默认 3000ms，与 sermcp `DUT_CTRL_MIN_RESET_MS` 缺省一致）。
    pub reset_ms: u64,
}

impl Default for PulseTiming {
    fn default() -> Self {
        Self {
            power_key_ms: 300,
            power_key_long_ms: 3000,
            reset_ms: 3000,
        }
    }
}

/// [`RelayDriver::open`] 的枚举结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayInfo {
    /// 继电器串口节点（COMx）。
    pub port: String,
    /// 协议波特率（固定 9600）。
    pub baud: u32,
    /// 应答 STATUS 的通道（1 起始）。板卡不回答 STATUS 时为空表——
    /// 此时开关命令仍可用（盲发），但通道枚举不可用。
    pub detected: Vec<ChannelInfo>,
}

/// 单通道枚举结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    /// 通道号（1 起始）。
    pub channel: u8,
    /// 当前是否吸合。
    pub on: bool,
}

/// 仅打开/关闭串口句柄的探测结果（不发任何协议帧）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortProbe {
    pub port: String,
    pub baud: u32,
}

/// 继电器板卡驱动（协议层）。地址 = 继电器串口节点（COMx）。
#[async_trait]
pub trait RelayDriver: Send + Sync {
    /// 打开继电器串口并枚举通道：对 1..=8 逐通道发 STATUS 查询（只读，
    /// 不切换任何通道）。端口打不开时报错；板卡不回答 STATUS 时
    /// `detected` 为空表但不视为错误。
    async fn open(&self, port: &str) -> DeviceResult<RelayInfo>;

    /// 通道通断（保持态，不做脉冲）。通道号从 1 开始。
    async fn set(&self, port: &str, channel: u8, on: bool) -> DeviceResult<()>;

    /// 脉冲：吸合 `ms` 毫秒后自动释放。脉冲在独占 task 内执行，
    /// 调用方取消（future 被 drop）不会中断释放——吸合态总能被收回。
    /// 释放失败时报错并注明通道可能仍处于吸合态。
    async fn pulse(&self, port: &str, channel: u8, ms: u64) -> DeviceResult<()>;

    /// 查询通道状态。`Ok(None)` = 板卡不回答 STATUS（状态未知）。
    async fn status(&self, port: &str, channel: u8) -> DeviceResult<Option<bool>>;

    /// 关闭串口句柄（下次命令自动重开）。幂等。
    async fn close(&self, port: &str) -> DeviceResult<()>;
}

/// CH340 类 4 字节协议继电器驱动（参考 bitshelf/sermcp dut-ctrl）。
pub struct Ch340RelayDriver {
    ports: Mutex<HashMap<String, PortHandle>>,
    opener: Opener,
    timing: PulseTiming,
}

#[derive(Clone)]
struct PortHandle {
    cmd_tx: mpsc::Sender<QueuedCommand>,
}

/// 发往串口独占 task 的命令。应答一律经 oneshot 回传 [`DeviceResult`]。
struct QueuedCommand {
    started: oneshot::Sender<()>,
    command: PortCommand,
}

enum PortCommand {
    ChannelTest {
        channel: u8,
        reply: oneshot::Sender<DeviceResult<ChannelTestReport>>,
    },
    Set {
        channel: u8,
        on: bool,
        reply: oneshot::Sender<DeviceResult<()>>,
    },
    Pulse {
        channel: u8,
        ms: u64,
        reply: oneshot::Sender<DeviceResult<()>>,
    },
    Status {
        channel: u8,
        reply: oneshot::Sender<DeviceResult<Option<bool>>>,
    },
    Close {
        reply: oneshot::Sender<DeviceResult<()>>,
    },
}

impl Ch340RelayDriver {
    /// 生产构造：tokio-serial 直开串口。未启用 `real-hardware` 特性时
    /// 所有端口打开都报 [`DeviceError::Unsupported`]（demo 模式不接硬件）。
    pub fn new() -> Self {
        Self::with_opener(Self::default_opener())
    }

    /// 测试/特殊传输构造：注入串口打开函数。
    pub fn with_opener(opener: Opener) -> Self {
        Self {
            ports: Mutex::new(HashMap::new()),
            opener,
            timing: PulseTiming::default(),
        }
    }

    /// 覆盖脉冲时长（短按/长按/复位）。数值为 0 时回落默认值。
    pub fn with_timing(mut self, timing: PulseTiming) -> Self {
        self.timing = timing;
        self
    }

    fn default_opener() -> Opener {
        #[cfg(feature = "real-hardware")]
        {
            Arc::new(|port: &str, baud: u32| open_serial(port, baud))
        }
        #[cfg(not(feature = "real-hardware"))]
        {
            Arc::new(|_port: &str, _baud: u32| Err(DeviceError::Unsupported))
        }
    }

    /// 仅打开/关闭串口句柄，不发送任何协议帧——真实端口「在位探测」用。
    /// 与协议层 `open()`（会发 STATUS 枚举帧）区分开。
    pub fn probe_port(&self, port: &str, baud: u32) -> DeviceResult<PortProbe> {
        let stream = (self.opener)(port, baud)?;
        drop(stream);
        Ok(PortProbe {
            port: port.to_string(),
            baud,
        })
    }

    /// 取（或惰性创建）端口独占 task 的句柄。task 随驱动丢弃（命令通道关闭）自然退出。
    fn ensure_task(&self, port: &str) -> DeviceResult<PortHandle> {
        let mut ports = self.ports.lock().unwrap();
        if let Some(handle) = ports.get(port)
            && !handle.cmd_tx.is_closed()
        {
            return Ok(handle.clone());
        }
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let opener = self.opener.clone();
        let port_name = port.to_string();
        tokio::spawn(run_port_task(
            port_name,
            ch340::DEFAULT_BAUD,
            opener,
            cmd_rx,
        ));
        let handle = PortHandle { cmd_tx };
        ports.insert(port.to_string(), handle.clone());
        Ok(handle)
    }

    /// 校验通道号：1..=255。
    fn valid_channel(channel: u8) -> DeviceResult<u8> {
        if channel == 0 {
            return Err(DeviceError::Invalid(
                "通道号无效（继电器通道从 1 开始）".into(),
            ));
        }
        Ok(channel)
    }

    /// 本驱动只接串口继电器目标；SNMP PDU 由专用后端负责（未来接入时在
    /// 运行时组装处做选择点），此处拒绝异类目标防止把 PDU 地址当串口打开。
    fn assert_relay_target(target: &ControlTarget) -> DeviceResult<()> {
        if target.driver == "snmp-pdu" {
            return Err(DeviceError::Unsupported);
        }
        Ok(())
    }

    /// Bound queue admission separately from the time needed to release a relay.
    async fn dispatch<T>(
        &self,
        port: &str,
        make: impl FnOnce(oneshot::Sender<DeviceResult<T>>) -> PortCommand,
    ) -> DeviceResult<T> {
        let handle = self.ensure_task(port)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        let (started, mut start) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + QUEUE_TIMEOUT;
        let queued = QueuedCommand {
            started,
            command: make(reply_tx),
        };
        tokio::time::timeout_at(deadline, handle.cmd_tx.send(queued))
            .await
            .map_err(|_| DeviceError::Timeout)?
            .map_err(|_| DeviceError::Disconnected)?;
        match tokio::time::timeout_at(deadline, &mut start).await {
            Ok(result) => result.map_err(|_| DeviceError::Disconnected)?,
            Err(_) => {
                // Close admission before checking whether the actor claimed the command.
                start.close();
                start.try_recv().map_err(|_| DeviceError::Timeout)?;
            }
        }
        reply_rx.await.map_err(|_| DeviceError::Disconnected)?
    }
}

impl Default for Ch340RelayDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RelayDriver for Ch340RelayDriver {
    async fn open(&self, port: &str) -> DeviceResult<RelayInfo> {
        self.ensure_task(port)?;
        // 通道枚举：逐通道 STATUS 查询（只读，不切换）。
        let mut detected = Vec::new();
        for channel in 1..=ch340::MAX_CHANNELS {
            if let Some(on) = self.status(port, channel).await? {
                detected.push(ChannelInfo { channel, on });
            }
        }
        Ok(RelayInfo {
            port: port.to_string(),
            baud: ch340::DEFAULT_BAUD,
            detected,
        })
    }

    async fn set(&self, port: &str, channel: u8, on: bool) -> DeviceResult<()> {
        let channel = Self::valid_channel(channel)?;
        self.dispatch(port, |reply| PortCommand::Set { channel, on, reply })
            .await
    }

    async fn pulse(&self, port: &str, channel: u8, ms: u64) -> DeviceResult<()> {
        let channel = Self::valid_channel(channel)?;
        if ms == 0 || ms > PULSE_MAX_MS {
            return Err(DeviceError::Invalid(format!(
                "脉冲时长 {ms}ms 超出范围（1..={PULSE_MAX_MS}ms）"
            )));
        }
        self.dispatch(port, |reply| PortCommand::Pulse { channel, ms, reply })
            .await
    }

    async fn status(&self, port: &str, channel: u8) -> DeviceResult<Option<bool>> {
        let channel = Self::valid_channel(channel)?;
        self.dispatch(port, |reply| PortCommand::Status { channel, reply })
            .await
    }

    async fn close(&self, port: &str) -> DeviceResult<()> {
        self.dispatch(port, |reply| PortCommand::Close { reply })
            .await
    }
}

#[async_trait]
impl ControlBackend for Ch340RelayDriver {
    /// 电源通断：目标通道保持态 ON/OFF。
    async fn power(&self, target: &ControlTarget, on: bool) -> DeviceResult<()> {
        Self::assert_relay_target(target)?;
        let channel = Self::valid_channel(u8::try_from(target.channel).map_err(|_| {
            DeviceError::Invalid(format!("通道号 {} 超出继电器范围", target.channel))
        })?)?;
        RelayDriver::set(self, &target.address, channel, on).await
    }

    /// 复位：目标通道脉冲（默认 3000ms，可经 [`PulseTiming`] 调整）。
    async fn reset(&self, target: &ControlTarget) -> DeviceResult<()> {
        Self::assert_relay_target(target)?;
        let channel = Self::valid_channel(u8::try_from(target.channel).map_err(|_| {
            DeviceError::Invalid(format!("通道号 {} 超出继电器范围", target.channel))
        })?)?;
        let ms = self.timing.reset_ms;
        RelayDriver::pulse(self, &target.address, channel, ms).await
    }

    /// 电源按键：短按 = power_key 通道短脉冲（默认 300ms），
    /// 长按（强制断电）= 更长脉冲（默认 3000ms）。
    async fn power_key(&self, target: &ControlTarget, long_press: bool) -> DeviceResult<()> {
        Self::assert_relay_target(target)?;
        let channel = Self::valid_channel(u8::try_from(target.channel).map_err(|_| {
            DeviceError::Invalid(format!("通道号 {} 超出继电器范围", target.channel))
        })?)?;
        let ms = if long_press {
            self.timing.power_key_long_ms
        } else {
            self.timing.power_key_ms
        };
        RelayDriver::pulse(self, &target.address, channel, ms).await
    }

    /// 逐通道状态查询（只读）：1..=8 逐通道发 STATUS，返回应答的 `(通道, 吸合)`。
    /// 板卡不回答 STATUS 的通道跳过（`open()` 枚举同语义）。
    async fn channel_status(&self, address: &str) -> DeviceResult<Vec<(u8, bool)>> {
        let mut states = Vec::new();
        for channel in 1..=ch340::MAX_CHANNELS {
            if let Some(on) = RelayDriver::status(self, address, channel).await? {
                states.push((channel, on));
            }
        }
        Ok(states)
    }

    /// Keep the complete test on the port task so cancellation cannot skip release.
    async fn channel_test(&self, address: &str, channel: u8) -> DeviceResult<ChannelTestReport> {
        let channel = Self::valid_channel(channel)?;
        self.dispatch(address, |reply| PortCommand::ChannelTest { channel, reply })
            .await
    }

    /// Single-channel read-only probe for offline monitoring: exactly one
    /// STATUS frame, reusing the 300 ms echo timeout of [`RelayDriver::status`].
    /// `Ok(None)` (port alive, board silent) is a board fault for the monitor;
    /// open/IO failures surface as `Err` and are classified by the caller.
    async fn probe_channel(&self, address: &str, channel: u8) -> DeviceResult<Option<bool>> {
        let channel = Self::valid_channel(channel)?;
        RelayDriver::status(self, address, channel).await
    }
}

/// 串口独占 task 主循环：读写都在本 task 内完成（命令经 mpsc 派发）。
async fn run_port_task(
    port: String,
    baud: u32,
    opener: Opener,
    mut cmd_rx: mpsc::Receiver<QueuedCommand>,
) {
    let mut stream: Option<BoxStream> = None;
    while let Some(QueuedCommand { started, command }) = cmd_rx.recv().await {
        if started.send(()).is_err() {
            continue;
        }
        match command {
            PortCommand::ChannelTest { channel, reply } => {
                let result = test_channel(&mut stream, &opener, &port, baud, channel).await;
                let _ = reply.send(result);
            }
            PortCommand::Set { channel, on, reply } => {
                let result = set_channel(&mut stream, &opener, &port, baud, channel, on).await;
                let _ = reply.send(result);
            }
            PortCommand::Pulse { channel, ms, reply } => {
                // 吸合 → 保持 → 自动释放。释放失败时错误消息必须注明
                // 通道可能仍处于吸合态（如电源通道保持吸合会让 DUT 持续断电）。
                let result = pulse_channel(&mut stream, &opener, &port, baud, channel, ms).await;
                let _ = reply.send(result);
            }
            PortCommand::Status { channel, reply } => {
                let result =
                    query_status_with_retry(&mut stream, &opener, &port, baud, channel).await;
                let _ = reply.send(result);
            }
            PortCommand::Close { reply } => {
                let result = match stream.take() {
                    Some(mut s) => tokio::time::timeout(IO_TIMEOUT, s.shutdown())
                        .await
                        .map_err(|_| DeviceError::Timeout)
                        .and_then(|result| result.map_err(io_err)),
                    None => Ok(()),
                };
                let _ = reply.send(result);
            }
        }
    }
    // task 结束（驱动被丢弃）：兜底关闭句柄。
    if let Some(mut s) = stream.take() {
        let _ = s.shutdown().await;
    }
}

/// 脉冲：吸合 → 保持 ms → 自动释放。释放失败报 Invalid 并注明通道可能仍吸合。
async fn pulse_channel(
    stream: &mut Option<BoxStream>,
    opener: &Opener,
    port: &str,
    baud: u32,
    channel: u8,
    ms: u64,
) -> DeviceResult<()> {
    set_channel(stream, opener, port, baud, channel, true).await?;
    tokio::time::sleep(Duration::from_millis(ms)).await;
    if let Err(err) = set_channel(stream, opener, port, baud, channel, false).await {
        return Err(DeviceError::Invalid(format!(
            "脉冲释放失败（通道 {channel} 可能仍处于吸合状态，请检查继电器并手动恢复）：{err}"
        )));
    }
    Ok(())
}

/// 确保串口已打开（惰性；错误映射见 [`open_serial`]）。
async fn ensure_open(
    stream: &mut Option<BoxStream>,
    opener: &Opener,
    port: &str,
    baud: u32,
) -> DeviceResult<()> {
    if stream.is_none() {
        *stream = Some((opener)(port, baud)?);
    }
    Ok(())
}

/// 发送 ON/OFF 命令并读回校验。
async fn set_channel(
    stream: &mut Option<BoxStream>,
    opener: &Opener,
    port: &str,
    baud: u32,
    channel: u8,
    on: bool,
) -> DeviceResult<()> {
    ensure_open(stream, opener, port, baud).await?;
    let opcode = if on { ch340::OP_ON } else { ch340::OP_OFF };
    let result = tokio::time::timeout(
        IO_TIMEOUT,
        send_and_verify(
            stream.as_mut().expect("ensure_open initialized stream"),
            channel,
            opcode,
        ),
    )
    .await
    .unwrap_or(Err(DeviceError::Timeout));
    if result.is_err() {
        stream.take();
    }
    result
}

/// 写帧 → 冲刷 → 读回。
///
/// 多数板卡原样回显命令帧（Verified = 已接受）；静默板卡（无回显）与
/// 读回超时视为已接受。EOF 表示句柄失效；回显校验和
/// 错误 = 通信内容不可信，报 Disconnected 并附帧十六进制。
async fn send_and_verify(stream: &mut BoxStream, channel: u8, opcode: u8) -> DeviceResult<()> {
    let frame = ch340::frame(channel, opcode);
    stream.write_all(&frame).await.map_err(io_err)?;
    stream.flush().await.map_err(io_err)?;
    read_echo(stream, channel, opcode).await
}

/// Maximum accumulated echo/status bytes from a relay port before discarding
/// older noise so a chatty or miswired UART cannot grow memory without bound.
const MAX_ACC_BYTES: usize = 4096;

async fn read_echo(stream: &mut BoxStream, channel: u8, opcode: u8) -> DeviceResult<()> {
    let started = Instant::now();
    let mut acc = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let remaining = ECHO_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(()); // No echo: silent relay board treated as accepted.
        }
        match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Err(_elapsed) => return Ok(()),
            Ok(Err(err)) => return Err(io_err(err)),
            Ok(Ok(0)) => return Err(DeviceError::Disconnected),
            Ok(Ok(n)) => {
                acc.extend_from_slice(&buf[..n]);
                if acc.len() > MAX_ACC_BYTES {
                    let keep = acc.len() - MAX_ACC_BYTES;
                    acc.drain(..keep);
                }
                match ch340::classify_echo(&acc, channel, opcode) {
                    EchoCheck::Verified(_) => return Ok(()),
                    EchoCheck::ChecksumBad(_) => return Err(DeviceError::Disconnected),
                    EchoCheck::Mismatch(_) | EchoCheck::Truncated(_) => continue,
                    EchoCheck::Absent => continue,
                }
            }
        }
    }
}

/// 发送 STATUS 查询并扫描应答。
async fn query_status(
    stream: &mut Option<BoxStream>,
    opener: &Opener,
    port: &str,
    baud: u32,
    channel: u8,
) -> DeviceResult<Option<bool>> {
    ensure_open(stream, opener, port, baud).await?;
    let result = tokio::time::timeout(
        IO_TIMEOUT,
        read_status(
            stream.as_mut().expect("ensure_open initialized stream"),
            channel,
        ),
    )
    .await
    .unwrap_or(Err(DeviceError::Timeout));
    if result.is_err() {
        stream.take();
    }
    result
}

async fn query_status_with_retry(
    stream: &mut Option<BoxStream>,
    opener: &Opener,
    port: &str,
    baud: u32,
    channel: u8,
) -> DeviceResult<Option<bool>> {
    match query_status(stream, opener, port, baud, channel).await {
        Err(DeviceError::Disconnected) => query_status(stream, opener, port, baud, channel).await,
        result => result,
    }
}

async fn test_channel(
    stream: &mut Option<BoxStream>,
    opener: &Opener,
    port: &str,
    baud: u32,
    channel: u8,
) -> DeviceResult<ChannelTestReport> {
    let before = query_status_with_retry(stream, opener, port, baud, channel).await?;
    let on = match set_channel(stream, opener, port, baud, channel, true).await {
        Ok(()) => query_status_with_retry(stream, opener, port, baud, channel).await,
        Err(error) => Err(error),
    };
    // A failed write or verification can leave the channel energized.
    set_channel(stream, opener, port, baud, channel, false)
        .await
        .map_err(|error| DeviceError::Invalid(format!("通道 {channel} 测试释放失败：{error}")))?;
    let after = query_status_with_retry(stream, opener, port, baud, channel).await?;
    Ok(ChannelTestReport {
        before,
        on_ok: on? == Some(true),
        off_ok: after == Some(false),
        after,
    })
}

async fn read_status(stream: &mut BoxStream, channel: u8) -> DeviceResult<Option<bool>> {
    let frame = ch340::frame(channel, ch340::OP_STATUS);
    stream.write_all(&frame).await.map_err(io_err)?;
    stream.flush().await.map_err(io_err)?;

    let started = Instant::now();
    let mut acc = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let remaining = ECHO_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(None); // 板卡不回答 STATUS
        }
        match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Err(_elapsed) => return Ok(None),
            Ok(Err(err)) => return Err(io_err(err)),
            Ok(Ok(0)) => return Err(DeviceError::Disconnected),
            Ok(Ok(n)) => {
                acc.extend_from_slice(&buf[..n]);
                if acc.len() > MAX_ACC_BYTES {
                    let keep = acc.len() - MAX_ACC_BYTES;
                    acc.drain(..keep);
                }
                if let Some(state) = ch340::scan_status(&acc, channel) {
                    return Ok(Some(state == ch340::OP_ON));
                }
                // Continue accumulating because replies may arrive fragmented.
            }
        }
    }
}

/// 会话内 IO 错误（写/冲刷/读失败）→ Disconnected：串口通信中断
/// （USB 拔出、驱动复位、端口被抢占）。
fn io_err(_err: std::io::Error) -> DeviceError {
    DeviceError::Disconnected
}

#[cfg(feature = "real-hardware")]
/// tokio-serial 打开真实串口，错误按类别映射 DeviceError。
fn open_serial(port: &str, baud: u32) -> DeviceResult<BoxStream> {
    let stream =
        tokio_serial::SerialStream::open(&tokio_serial::new(port, baud)).map_err(map_open_error)?;
    Ok(Box::new(stream))
}

#[cfg(feature = "real-hardware")]
fn map_open_error(err: serialport::Error) -> DeviceError {
    use serialport::ErrorKind;
    match err.kind {
        ErrorKind::NoDevice => {
            // serialport 4.10 Windows 端（src/windows/error.rs）把
            // ERROR_FILE_NOT_FOUND / ERROR_ACCESS_DENIED 一律归为 NoDevice：
            // 端口不存在与被其他进程占用同源，按 OS 描述细分。
            let description = err.description.to_ascii_lowercase();
            if description.contains("denied") || description.contains("拒绝访问") {
                DeviceError::PermissionDenied
            } else {
                DeviceError::NotFound
            }
        }
        ErrorKind::InvalidInput => DeviceError::Invalid(err.description),
        ErrorKind::Io(io_kind) => match io_kind {
            std::io::ErrorKind::NotFound => DeviceError::NotFound,
            std::io::ErrorKind::PermissionDenied => DeviceError::PermissionDenied,
            std::io::ErrorKind::TimedOut => DeviceError::Timeout,
            _ => DeviceError::Disconnected,
        },
        ErrorKind::Unknown => DeviceError::Disconnected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use duthub_core::ApprovalError;
    use duthub_core::model::{
        DutConfig, FlashConfig, McpConfig, RelayChannels, RelayConfig, SerialConfig,
    };
    use duthub_device::control::{ControlAction, ControlEntryError, ControlService};

    /// 模拟板卡行为开关。
    #[derive(Clone)]
    struct MockBehavior {
        /// ON/OFF 命令原样回显。
        echo: bool,
        /// 回显时故意写坏校验字节（帧内容不可信 → 驱动必须报错）。
        corrupt_echo: bool,
        /// 只对 OFF 帧写坏校验字节（脉冲释放失败 → 驱动必须报告通道可能仍吸合）。
        corrupt_off: bool,
        /// 是否应答 STATUS。
        answer_status: bool,
        /// 只对 1..=n 的通道应答 STATUS（其余静默）；与 answer_status 组合使用。
        status_channels: u8,
        /// 继电器触点粘连：命令回显正常但 STATUS 恒报释放（ON 断言必须失败）。
        status_stuck_off: bool,
    }

    impl Default for MockBehavior {
        fn default() -> Self {
            Self {
                echo: true,
                corrupt_echo: false,
                corrupt_off: false,
                answer_status: false,
                status_channels: 0,
                status_stuck_off: false,
            }
        }
    }

    /// 模拟板卡收发记录。
    #[derive(Default)]
    struct MockLog {
        /// 收到的 (channel, opcode, checksum 是否正确) 序列。
        frames: Vec<(u8, u8, bool)>,
        /// 每次 OFF 的 (channel, 吸合毫秒数)。
        pulse_ms: Vec<(u8, u128)>,
    }

    /// 模拟板卡服务循环：解析 4 字节帧，按 behavior 决定回显/STATUS 应答。
    async fn serve_mock_relay(
        stream: tokio::io::DuplexStream,
        log: Arc<StdMutex<MockLog>>,
        behavior: MockBehavior,
    ) {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let mut state: HashMap<u8, bool> = HashMap::new();
        let mut on_at: HashMap<u8, Instant> = HashMap::new();
        let mut frame = [0u8; 4];
        loop {
            if rd.read_exact(&mut frame).await.is_err() {
                break;
            }
            let (ch, op) = (frame[1], frame[2]);
            let ck_ok = frame[3] == ch340::checksum(ch, op);
            log.lock().unwrap().frames.push((ch, op, ck_ok));
            if op == ch340::OP_STATUS {
                // 只对 1..=status_channels 的通道应答，其余静默（模拟不支持的
                // 通道）。应答帧：[A0, ch, state, A0+ch+state]。
                if behavior.answer_status && ck_ok && ch >= 1 && ch <= behavior.status_channels {
                    let actual = state.get(&ch).copied().unwrap_or(false);
                    let st = if behavior.status_stuck_off || !actual {
                        0
                    } else {
                        1
                    };
                    let resp = [ch340::HEADER, ch, st, ch340::checksum(ch, st)];
                    let _ = wr.write_all(&resp).await;
                }
            } else if op == ch340::OP_ON || op == ch340::OP_OFF {
                state.insert(ch, op == ch340::OP_ON);
                if behavior.echo && ck_ok {
                    let mut echo = ch340::frame(ch, op);
                    if (behavior.corrupt_echo && op == ch340::OP_ON)
                        || (behavior.corrupt_off && op == ch340::OP_OFF)
                    {
                        echo[3] ^= 0xFF;
                    }
                    let _ = wr.write_all(&echo).await;
                }
                if op == ch340::OP_ON {
                    on_at.insert(ch, Instant::now());
                } else if let Some(t) = on_at.remove(&ch) {
                    log.lock()
                        .unwrap()
                        .pulse_ms
                        .push((ch, t.elapsed().as_millis()));
                }
            }
        }
    }

    /// 构造注入 duplex 的 opener：每次 open 生成一对新管道 + 一个模拟板卡 task。
    fn mock_opener(behavior: MockBehavior) -> (Opener, Arc<StdMutex<MockLog>>) {
        let log = Arc::new(StdMutex::new(MockLog::default()));
        let log_for_opener = log.clone();
        let opener: Opener = Arc::new(move |_port: &str, _baud: u32| {
            let (client, server) = tokio::io::duplex(64);
            let log = log_for_opener.clone();
            let behavior = behavior.clone();
            tokio::spawn(serve_mock_relay(server, log, behavior));
            Ok(Box::new(client))
        });
        (opener, log)
    }

    fn control_target(address: &str, channel: u16) -> ControlTarget {
        ControlTarget {
            address: address.into(),
            driver: "CH340".into(),
            channel,
        }
    }

    fn test_driver(behavior: MockBehavior) -> (Ch340RelayDriver, Arc<StdMutex<MockLog>>) {
        let (opener, log) = mock_opener(behavior);
        (Ch340RelayDriver::with_opener(opener), log)
    }

    #[tokio::test]
    async fn unplugged_handle_is_reopened_without_replaying_an_ambiguous_write() {
        let good = MockBehavior {
            answer_status: true,
            status_channels: 4,
            ..Default::default()
        };
        let (connected, log) = mock_opener(good);
        let (disconnect_tx, disconnect_rx) = oneshot::channel::<()>();
        let disconnect_rx = StdMutex::new(Some(disconnect_rx));
        let opener: Opener = Arc::new(move |port, baud| {
            let Some(disconnect_rx) = disconnect_rx.lock().unwrap().take() else {
                return connected(port, baud);
            };
            let (client, mut peer) = tokio::io::duplex(64);
            tokio::spawn(async move {
                let mut frame = [0; 4];
                peer.read_exact(&mut frame).await.unwrap();
                peer.write_all(&frame).await.unwrap();
                let _ = disconnect_rx.await;
                // The device received the second command but vanished before acknowledgment.
                peer.read_exact(&mut frame).await.unwrap();
                assert_eq!(frame, ch340::frame(2, ch340::OP_ON));
            });
            Ok(Box::new(client))
        });
        let driver = Ch340RelayDriver::with_opener(opener);
        driver.set("MOCK0", 2, false).await.unwrap();
        disconnect_tx.send(()).unwrap();
        assert_eq!(
            driver.set("MOCK0", 2, true).await,
            Err(DeviceError::Disconnected)
        );
        assert!(
            log.lock().unwrap().frames.is_empty(),
            "the ON command must not replay"
        );
        driver.set("MOCK0", 2, false).await.unwrap();
        assert_eq!(driver.status("MOCK0", 2).await.unwrap(), Some(false));
        assert_eq!(
            log.lock().unwrap().frames.as_slice(),
            &[(2, ch340::OP_OFF, true), (2, ch340::OP_STATUS, true),]
        );
    }

    #[tokio::test]
    async fn set_frames_follow_channel_mapping() {
        let (driver, log) = test_driver(MockBehavior::default());
        driver.set("MOCK0", 4, true).await.unwrap();
        driver.set("MOCK0", 4, false).await.unwrap();
        driver.set("MOCK0", 2, true).await.unwrap();
        let frames = &log.lock().unwrap().frames;
        assert_eq!(
            frames,
            &[
                (4, ch340::OP_ON, true),
                (4, ch340::OP_OFF, true),
                (2, ch340::OP_ON, true)
            ]
        );
    }

    #[tokio::test]
    async fn control_backend_maps_power_reset_power_key() {
        let (driver, log) = test_driver(MockBehavior::default());
        // 缩短脉冲便于测试；断言短按不继承长按时长
        let driver = Arc::new(driver.with_timing(PulseTiming {
            power_key_ms: 40,
            power_key_long_ms: 120,
            reset_ms: 60,
        }));
        let backend: Arc<dyn ControlBackend> = driver.clone();
        backend
            .power(&control_target("MOCK0", 4), true)
            .await
            .unwrap();
        backend.reset(&control_target("MOCK0", 1)).await.unwrap();
        backend
            .power_key(&control_target("MOCK0", 2), false)
            .await
            .unwrap();
        backend
            .power_key(&control_target("MOCK0", 2), true)
            .await
            .unwrap();

        let guard = log.lock().unwrap();
        // power 是保持态（只 ON 不 OFF）；reset/power_key 是脉冲（ON+OFF）
        assert_eq!(
            guard.frames,
            &[
                (4, ch340::OP_ON, true),
                (1, ch340::OP_ON, true),
                (1, ch340::OP_OFF, true),
                (2, ch340::OP_ON, true),
                (2, ch340::OP_OFF, true),
                (2, ch340::OP_ON, true),
                (2, ch340::OP_OFF, true),
            ]
        );
        // 脉冲时长按通道断言：reset≥60ms，短按≥40ms，长按≥120ms，短按 < 长按
        let pulses = &guard.pulse_ms;
        assert_eq!(pulses.len(), 3);
        assert_eq!(pulses[0].0, 1);
        assert!(
            pulses[0].1 >= 60,
            "reset 脉冲应 ≥60ms，实际 {}ms",
            pulses[0].1
        );
        assert_eq!(pulses[1].0, 2);
        assert!(
            pulses[1].1 >= 40,
            "短按脉冲应 ≥40ms，实际 {}ms",
            pulses[1].1
        );
        assert_eq!(pulses[2].0, 2);
        assert!(
            pulses[2].1 >= 120,
            "长按脉冲应 ≥120ms，实际 {}ms",
            pulses[2].1
        );
        assert!(pulses[1].1 < pulses[2].1, "短按不得继承长按时长");
    }

    #[tokio::test]
    async fn unconfigured_and_out_of_range_channels_rejected_without_frames() {
        let (driver, log) = test_driver(MockBehavior::default());
        // 通道 0 = 未配置
        assert!(matches!(
            driver.set("MOCK0", 0, true).await,
            Err(DeviceError::Invalid(_))
        ));
        assert!(matches!(
            driver.status("MOCK0", 0).await,
            Err(DeviceError::Invalid(_))
        ));
        // 通道 0 / 超范围经 ControlBackend 入口同样拒绝
        let backend: Arc<dyn ControlBackend> = Arc::new(driver);
        assert!(matches!(
            backend.power(&control_target("MOCK0", 0), true).await,
            Err(DeviceError::Invalid(_))
        ));
        assert!(matches!(
            backend.power(&control_target("MOCK0", 300), true).await,
            Err(DeviceError::Invalid(_))
        ));
        // PDU 目标不得被串口驱动执行
        let pdu = ControlTarget {
            address: "192.168.1.100".into(),
            driver: "snmp-pdu".into(),
            channel: 3,
        };
        assert!(matches!(
            backend.power(&pdu, true).await,
            Err(DeviceError::Unsupported)
        ));
        assert!(
            log.lock().unwrap().frames.is_empty(),
            "被拒绝的动作不得发出任何协议帧"
        );
    }

    #[tokio::test]
    async fn silent_board_accepts_commands_without_echo() {
        let (driver, log) = test_driver(MockBehavior {
            echo: false,
            ..Default::default()
        });
        driver.set("MOCK0", 1, true).await.unwrap();
        driver.set("MOCK0", 1, false).await.unwrap();
        let frames = &log.lock().unwrap().frames;
        assert_eq!(frames, &[(1, ch340::OP_ON, true), (1, ch340::OP_OFF, true)]);
    }

    /// Offline monitoring uses `probe_channel`: exactly one STATUS frame, no
    /// switching, and a silent channel is `Ok(None)` (board fault, not error).
    #[tokio::test]
    async fn probe_channel_reads_one_status_without_switching() {
        let (driver, log) = test_driver(MockBehavior {
            answer_status: true,
            status_channels: 4,
            ..Default::default()
        });
        let backend: Arc<dyn ControlBackend> = Arc::new(driver);
        backend
            .power(&control_target("MOCK0", 4), true)
            .await
            .unwrap();
        log.lock().unwrap().frames.clear();

        assert_eq!(
            backend.probe_channel("MOCK0", 4).await.unwrap(),
            Some(true),
            "the answering channel reports its state"
        );
        assert_eq!(
            backend.probe_channel("MOCK0", 5).await.unwrap(),
            None,
            "a channel the board ignores is a silent board, not an error"
        );
        assert_eq!(
            log.lock().unwrap().frames,
            &[(4, ch340::OP_STATUS, true), (5, ch340::OP_STATUS, true)],
            "a probe must never emit ON/OFF frames"
        );
        // Channel 0 is rejected before any frame is sent (→ Unobservable).
        assert!(matches!(
            backend.probe_channel("MOCK0", 0).await,
            Err(DeviceError::Invalid(_))
        ));
        assert_eq!(log.lock().unwrap().frames.len(), 2);
    }

    #[tokio::test]
    async fn corrupt_echo_rejected_as_disconnected() {
        let (driver, _log) = test_driver(MockBehavior {
            corrupt_echo: true,
            ..Default::default()
        });
        let err = driver.set("MOCK0", 1, true).await.unwrap_err();
        assert!(matches!(err, DeviceError::Disconnected));
    }

    #[tokio::test]
    async fn pulse_release_failure_reports_maybe_stuck_channel() {
        let (driver, _log) = test_driver(MockBehavior {
            corrupt_off: true,
            ..Default::default()
        });
        let err = driver.pulse("MOCK0", 2, 20).await.unwrap_err();
        match &err {
            DeviceError::Invalid(msg) => {
                assert!(
                    msg.contains("仍处于吸合状态"),
                    "错误须提示通道可能仍吸合: {msg}"
                );
            }
            other => panic!("期望 Invalid(释放失败)，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_enumerates_status_channels() {
        let (driver, _log) = test_driver(MockBehavior {
            answer_status: true,
            status_channels: 3,
            ..Default::default()
        });
        let info = driver.open("MOCK0").await.unwrap();
        assert_eq!(info.baud, ch340::DEFAULT_BAUD);
        assert_eq!(
            info.detected,
            vec![
                ChannelInfo {
                    channel: 1,
                    on: false
                },
                ChannelInfo {
                    channel: 2,
                    on: false
                },
                ChannelInfo {
                    channel: 3,
                    on: false
                },
            ]
        );
    }

    // ── 继电器测试（channel_status / channel_test，经 ControlBackend 入口） ────

    #[tokio::test]
    async fn backend_channel_status_enumerates_answered_channels() {
        let (driver, log) = test_driver(MockBehavior {
            answer_status: true,
            status_channels: 3,
            ..Default::default()
        });
        let backend: Arc<dyn ControlBackend> = Arc::new(driver);
        assert_eq!(
            backend.channel_status("MOCK0").await.unwrap(),
            vec![(1, false), (2, false), (3, false)]
        );
        // 全程只读：不得出现任何 ON/OFF 帧
        assert!(
            log.lock()
                .unwrap()
                .frames
                .iter()
                .all(|&(_, op, _)| op == ch340::OP_STATUS),
            "channel_status 只允许发 STATUS 帧"
        );
    }

    #[tokio::test]
    async fn backend_channel_test_roundtrip_confirms_on_and_off() {
        let (driver, log) = test_driver(MockBehavior {
            answer_status: true,
            status_channels: 1,
            ..Default::default()
        });
        let backend: Arc<dyn ControlBackend> = Arc::new(driver);
        let report = backend.channel_test("MOCK0", 1).await.unwrap();
        assert_eq!(
            report,
            ChannelTestReport {
                before: Some(false),
                on_ok: true,
                off_ok: true,
                after: Some(false),
            },
            "往返报告：OFF → ON 确认 → OFF 确认恢复"
        );
        assert_eq!(
            log.lock().unwrap().frames,
            &[
                (1, ch340::OP_STATUS, true),
                (1, ch340::OP_ON, true),
                (1, ch340::OP_STATUS, true),
                (1, ch340::OP_OFF, true),
                (1, ch340::OP_STATUS, true),
            ],
            "帧序：STATUS(before) → ON → STATUS(on 断言) → OFF → STATUS(off 断言)"
        );
    }

    #[tokio::test]
    async fn backend_channel_test_reports_failed_on_but_still_releases() {
        // 触点粘连：命令回显正常，但 STATUS 恒报释放 → on_ok 必须为 false，
        // 且 OFF 恢复帧仍然发出（恢复优先于断言）。
        let (driver, log) = test_driver(MockBehavior {
            answer_status: true,
            status_channels: 1,
            status_stuck_off: true,
            ..Default::default()
        });
        let backend: Arc<dyn ControlBackend> = Arc::new(driver);
        let report = backend.channel_test("MOCK0", 1).await.unwrap();
        assert_eq!(
            report,
            ChannelTestReport {
                before: Some(false),
                on_ok: false,
                off_ok: true,
                after: Some(false),
            }
        );
        let frames = log.lock().unwrap().frames.clone();
        assert!(
            frames.contains(&(1, ch340::OP_OFF, true)),
            "on_ok 失败后仍必须发 OFF 恢复"
        );
    }

    #[tokio::test]
    async fn backend_channel_test_releases_after_an_on_verification_error() {
        let (driver, log) = test_driver(MockBehavior {
            answer_status: true,
            status_channels: 1,
            corrupt_echo: true,
            ..Default::default()
        });
        assert!(matches!(
            driver.channel_test("MOCK0", 1).await,
            Err(DeviceError::Disconnected)
        ));
        assert!(
            log.lock()
                .unwrap()
                .frames
                .contains(&(1, ch340::OP_OFF, true))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn long_pulse_waits_for_release() {
        let (driver, log) = test_driver(MockBehavior::default());
        let started = tokio::time::Instant::now();
        driver.pulse("MOCK0", 1, PULSE_MAX_MS).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(PULSE_MAX_MS));
        assert!(
            log.lock()
                .unwrap()
                .frames
                .contains(&(1, ch340::OP_OFF, true))
        );
    }

    #[tokio::test]
    async fn backend_channel_test_rejects_invalid_channel_without_frames() {
        let (driver, log) = test_driver(MockBehavior {
            answer_status: true,
            status_channels: 1,
            ..Default::default()
        });
        let backend: Arc<dyn ControlBackend> = Arc::new(driver);
        assert!(matches!(
            backend.channel_test("MOCK0", 0).await,
            Err(DeviceError::Invalid(_))
        ));
        assert!(
            log.lock().unwrap().frames.is_empty(),
            "通道 0 不得发出任何协议帧"
        );
    }

    #[tokio::test]
    async fn open_fails_when_port_unavailable() {
        let opener: Opener = Arc::new(|_port: &str, _baud: u32| Err(DeviceError::NotFound));
        let driver = Ch340RelayDriver::with_opener(opener);
        assert!(matches!(
            driver.open("COMX").await,
            Err(DeviceError::NotFound)
        ));
        assert!(matches!(
            driver.set("COMX", 1, true).await,
            Err(DeviceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn probe_port_sends_no_protocol_frames() {
        let (driver, log) = test_driver(MockBehavior::default());
        let probe = driver.probe_port("MOCK0", 9600).unwrap();
        assert_eq!(
            probe,
            PortProbe {
                port: "MOCK0".into(),
                baud: 9600
            }
        );
        assert!(
            log.lock().unwrap().frames.is_empty(),
            "open/close 探测不得发送任何协议帧"
        );
    }

    #[tokio::test]
    async fn close_releases_handle_and_next_command_reopens() {
        let (driver, log) = test_driver(MockBehavior::default());
        driver.set("MOCK0", 1, true).await.unwrap();
        driver.close("MOCK0").await.unwrap();
        driver.set("MOCK0", 1, false).await.unwrap();
        assert_eq!(log.lock().unwrap().frames.len(), 2);
        // 重复 close 幂等
        driver.close("MOCK0").await.unwrap();
    }

    // ── 授权拒绝路径（经共用设备操作入口 duthub-device::control::ControlService，
    //    审核报告 P1-3：授权在入口统一完成，驱动只收已解析目标） ────────────────

    fn dut_with_relay(id: &str) -> DutConfig {
        DutConfig {
            id: id.into(),
            name: "RK3576".into(),
            serial: SerialConfig {
                host: "local".into(),
                port: "COM3".into(),
                baud: 115_200,
                format: "8N1".into(),
                reference_log: None,
                reference_similarity: None,
                split_logs: false,
            },
            adb: Default::default(),
            relay: RelayConfig::UsbSerial {
                host: "local".into(),
                node: "MOCK0".into(),
                driver: "CH340".into(),
                channels: RelayChannels {
                    power: Some(4),
                    reset: Some(1),
                    recovery: None,
                    maskrom: None,
                    power_key: Some(2),
                },
            },
            mcp: McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8787/mcp".into(),
            },
            a2a: Default::default(),
            flash: FlashConfig::default(),
            media: Default::default(),
            heartbeat: Default::default(),
        }
    }

    fn control_service_with_driver(
        driver: Ch340RelayDriver,
    ) -> (ControlService, Arc<duthub_core::ApprovalManager>) {
        let approval = Arc::new(duthub_core::ApprovalManager::new());
        let backend: Arc<dyn ControlBackend> = Arc::new(driver);
        (
            ControlService::new(Some(backend), approval.clone()),
            approval,
        )
    }

    /// 授权路径测试夹具：ControlService 是同步门面（内部 block_on，见
    /// duthub-device::control），这些测试必须用普通 `#[test]` 而非
    /// `#[tokio::test]`——驱动的 task 都在 ControlService 的 runtime 上派生。
    /// 脉冲缩短避免测试拖慢。
    fn auth_driver() -> (Ch340RelayDriver, Arc<StdMutex<MockLog>>) {
        let (driver, log) = test_driver(MockBehavior::default());
        let driver = driver.with_timing(PulseTiming {
            power_key_ms: 20,
            power_key_long_ms: 40,
            reset_ms: 30,
        });
        (driver, log)
    }

    #[test]
    fn missing_token_rejected_before_reaching_relay() {
        let (driver, log) = auth_driver();
        let (svc, _approval) = control_service_with_driver(driver);
        svc.register_dut(dut_with_relay("dut-a"));

        let err = svc
            .execute_by_id("dut-a", ControlAction::Power { on: true }, "")
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Approval(ApprovalError::Missing)
        ));
        assert!(
            log.lock().unwrap().frames.is_empty(),
            "无令牌不得触达继电器"
        );
    }

    #[test]
    fn mismatched_token_rejected_before_reaching_relay() {
        let (driver, log) = auth_driver();
        let (svc, approval) = control_service_with_driver(driver);
        svc.register_dut(dut_with_relay("dut-a"));

        // 令牌签给 dut-b / power=false，与本次操作（dut-a / power=true）不匹配
        let token = approval.issue("dut-b", "control.power", r#"{"on":false}"#);
        let err = svc
            .execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Approval(ApprovalError::Mismatch)
        ));
        assert!(log.lock().unwrap().frames.is_empty());
    }

    #[test]
    fn valid_token_reaches_relay_with_resolved_channel() {
        let (driver, log) = auth_driver();
        let (svc, approval) = control_service_with_driver(driver);
        svc.register_dut(dut_with_relay("dut-a"));

        // power → power 通道 4
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap();
        // reset → reset 通道 1（脉冲）
        let token = approval.issue("dut-a", "control.reset", "{}");
        svc.execute_by_id("dut-a", ControlAction::Reset, &token.token)
            .unwrap();
        // power_key 长按 → power_key 通道 2（脉冲）
        let token = approval.issue("dut-a", "control.power_key", r#"{"long_press":true}"#);
        svc.execute_by_id(
            "dut-a",
            ControlAction::PowerKey { long_press: true },
            &token.token,
        )
        .unwrap();

        let frames_before;
        {
            let guard = log.lock().unwrap();
            let frames = &guard.frames;
            assert_eq!(frames[0], (4, ch340::OP_ON, true), "power → 通道 4");
            assert_eq!(frames[1], (1, ch340::OP_ON, true), "reset → 通道 1 脉冲");
            assert_eq!(frames[2], (1, ch340::OP_OFF, true));
            assert_eq!(frames[3], (2, ch340::OP_ON, true), "power_key → 通道 2");
            assert_eq!(frames[4], (2, ch340::OP_OFF, true));
            frames_before = guard.frames.len();
        }

        // 单次消费：令牌重放必须拒绝且不再触达继电器
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap();
        assert!(matches!(
            svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token),
            Err(ControlEntryError::Approval(ApprovalError::Unknown))
        ));
        assert_eq!(log.lock().unwrap().frames.len(), frames_before + 1);
    }

    #[test]
    fn unbound_dut_rejected_without_consuming_token_or_reaching_relay() {
        let (driver, log) = auth_driver();
        let (svc, approval) = control_service_with_driver(driver);
        let mut dut = dut_with_relay("dut-b");
        dut.relay = RelayConfig::Disabled;
        svc.register_dut(dut);

        let token = approval.issue("dut-b", "control.power", r#"{"on":true}"#);
        let err = svc
            .execute_by_id("dut-b", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Device(DeviceError::Invalid(_))
        ));
        // 绑定缺失：令牌不消费、继电器不触达，修正配置后可重试
        assert!(
            approval
                .validate(&token.token, "dut-b", "control.power", r#"{"on":true}"#)
                .is_ok()
        );
        assert!(log.lock().unwrap().frames.is_empty());
    }

    #[test]
    fn missing_channel_rejected_before_reaching_relay() {
        let (driver, log) = auth_driver();
        let (svc, approval) = control_service_with_driver(driver);
        let mut dut = dut_with_relay("dut-c");
        if let RelayConfig::UsbSerial { channels, .. } = &mut dut.relay {
            channels.power = None; // 未配置电源通道
        }
        svc.register_dut(dut);

        let token = approval.issue("dut-c", "control.power", r#"{"on":true}"#);
        let err = svc
            .execute_by_id("dut-c", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Device(DeviceError::Invalid(_))
        ));
        assert!(log.lock().unwrap().frames.is_empty());
    }
}
