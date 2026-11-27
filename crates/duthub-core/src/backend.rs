//! Device backend DTOs and the shared `ControlBackend` trait.
//!
//! Real hardware implementations live behind `--features real-hardware`; tests
//! default to demo/mock backends.

use std::borrow::Borrow;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{DeviceError, DeviceResult};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DutId(pub String);

impl DutId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DutId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for DutId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for DutId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DutId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for DutId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SerialPortInfo {
    pub port: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdbDevice {
    pub serial: String,
    /// State from `adb devices -l`; write operations require `state == "device"`.
    pub state: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u64,
}

/// Resolved target for a control action: after the shared device entry point verifies
/// authorization, binding, and capabilities, it resolves the DUT configuration into a device
/// address and channel that drivers use directly. Drivers never touch approval tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlTarget {
    /// Resolved device address (relay serial node or SNMP PDU address).
    pub address: String,
    /// Board protocol driver identifier (e.g. `CH340`, or `"snmp-pdu"` for SNMP PDUs).
    pub driver: String,
    /// Target channel or outlet number (relay channel or PDU outlet).
    pub channel: u16,
}

/// Report for a single-channel ON -> STATUS -> OFF round-trip relay test (recorded end-to-end).
///
/// - `before`: channel state before the round-trip (`None` when the board does not answer STATUS,
///   in which case ON/OFF assertions cannot be trusted and `on_ok`/`off_ok` are both `false`).
/// - `on_ok`: STATUS readback after ON confirmed the channel is closed/energized.
/// - `off_ok`: STATUS readback after OFF confirmed the channel is released.
/// - `after`: final STATUS readback (restored state; relay tests always restore to released).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelTestReport {
    pub before: Option<bool>,
    pub on_ok: bool,
    pub off_ok: bool,
    pub after: Option<bool>,
}

#[async_trait]
pub trait ControlBackend: Send + Sync {
    /// Switch the target channel/outlet on or off. Authorization and binding checks run in the
    /// shared entry point; drivers only execute the action.
    async fn power(&self, target: &ControlTarget, on: bool) -> DeviceResult<()>;
    /// Issue a reset pulse (relay drivers only; PDUs do not emulate reset).
    async fn reset(&self, target: &ControlTarget) -> DeviceResult<()>;
    /// Issue a power-key pulse (`long_press` = long press to force power off; relay drivers only).
    async fn power_key(&self, target: &ControlTarget, long_press: bool) -> DeviceResult<()>;

    /// Query per-channel relay state (read-only, polling `OP_STATUS` per channel). Returns
    /// `(channel, closed)` pairs for channels that answered STATUS, or an empty list if the board
    /// does not answer STATUS. Implemented by relay drivers only; other backends such as PDUs
    /// return [`DeviceError::Unsupported`].
    async fn channel_status(&self, _address: &str) -> DeviceResult<Vec<(u8, bool)>> {
        Err(DeviceError::Unsupported)
    }

    /// Run a single-channel ON -> STATUS -> OFF round-trip relay test (write action). Even if the
    /// ON assertion fails, OFF must still run to restore the released state (recovery takes
    /// priority over assertion).
    async fn channel_test(&self, _address: &str, _channel: u8) -> DeviceResult<ChannelTestReport> {
        Err(DeviceError::Unsupported)
    }

    /// Single-channel read-only status probe (relay offline monitoring).
    ///
    /// `Ok(Some(on))` = the board answered STATUS; `Ok(None)` = the port is
    /// present but the board stays silent; `Err(..)` = transport / port
    /// failure. Unlike [`Self::channel_status`] (which polls every channel and
    /// therefore holds the port for several seconds), this issues exactly one
    /// STATUS frame so a monitor can keep the write gate for ~0.3 s. Backends
    /// without a per-channel read path keep the default (`Unsupported`), which
    /// the monitor classifies as `Unobservable` instead of a fault.
    async fn probe_channel(&self, _address: &str, _channel: u8) -> DeviceResult<Option<bool>> {
        Err(DeviceError::Unsupported)
    }
}
