//! The `RelayDriver` interface and CH340-compatible implementation.
//!
//! Each serial port has one Tokio task that owns the handle.
//! Callers submit commands to that task, which performs every read and write.
//! Map transport and protocol failures to `DeviceError`.
//!
//! Implement `duthub_core::ControlBackend` for the shared control service.
//! The service checks authorization, binding, and capabilities before dispatch.
//! The driver receives the resolved target and never handles approval tokens.
//! The desktop injects this driver at startup.
//!
//! Use the injected `Opener` to select the serial transport.
//! Hardware builds use Tokio serial. Tests use `tokio::io::duplex`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use duthub_core::backend::{ChannelTestReport, ControlBackend, ControlTarget};
use duthub_core::error::{DeviceError, DeviceResult};

use crate::ch340::{self, EchoCheck};

/// At 9,600 baud an echo takes about 4 ms. Allow 300 ms for board processing.
/// Boards that do not echo commands incur one timeout per command.
const ECHO_TIMEOUT: Duration = Duration::from_millis(300);
/// Maximum time to wait for the port task to start a queued command.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(30);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap pulse length so an invalid duration cannot monopolize the port task.
const PULSE_MAX_MS: u64 = 60_000;

/// Asynchronous relay transport. Hardware builds use a serial stream.
/// Tests inject one end of a duplex stream.
pub trait RelayStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> RelayStream for T {}

/// Boxed serial byte stream.
pub type BoxStream = Box<dyn RelayStream>;

/// Open an exclusive stream at the specified port and baud rate.
pub type Opener = Arc<dyn Fn(&str, u32) -> DeviceResult<BoxStream> + Send + Sync>;

/// Pulse durations in milliseconds. A pulse energizes, holds, and releases the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PulseTiming {
    /// Short `power_key` press, 300 ms by default.
    pub power_key_ms: u64,
    /// Long `power_key` press, 3,000 ms by default.
    pub power_key_long_ms: u64,
    /// Reset pulse, 3,000 ms by default.
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

/// Channel discovery results from `RelayDriver::open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayInfo {
    /// Serial port name, such as COM3.
    pub port: String,
    /// Protocol baud rate, fixed at 9,600.
    pub baud: u32,
    /// One-based channels that answered the status query.
    /// Boards that do not answer still accept commands without confirmation.
    pub detected: Vec<ChannelInfo>,
}

/// State reported for one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    /// One-based channel number.
    pub channel: u8,
    /// Whether the relay is energized.
    pub on: bool,
}

/// Result of opening and closing a port without sending protocol frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortProbe {
    pub port: String,
    pub baud: u32,
}

/// Relay protocol driver addressed by serial port name.
#[async_trait]
pub trait RelayDriver: Send + Sync {
    /// Open the port and query channels 1 through 8 without changing their states.
    /// Report a failed port open as an error.
    /// A board that does not respond to status queries yields an empty channel list.
    async fn open(&self, port: &str) -> DeviceResult<RelayInfo>;

    /// Set the persistent state of a one-based channel.
    async fn set(&self, port: &str, channel: u8, on: bool) -> DeviceResult<()>;

    /// Energize a relay for the requested duration, then release it.
    /// The port task owns the pulse and still releases it if the caller drops its future.
    /// Report a failed release because the relay may remain energized.
    async fn pulse(&self, port: &str, channel: u8, ms: u64) -> DeviceResult<()>;

    /// Query one channel. A board that does not answer yields `Ok(None)`.
    async fn status(&self, port: &str, channel: u8) -> DeviceResult<Option<bool>>;

    /// Close the handle if present. The next command opens it again.
    async fn close(&self, port: &str) -> DeviceResult<()>;
}

/// Four-byte relay protocol used by CH340-compatible boards.
pub struct Ch340RelayDriver {
    ports: Mutex<HashMap<String, PortHandle>>,
    opener: Opener,
    timing: PulseTiming,
}

impl std::fmt::Debug for Ch340RelayDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ch340RelayDriver")
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct PortHandle {
    cmd_tx: mpsc::Sender<QueuedCommand>,
}

