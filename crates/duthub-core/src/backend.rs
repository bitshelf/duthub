//! 设备后端 trait。
//!
//! 契约要点：
//! - 真实硬件实现放在 `--features real-hardware` 之后；测试默认用 Demo/Mock。
//! - 烧录**不**在本 trait 暴露写入接口：真实刷写走独立的特权接口 + 审批。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

use crate::error::{DeviceError, DeviceResult};
use crate::model::DutConfig;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DutId(pub String);

impl DutId {
    pub fn as_str(&self) -> &str {
        &self.0
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
    /// `adb devices -l` 的 state；写入操作前必须校验 == "device"。
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

#[async_trait]
pub trait SerialBackend: Send + Sync {
    async fn list(&self) -> DeviceResult<Vec<SerialPortInfo>>;
    async fn open(&self, dut: &DutId, config: &DutConfig) -> DeviceResult<()>;
    async fn write(&self, dut: &DutId, data: &[u8]) -> DeviceResult<()>;
    async fn close(&self, dut: &DutId) -> DeviceResult<()>;
}

#[async_trait]
pub trait AdbBackend: Send + Sync {
    async fn devices(&self, executable: &Path) -> DeviceResult<Vec<AdbDevice>>;
}

#[async_trait]
pub trait FlashExecutor: Send + Sync {
    /// 只允许帮助/版本类参数的只读测试；参数白名单在调用方（duthub-flash） enforce。
    async fn test(
        &self,
        executable: &Path,
        args: &[String],
        timeout: Duration,
    ) -> DeviceResult<ProcessResult>;
}

/// 控制动作的已解析目标：共用设备操作入口完成授权/绑定/能力校验后，
/// 把 DUT 配置解析成驱动可直接使用的设备地址与通道，驱动**不接触**审批令牌。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlTarget {
    /// 已解析的设备地址（继电器串口节点 / SNMP PDU 地址）
    pub address: String,
    /// 板卡协议驱动标识（如 CH340；SNMP PDU 为 "snmp-pdu"）
    pub driver: String,
    /// 动作对应的通道或插座号（继电器通道 / PDU 插座）
    pub channel: u16,
}

/// 单通道 ON→STATUS→OFF 往返测试报告（继电器测试，全程记录）。
///
/// - `before`：往返前通道状态（板卡不回答 STATUS 时为 `None`，此时 ON/OFF
///   断言不可信，`on_ok`/`off_ok` 必为 `false`）；
/// - `on_ok`：ON 后 STATUS 读回确认吸合；
/// - `off_ok`：OFF 后 STATUS 读回确认释放；
/// - `after`：最后一次 STATUS 读回（恢复态，继电器测试要求恒为释放）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelTestReport {
    pub before: Option<bool>,
    pub on_ok: bool,
    pub off_ok: bool,
    pub after: Option<bool>,
}

#[async_trait]
pub trait ControlBackend: Send + Sync {
    /// 通断目标通道/插座。授权与绑定校验在共用入口完成；驱动只执行动作。
    async fn power(&self, target: &ControlTarget, on: bool) -> DeviceResult<()>;
    /// 复位脉冲（仅继电器类驱动支持；PDU 不伪造 Reset）。
    async fn reset(&self, target: &ControlTarget) -> DeviceResult<()>;
    /// 电源按键脉冲（long_press=长按强制断电；仅继电器类驱动支持）。
    async fn power_key(&self, target: &ControlTarget, long_press: bool) -> DeviceResult<()>;

    /// 继电器逐通道状态查询（只读，OP_STATUS 逐通道轮询）。返回应答 STATUS 的
    /// `(通道号, 是否吸合)` 列表；板卡不回答 STATUS 时为空表。仅继电器类驱动
    /// 实现；PDU 等其他后端默认 [`DeviceError::Unsupported`]。
    async fn channel_status(&self, _address: &str) -> DeviceResult<Vec<(u8, bool)>> {
        Err(DeviceError::Unsupported)
    }

    /// 继电器单通道 ON→STATUS→OFF 往返测试（写动作）。无论 ON 断言是否通过，
    /// 都必须继续执行 OFF 恢复释放态（恢复优先于断言）。
    async fn channel_test(&self, _address: &str, _channel: u8) -> DeviceResult<ChannelTestReport> {
        Err(DeviceError::Unsupported)
    }
}
