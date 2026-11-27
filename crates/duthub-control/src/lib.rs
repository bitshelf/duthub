//! `duthub-control`: protocol driver for relay boards.
//!
//! Currently the only implementation is the CH340-based 4-byte protocol relay ([`ch340`]
//! protocol + [`driver::Ch340RelayDriver`]). The desktop app constructs and injects it directly
//! into `duthub_device::control::ControlService` without extra forwarding layers.
//!
//! Modules:
//! - [`ch340`]: 4-byte protocol (frame construction, echo classification, STATUS scanning); pure
//!   functions testable independently.
//! - [`driver`]: [`driver::RelayDriver`] trait + CH340 implementation (single task-exclusive
//!   serial port handle, errors unified via [`duthub_core::DeviceError`]) +
//!   [`duthub_core::ControlBackend`] adapter (`power`: channel on/off, `reset`: pulse,
//!   `power_key`: short/long pulse).

#![forbid(unsafe_code)]

pub mod ch340;
pub mod driver;

pub use driver::{
    Ch340RelayDriver, ChannelInfo, Opener, PortProbe, PulseTiming, RelayDriver, RelayStream,
};

/// Real hardware detection (default: `#[ignore]`). Explicit execution:
/// `cargo test -p duthub-control --features real-hardware -- --ignored --nocapture`.
/// Only opens and closes the serial port handle without sending any protocol frames.
#[cfg(all(test, feature = "real-hardware"))]
mod real_probe_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "real hardware probe: explicit run only (open/close serial port without frames)"]
    async fn probe_com25_open_close() {
        let ports =
            tokio_serial::available_ports().expect("Failed to enumerate local serial ports.");
        let names: Vec<String> = ports.iter().map(|p| p.port_name.clone()).collect();
        println!("Local serial ports: {names:?}");
        assert!(
            names.iter().any(|n| n == "COM25"),
            "COM25 is not listed; therefore, it cannot be detected."
        );

        let driver = Ch340RelayDriver::new();
        let probe = driver.probe_port("COM25", ch340::DEFAULT_BAUD).expect(
            "COM25 connection failed (occupied by another process or driver malfunctioning).",
        );
        println!("COM25 open and close probe passed: {probe:?}");
    }

    /// STATUS: Channel enumeration (Phase approval obtained: Only read-only queries of frames
    /// [0xA0, ch, 0x05, checksum] are allowed; any ON/OFF switching is prohibited). Continuous
    /// enumeration is used to verify the stability of response. The actual ON/OFF operation is
    /// performed after the user specifies the connection between "channel → power/switch/button".
    #[tokio::test]
    #[ignore = "real hardware enumeration: explicit run only (read-only STATUS frames, no switching)"]
    async fn open_com25_enumerate_channels() {
        let driver = Ch340RelayDriver::new();
        let first = RelayDriver::open(&driver, "COM25")
            .await
            .expect("Failure in COM25 enumeration.");
        println!("First enumeration: {first:?}");
        let second = RelayDriver::open(&driver, "COM25")
            .await
            .expect("Failed to perform secondary enumeration on COM25.");
        println!("Second enumeration: {second:?}");
        assert_eq!(
            first, second,
            "Inconsistent results from both enumerations (response is unstable)."
        );
        println!(
            "Responses were stable. Confirm the configured DUT channel mapping before testing ON or OFF."
        );
    }

    /// Energize the relay in a single channel (**authorized by users**: Channel 1 turns ON ->
    /// STATUS -> OFF, then returns to OFF; never simultaneously activate both channels). Drives
    /// `COM25` directly without launching the GUI:
    /// 1) `ControlService::channel_status` reads the STATUS enumeration (`channelsSeen` /
    ///    `statesBefore`);
    /// 2) `channel_test(1)` runs after `control.test` approval, confirming ON -> STATUS -> OFF ->
    ///    STATUS recovery. All steps are recorded in `ChannelTestReport`.
    ///
    /// Any step that encounters an exception fails immediately without silent retry.
    /// `ControlService` is a synchronous facade (internal `block_on`), so all tasks run inside
    /// its internal runtime (see test comments in `driver.rs`).
    #[test]
    #[ignore = "real hardware relay roundtrip: explicit run only (channel 1 ON->STATUS->OFF, requires user authorization)"]
    fn relay_live_roundtrip() {
        use duthub_core::HeartbeatConfig;
        use duthub_core::model::{
            AdbConfig, DutA2aConfig, DutConfig, DutMediaConfig, FlashConfig, McpConfig,
            RelayChannels, RelayConfig, SerialConfig,
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

        // 0) Read-only STATUS enumeration (no approval required)
        let channels = control
            .channel_status("dut-live-com25")
            .expect("Failed to enumerate COM25 status information.");
        println!(
            "channelsSeen: {:?}",
            channels.iter().map(|(c, _)| *c).collect::<Vec<_>>()
        );
        println!("statesBefore: {channels:?}");
        assert!(
            channels.iter().any(|&(c, _)| c == 1),
            "Channel 1 does not respond to status requests. It is not possible to perform roundtrip communications."
        );

        // 1) Channel 1 for round-trip operations (write actions; use control.test to approve gate
        // operations)
        let token = approval.issue("dut-live-com25", "control.test", r#"{"channel":1}"#);
        let report = control
            .channel_test("dut-live-com25", 1, &token.token)
            .expect("The channel_test command failed.");
        println!("testChannel: 1");
        println!("report: {report:?}");
        assert_eq!(
            report.before,
            channels.iter().find(|&&(c, _)| c == 1).map(|&(_, on)| on),
            "The state before the round trip should be consistent with that listed in the enumeration."
        );
        assert!(
            report.on_ok,
            "After ON, the status is not confirmed yet. Energize."
        );
        assert!(
            report.off_ok,
            "After setting OFF, the status of the resource has not been confirmed for release."
        );
        assert_eq!(
            report.after,
            Some(false),
            "The return passage 1 must be restored to the OFF state."
        );
        println!(
            "onVerified={} offVerified={} statesAfter=[(1, {:?})] - round trip passed and returned to OFF",
            report.on_ok, report.off_ok, report.after
        );
    }
}
