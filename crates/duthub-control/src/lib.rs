//! duthub-control：继电器板卡协议驱动。
//!
//! 当前唯一实现：CH340 类 4 字节协议继电器（[`ch340`] 协议 +
//! [`driver::Ch340RelayDriver`]）。
//! 单一实现时由桌面端启动时直接构造并注入共用设备操作入口
//! （`duthub-device::control::ControlService），不增加转发层或后端选择逻辑`。
//!
//! 模块：
//! - [`ch340`]：4 字节协议（帧构造 / 回显分类 / STATUS 扫描），纯函数可独立测试；
//! - [`driver`]：[`driver::RelayDriver`] trait + CH340 实现（单 task 独占串口句柄，
//!   错误统一 [`duthub_core::DeviceError`]）+ [`duthub_core::ControlBackend`] 适配
//!   （`power=通道通断，reset=脉冲，power_key=短/长脉冲`）。

#![forbid(unsafe_code)]

pub mod ch340;
pub mod driver;

pub use driver::{
    Ch340RelayDriver, ChannelInfo, Opener, PortProbe, PulseTiming, RelayDriver, RelayStream,
};

/// 真实硬件探测（默认 `#[ignore]`，显式运行：
/// `cargo test -p duthub-control --features real-hardware -- --ignored --nocapture`）。
/// 只 open/close 串口句柄，不发送任何协议帧。
#[cfg(all(test, feature = "real-hardware"))]
mod real_probe_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "真实硬件探测：显式运行（不发协议帧，只 open/close 串口）"]
    async fn probe_com25_open_close() {
        let ports = tokio_serial::available_ports().expect("枚举本机串口失败");
        let names: Vec<String> = ports.iter().map(|p| p.port_name.clone()).collect();
        println!("本机串口: {names:?}");
        assert!(names.iter().any(|n| n == "COM25"), "COM25 不在列，无法探测");

        let driver = Ch340RelayDriver::new();
        let probe = driver
            .probe_port("COM25", ch340::DEFAULT_BAUD)
            .expect("COM25 open 失败（被占用或驱动异常）");
        println!("COM25 open/close 探测通过: {probe:?}");
    }

    /// STATUS 通道枚举（已获阶段批准：只发 [0xA0, ch, 0x05, checksum] 只读查询帧，
    /// 禁止任何 ON/OFF 切换）。连续两次枚举核对响应稳定性。
    /// 真实 ON/OFF 动作用于用户指明「通道→电源/复位/按键」接线后再验证。
    #[tokio::test]
    #[ignore = "真实硬件枚举：显式运行（仅 STATUS 只读帧，不切换继电器）"]
    async fn open_com25_enumerate_channels() {
        let driver = Ch340RelayDriver::new();
        let first = RelayDriver::open(&driver, "COM25")
            .await
            .expect("COM25 枚举失败");
        println!("第一次枚举: {first:?}");
        let second = RelayDriver::open(&driver, "COM25")
            .await
            .expect("COM25 二次枚举失败");
        println!("第二次枚举: {second:?}");
        assert_eq!(first, second, "两次枚举结果不一致（响应不稳定）");
        println!(
            "响应稳定。通道→电源/复位/按键映射保持为 DUT 配置项；真实 ON/OFF 待接线确认后验证。"
        );
    }

    /// 真机继电器单通道往返（**已获用户明确授权**：通道 1 ON→STATUS→OFF 并恢复 OFF；
    /// 绝不同时动作两个通道）。直接驱动 COM25，不启动 GUI。流程：
    /// 1) `ControlService::channel_status` 只读 STATUS 枚举（channelsSeen / statesBefore）；
    /// 2) `channel_test(1)` 经 control.test 审批闸门执行
    ///    ON→STATUS 确认吸合→OFF→STATUS 确认恢复，全程记录进 [`ChannelTestReport`]。
    ///
    /// 任何一步异常即 fail（测试侧不静默重试，由调用方 escalate 决策）。
    /// `ControlService` 是同步门面（内部 `block_on`）：用普通 `#[test]`，驱动 task 都在其
    /// 内部 runtime 上派生（见 driver.rs 测试注释）。
    #[test]
    #[ignore = "真实硬件继电器往返：显式运行（单通道 1 ON→STATUS→OFF，已获用户授权）"]
    fn relay_live_roundtrip() {
        use duthub_core::model::{
            AdbConfig, DutA2aConfig, DutConfig, DutMediaConfig, FlashConfig, HeartbeatConfig,
            McpConfig, RelayChannels, RelayConfig, SerialConfig,
        };
        use duthub_core::{ApprovalManager, ControlBackend};
        use duthub_device::control::ControlService;
        use std::sync::Arc;

        let approval = Arc::new(ApprovalManager::new());
        let backend: Arc<dyn ControlBackend> = Arc::new(Ch340RelayDriver::new());
        let control = ControlService::new(Some(backend), approval.clone());
        control.register_dut(DutConfig {
            id: "dut-live-com25".into(),
            name: "COM25 继电器板".into(),
            serial: SerialConfig {
                host: "local".into(),
                port: "COM25".into(),
                baud: ch340::DEFAULT_BAUD,
                format: "8N1".into(),
                reference_log: None,
                reference_similarity: None,
                split_logs: false,
            },
            adb: AdbConfig::default(),
            relay: RelayConfig::UsbSerial {
                host: "local".into(),
                node: "COM25".into(),
                driver: "CH340".into(),
                channels: RelayChannels {
                    power: Some(1),
                    reset: None,
                    recovery: None,
                    maskrom: None,
                    power_key: None,
                },
            },
            mcp: McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8797/mcp".into(),
            },
            a2a: DutA2aConfig::default(),
            flash: FlashConfig::default(),
            media: DutMediaConfig::default(),
            heartbeat: HeartbeatConfig::default(),
        });

        // 0) 只读 STATUS 枚举（无需审批）
        let channels = control
            .channel_status("dut-live-com25")
            .expect("COM25 STATUS 枚举失败");
        println!(
            "channelsSeen: {:?}",
            channels.iter().map(|(c, _)| *c).collect::<Vec<_>>()
        );
        println!("statesBefore: {channels:?}");
        assert!(
            channels.iter().any(|&(c, _)| c == 1),
            "通道 1 未应答 STATUS，无法做往返"
        );

        // 1) 通道 1 往返（写动作，走 control.test 审批闸门）
        let token = approval.issue("dut-live-com25", "control.test", r#"{"channel":1}"#);
        let report = control
            .channel_test("dut-live-com25", 1, &token.token)
            .expect("channel_test 执行失败");
        println!("testChannel: 1");
        println!("report: {report:?}");
        assert_eq!(
            report.before,
            channels.iter().find(|&&(c, _)| c == 1).map(|&(_, on)| on),
            "往返前状态应与枚举一致"
        );
        assert!(report.on_ok, "ON 后 STATUS 未确认吸合");
        assert!(report.off_ok, "OFF 后 STATUS 未确认释放");
        assert_eq!(report.after, Some(false), "往返后通道 1 必须恢复 OFF");
        println!(
            "onVerified={} offVerified={} statesAfter=[(1, {:?})] —— 往返通过，已恢复 OFF",
            report.on_ok, report.off_ok, report.after
        );
    }
}