/// Commands for the serial port task. Return each result over a oneshot channel.
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
    /// Open a physical serial port using Tokio serial.
    /// Without the hardware feature, opening a port returns `DeviceError::Unsupported`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_opener(Self::default_opener())
    }

    /// Inject an opener for testing or an alternate transport.
    pub fn with_opener(opener: Opener) -> Self {
        Self {
            ports: Mutex::new(HashMap::new()),
            opener,
            timing: PulseTiming::default(),
        }
    }

    /// Override short press, long press, and reset times. Zero selects the default.
    #[must_use]
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

    /// Probe a port by opening and closing its handle without sending protocol frames.
    /// The protocol-level open operation also sends status queries.
    pub fn probe_port(&self, port: &str, baud: u32) -> DeviceResult<PortProbe> {
        let stream = (self.opener)(port, baud)?;
        drop(stream);
        Ok(PortProbe {
            port: port.to_string(),
            baud,
        })
    }

    /// Get or start the port task. Closing the driver's command channel lets the task exit.
    fn ensure_task(&self, port: &str) -> PortHandle {
        let mut ports = duthub_core::util::lock_unpoisoned(&self.ports);
        if let Some(handle) = ports.get(port)
            && !handle.cmd_tx.is_closed()
        {
            return handle.clone();
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
        handle
    }

    /// Validate the one-based channel number against the protocol limit of 255.
    fn valid_channel(channel: u8) -> DeviceResult<u8> {
        if channel == 0 {
            return Err(DeviceError::Invalid(
                "Invalid relay channel. Channels start at 1".into(),
            ));
        }
        Ok(channel)
    }

    /// Reject targets other than serial relays before opening a port.
    /// A separate backend handles SNMP PDUs.
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
        let handle = self.ensure_task(port);
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
        if let Ok(result) = tokio::time::timeout_at(deadline, &mut start).await {
            result.map_err(|_| DeviceError::Disconnected)?;
        } else {
            // Close admission before checking whether the actor claimed the command.
            start.close();
            start.try_recv().map_err(|_| DeviceError::Timeout)?;
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
        let _ = self.ensure_task(port);
        // Query each channel's status without switching it.
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
                "Pulse duration {ms} ms is outside the range 1 through {PULSE_MAX_MS} ms"
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
    /// Set the configured power relay to the persistent ON or OFF state.
    async fn power(&self, target: &ControlTarget, on: bool) -> DeviceResult<()> {
        Self::assert_relay_target(target)?;
        let channel = Self::valid_channel(u8::try_from(target.channel).map_err(|_| {
            DeviceError::Invalid(format!("Relay channel {} is out of range", target.channel))
        })?)?;
        RelayDriver::set(self, &target.address, channel, on).await
    }

    /// Pulse the reset relay using the duration from `PulseTiming`.
    async fn reset(&self, target: &ControlTarget) -> DeviceResult<()> {
        Self::assert_relay_target(target)?;
        let channel = Self::valid_channel(u8::try_from(target.channel).map_err(|_| {
            DeviceError::Invalid(format!("Relay channel {} is out of range", target.channel))
        })?)?;
        let ms = self.timing.reset_ms;
        RelayDriver::pulse(self, &target.address, channel, ms).await
    }

    /// A short press pulses the power key for 300 ms by default.
    /// A long press normally holds it for 3,000 ms to force a power-off.
    async fn power_key(&self, target: &ControlTarget, long_press: bool) -> DeviceResult<()> {
        Self::assert_relay_target(target)?;
        let channel = Self::valid_channel(u8::try_from(target.channel).map_err(|_| {
            DeviceError::Invalid(format!("Relay channel {} is out of range", target.channel))
        })?)?;
        let ms = if long_press {
            self.timing.power_key_long_ms
        } else {
            self.timing.power_key_ms
        };
        RelayDriver::pulse(self, &target.address, channel, ms).await
    }

    /// Read status for channels 1 through 8.
    /// The result includes only channels that answer the query.
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

/// The port task owns all serial reads and writes and receives commands over its channel.
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
                // Energize, hold, and release the relay.
                // Report release failures because the channel may remain energized.
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
    // Close the serial handle on task exit.
    if let Some(mut s) = stream.take() {
        let _ = s.shutdown().await;
    }
}

/// Pulse the relay and report a release failure if its final state is uncertain.
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
            "Pulse release failed. Channel {channel} may still be energized and requires manual inspection: {err}"
        )));
    }
    Ok(())
}

/// Open the serial handle if needed.
fn ensure_open(
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

/// Send an ON or OFF command and check its echo.
async fn set_channel(
    stream: &mut Option<BoxStream>,
    opener: &Opener,
    port: &str,
    baud: u32,
    channel: u8,
    on: bool,
) -> DeviceResult<()> {
    ensure_open(stream, opener, port, baud)?;
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

/// Write and flush a frame, then read its echo.
///
/// A valid echo confirms acceptance. Silent boards and echo timeouts are accepted without verification.
/// EOF indicates that the serial handle is no longer usable.
/// A bad echo checksum returns a disconnection error with the raw frame in hex.
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
                    EchoCheck::Mismatch(_) | EchoCheck::Truncated(_) | EchoCheck::Absent => {}
                }
            }
        }
    }
}

