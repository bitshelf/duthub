//! Regression: a press-and-hold must survive an in-flight board probe.
//!
//! The relay offline monitor probes a board under the shared relay write gate
//! (`try_read` for one STATUS round-trip). A press that lands in that window
//! used to fail immediately with `DeviceError::Busy`, which the GUI reported as
//! a control failure (`AlertSource::Control`). The gate is still exclusive, but
//! acquisition now waits briefly for the probe; a wedged port still fails
//! honestly after the bound.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use duthub_core::backend::ChannelTestReport;
use duthub_core::model::{DutConfig, McpConfig, RelayChannels, RelayConfig, SerialConfig};
use duthub_core::{ApprovalManager, ControlBackend, ControlTarget, DeviceError, DeviceResult};
use duthub_device::control::ControlService;
use duthub_device::relay_watch::RelayProbeSample;

/// Backend whose probe blocks like a real STATUS exchange.
#[derive(Default)]
struct SlowProbeBackend {
    probe_started: AtomicBool,
    power_calls: Mutex<Vec<bool>>,
}

#[async_trait]
impl ControlBackend for SlowProbeBackend {
    async fn power(&self, _target: &ControlTarget, on: bool) -> DeviceResult<()> {
        self.power_calls.lock().unwrap().push(on);
        Ok(())
    }

    async fn reset(&self, _target: &ControlTarget) -> DeviceResult<()> {
        Ok(())
    }

    async fn power_key(&self, _target: &ControlTarget, _long_press: bool) -> DeviceResult<()> {
        Ok(())
    }

    async fn channel_status(&self, _address: &str) -> DeviceResult<Vec<(u8, bool)>> {
        Err(DeviceError::Unsupported)
    }

    async fn channel_test(&self, _address: &str, _channel: u8) -> DeviceResult<ChannelTestReport> {
        Err(DeviceError::Unsupported)
    }

    async fn probe_channel(&self, _address: &str, _channel: u8) -> DeviceResult<Option<bool>> {
        self.probe_started.store(true, Ordering::SeqCst);
        // A healthy STATUS exchange takes ≪300 ms; any real driver blocks here.
        std::thread::sleep(Duration::from_millis(300));
        Ok(Some(false))
    }
}

fn dut_with_serial_relay() -> DutConfig {
    DutConfig {
        id: "dut".into(),
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
        adb: duthub_core::model::AdbConfig::default(),
        relay: RelayConfig::UsbSerial {
            host: "local".into(),
            node: "COM25".into(),
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
        a2a: duthub_core::model::DutA2aConfig::default(),
        flash: duthub_core::model::FlashConfig::default(),
        media: duthub_core::model::DutMediaConfig::default(),
        heartbeat: duthub_core::model::HeartbeatConfig::default(),
    }
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn press_and_hold_waits_for_an_in_flight_board_probe() {
    let backend = Arc::new(SlowProbeBackend::default());
    let control = Arc::new(ControlService::new(
        Some(backend.clone()),
        Arc::new(ApprovalManager::new()),
    ));
    let dut = dut_with_serial_relay();

    // 1) A monitor probe takes the shared read gate.
    let probe_control = control.clone();
    let probe = std::thread::spawn(move || probe_control.probe_board_status("COM25", 1));
    wait_until(Duration::from_secs(2), || {
        backend.probe_started.load(Ordering::SeqCst)
    });

    // 2) The user presses while the probe is still inside the backend. The hold
    //    must wait for the gate instead of returning Busy.
    let (release, released) = tokio::sync::oneshot::channel();
    let hold_control = control.clone();
    let hold_dut = dut.clone();
    let hold = std::thread::spawn(move || hold_control.hold_channel(&hold_dut, 1, released));

    std::thread::sleep(Duration::from_millis(50));
    assert!(
        backend.power_calls.lock().unwrap().is_empty(),
        "the hold must be waiting for the probe gate, not have failed or switched ON early"
    );

    // 3) The probe finishes, the hold acquires the gate and switches ON.
    assert_eq!(probe.join().unwrap(), RelayProbeSample::Answered(false));
    wait_until(Duration::from_secs(2), || {
        !backend.power_calls.lock().unwrap().is_empty()
    });
    assert_eq!(*backend.power_calls.lock().unwrap(), vec![true]);

    // 4) Releasing completes the hold with the OFF write.
    release.send(()).expect("release receiver is alive");
    assert!(hold.join().unwrap().is_ok());
    assert_eq!(*backend.power_calls.lock().unwrap(), vec![true, false]);
}