/// Send a channel status query and scan for its reply.
async fn query_status(
    stream: &mut Option<BoxStream>,
    opener: &Opener,
    port: &str,
    baud: u32,
    channel: u8,
) -> DeviceResult<Option<bool>> {
    ensure_open(stream, opener, port, baud)?;
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
        .map_err(|error| {
            DeviceError::Invalid(format!(
                "Failed to release channel {channel} after the test: {error}"
            ))
        })?;
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
            return Ok(None); // The board did not answer the query.
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

/// Map read, write, and flush failures to a disconnected transport.
/// USB removal, driver resets, and port loss can cause these failures.
fn io_err(_err: std::io::Error) -> DeviceError {
    DeviceError::Disconnected
}

#[cfg(feature = "real-hardware")]
/// Open a physical serial port and map the transport error.
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
            // On Windows, serialport maps both file-not-found and access-denied errors
            // to the same NoDevice category.
            // Use the operating-system message to distinguish a missing port from denied access.
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
    use duthub_core::HeartbeatConfig;
    use duthub_core::model::{
        AdbConfig, DutA2aConfig, DutConfig, DutMediaConfig, FlashConfig, McpConfig, RelayChannels,
        RelayConfig, SerialConfig,
    };
    use duthub_device::control::{ControlAction, ControlEntryError, ControlService};

    /// Simulated relay board fault-injection toggles for unit tests.
    #[derive(Clone)]
    #[expect(
        clippy::struct_excessive_bools,
        reason = "test mock fault-injection switches"
    )]
    struct MockBehavior {
        /// Echo ON and OFF commands unchanged.
        echo: bool,
        /// Corrupt the echo checksum to verify the driver rejects it.
        corrupt_echo: bool,
        /// Corrupt only the OFF echo to simulate a failed release.
        corrupt_off: bool,
        /// Whether the board answers status queries.
        answer_status: bool,
        /// When status replies are enabled, answer only for channels 1 through n.
        status_channels: u8,
        /// Simulate a stuck contact by echoing commands while reporting the relay as released.
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

    /// Traffic recorded by the simulated relay board.
    #[derive(Default)]
    struct MockLog {
        /// Received channel and opcode pairs with their checksum validity.
        frames: Vec<(u8, u8, bool)>,
        /// Channel and energization duration in milliseconds for each OFF command.
        pulse_ms: Vec<(u8, u128)>,
    }

    /// Parse four-byte frames and respond according to the configured board behavior.
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
            match op {
                ch340::OP_STATUS => {
                    // Answer only for supported channels. Unsupported channels stay silent.
                    // Build the response from the header, channel, state, and their checksum.
                    if behavior.answer_status && ck_ok && ch >= 1 && ch <= behavior.status_channels
                    {
                        let actual = state.get(&ch).copied().unwrap_or(false);
                        let st = u8::from(!behavior.status_stuck_off && actual);
                        let resp = [ch340::HEADER, ch, st, ch340::checksum(ch, st)];
                        let _ = wr.write_all(&resp).await;
                    }
                }
                ch340::OP_ON | ch340::OP_OFF => {
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
                _ => {}
            }
        }
    }

    /// Create a fresh duplex pair and simulated board task whenever the port opens.
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
        // Shorten test pulses and verify a short press does not use the long-press duration.
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
        // Power stays latched. Reset and the power key send an ON followed by an OFF.
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
        // Verify the reset, short-press, and long-press durations by channel.
        let pulses = &guard.pulse_ms;
        assert_eq!(pulses.len(), 3);
        assert_eq!(pulses[0].0, 1);
        assert!(
            pulses[0].1 >= 60,
            "Reset pulse must last at least 60 ms, got {} ms",
            pulses[0].1
        );
        assert_eq!(pulses[1].0, 2);
        assert!(
            pulses[1].1 >= 40,
            "Short press must last at least 40 ms, got {} ms",
            pulses[1].1
        );
        assert_eq!(pulses[2].0, 2);
        assert!(
            pulses[2].1 >= 120,
            "Long press must last at least 120 ms, got {} ms",
            pulses[2].1
        );
        assert!(
            pulses[1].1 < pulses[2].1,
            "Short presses must not use the long-press duration"
        );
    }

    #[tokio::test]
    async fn unconfigured_and_out_of_range_channels_rejected_without_frames() {
        let (driver, log) = test_driver(MockBehavior::default());
        // Channel zero means no channel is configured.
        assert!(matches!(
            driver.set("MOCK0", 0, true).await,
            Err(DeviceError::Invalid(_))
        ));
        assert!(matches!(
            driver.status("MOCK0", 0).await,
            Err(DeviceError::Invalid(_))
        ));
        // The shared backend also rejects zero and out-of-range channel numbers.
        let backend: Arc<dyn ControlBackend> = Arc::new(driver);
        assert!(matches!(
            backend.power(&control_target("MOCK0", 0), true).await,
            Err(DeviceError::Invalid(_))
        ));
        assert!(matches!(
            backend.power(&control_target("MOCK0", 300), true).await,
            Err(DeviceError::Invalid(_))
        ));
        // Reject a PDU target before it reaches the serial relay.
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
            "Rejected actions must not send protocol frames"
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
                    msg.contains("still be energized"),
                    "The error must warn that the channel may still be energized: {msg}"
                );
            }
            other => panic!("Expected Invalid for release failure, got {other:?}"),
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

    // Relay status and test calls through the shared control backend.

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
        // The status check must send no ON or OFF frames.
        assert!(
            log.lock()
                .unwrap()
                .frames
                .iter()
                .all(|&(_, op, _)| op == ch340::OP_STATUS),
            "channel_status may send status frames only"
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
            "The report must confirm the ON and OFF transitions"
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
            "Frame sequence must be status, ON, status, OFF, status"
        );
    }

    #[tokio::test]
    async fn backend_channel_test_reports_failed_on_but_still_releases() {
        // The mocked board echoes commands but keeps reporting a released relay.
        // The ON check fails, and the driver must still send OFF to restore the relay.
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
            "A failed ON verification must still send OFF"
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
            "Channel zero must not send protocol frames"
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
            "The port probe must not send protocol frames"
        );
    }

    #[tokio::test]
    async fn close_releases_handle_and_next_command_reopens() {
        let (driver, log) = test_driver(MockBehavior::default());
        driver.set("MOCK0", 1, true).await.unwrap();
        driver.close("MOCK0").await.unwrap();
        driver.set("MOCK0", 1, false).await.unwrap();
        assert_eq!(log.lock().unwrap().frames.len(), 2);
        // Closing a closed port is harmless.
        driver.close("MOCK0").await.unwrap();
    }

    // Exercise denied operations through the shared control service.
    // The driver sees only authorized operations on resolved targets.

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
            adb: AdbConfig::default(),
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
            a2a: DutA2aConfig::default(),
            flash: FlashConfig::default(),
            media: DutMediaConfig::default(),
            heartbeat: HeartbeatConfig::default(),
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

    /// The control service is synchronous and runs its own Tokio runtime.
    /// Use a regular test so the driver tasks run on that runtime.
    /// A Tokio test would add a nested runtime.
    /// Shorten pulses to keep the tests fast.
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
            "A missing token must block relay dispatch"
        );
    }

    #[test]
    fn mismatched_token_rejected_before_reaching_relay() {
        let (driver, log) = auth_driver();
        let (svc, approval) = control_service_with_driver(driver);
        svc.register_dut(dut_with_relay("dut-a"));

        // The token authorizes power-off for dut-b while the request powers on dut-a.
        let token = approval.issue("dut-b", "control.power", r#"{"on":false}"#);
        let err = svc
            .execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap_err();
        assert!(matches!(
            err,
            ControlEntryError::Approval(ApprovalError::Mismatch)
        ));
        assert_eq!(log.lock().unwrap().frames, [] as [(u8, u8, bool); 0]);
    }

    #[test]
    fn valid_token_reaches_relay_with_resolved_channel() {
        let (driver, log) = auth_driver();
        let (svc, approval) = control_service_with_driver(driver);
        svc.register_dut(dut_with_relay("dut-a"));

        // Power maps to channel 4.
        let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
        svc.execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
            .unwrap();
        // Reset pulses channel 1.
        let token = approval.issue("dut-a", "control.reset", "{}");
        svc.execute_by_id("dut-a", ControlAction::Reset, &token.token)
            .unwrap();
        // A long power-key press pulses channel 2.
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
            assert_eq!(
                frames[0],
                (4, ch340::OP_ON, true),
                "Power must use channel 4"
            );
            assert_eq!(
                frames[1],
                (1, ch340::OP_ON, true),
                "Reset must pulse channel 1"
            );
            assert_eq!(frames[2], (1, ch340::OP_OFF, true));
            assert_eq!(
                frames[3],
                (2, ch340::OP_ON, true),
                "Power key must use channel 2"
            );
            assert_eq!(frames[4], (2, ch340::OP_OFF, true));
            frames_before = guard.frames.len();
        }

        // A consumed token cannot trigger the relay again.
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
        // A missing binding preserves the token so the request can be retried after configuration.
        assert!(
            approval
                .validate(&token.token, "dut-b", "control.power", r#"{"on":true}"#)
                .is_ok()
        );
        assert_eq!(log.lock().unwrap().frames, [] as [(u8, u8, bool); 0]);
    }

    #[test]
    fn missing_channel_rejected_before_reaching_relay() {
        let (driver, log) = auth_driver();
        let (svc, approval) = control_service_with_driver(driver);
        let mut dut = dut_with_relay("dut-c");
        if let RelayConfig::UsbSerial { channels, .. } = &mut dut.relay {
            channels.power = None; // No power channel configured.
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
        assert_eq!(log.lock().unwrap().frames, [] as [(u8, u8, bool); 0]);
    }
}
