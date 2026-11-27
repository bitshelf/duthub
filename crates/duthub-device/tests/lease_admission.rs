//! Integration tests for write-channel lease admission (`docs/SECURITY.md`):
//! GUI auto-holds a lease on open / releases on close / blocks agent writes
//! while held (INV-01) / allows System (heartbeat) writes / rejects stale
//! generations at admission (INV-04 `[D]`) / audits `write.deny` / enforces the
//! same rules on `ControlService`.
//!
//! Actor-side `[E]` re-check (demo session) is covered by
//! `src/demo.rs::actor_recheck_drops_stale_generation_write`.

use std::sync::Arc;
use std::time::Duration;

use duthub_core::ApprovalManager;
use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::lease::{LeaseHolder, LeasePhase, WriteIdentity};
use duthub_core::model::{DutConfig, McpConfig, RelayChannels, RelayConfig, SerialConfig};
use duthub_device::control::{ControlAction, ControlService};
use duthub_device::{DeviceService, SessionKind};

fn service_with_lease(
    tag: &str,
) -> (
    DeviceService,
    Arc<duthub_core::lease::ControlLeaseManager>,
    Arc<AuditSink>,
) {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target/test-tmp/lease-admission")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sink = AuditSink::memory();
    let mgr = Arc::new(duthub_core::lease::ControlLeaseManager::new(Some(
        sink.clone(),
    )));
    let service = DeviceService::custom(256, dir, 0)
        .with_lease_authority(mgr.clone())
        .with_audit_sink(sink.clone());
    (service, mgr, sink)
}

fn agent(name: &str) -> LeaseHolder {
    LeaseHolder::Agent {
        agent_name: name.into(),
        credential_id: format!("cred-{name}"),
    }
}

/// GUI interactive path: opening a session auto-holds the lease and closing releases it;
/// while held, agent writes are rejected and System (heartbeat) writes pass (INV-01).
#[test]
fn gui_session_auto_holds_lease_and_blocks_agent_writes() {
    let (service, mgr, sink) = service_with_lease("auto-hold");
    service
        .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    // GUI holds the lease immediately on open.
    let status = mgr.status("dut-a");
    assert_eq!(status.phase, LeasePhase::Held);
    assert_eq!(status.holder.as_deref(), Some("GUI（人类在场）"));
    // Agent write without a lease (transitional auth-off mode) must be rejected while GUI holds control (INV-01).
    let err = service
        .write_line("dut-a", WriteIdentity::agent(None, None), "agent-cmd")
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("GUI"),
        "denial message must indicate GUI presence: {msg}"
    );
    // Agent write with an invalid lease handle is also rejected.
    assert!(
        service
            .write_line(
                "dut-a",
                WriteIdentity::agent(Some("lease-nope".into()), Some(9)),
                "agent-cmd2"
            )
            .is_err()
    );
    // GUI writes pass with automatic lease context.
    service
        .write_line("dut-a", WriteIdentity::gui(), "gui-cmd")
        .unwrap();
    // System (heartbeat) writes pass.
    service
        .write_line("dut-a", WriteIdentity::system(), "uname -a")
        .unwrap();
    // Agent acquire is rejected with Busy while GUI holds the lease.
    assert!(matches!(
        mgr.acquire("dut-a", &agent("claude"), None),
        Err(duthub_core::lease::LeaseError::Busy { .. })
    ));
    // Denial is recorded as `write.deny` in the audit sink.
    assert!(sink.recent(200).iter().any(|e| e.action == "write.deny"
        && e.dut.as_deref() == Some("dut-a")
        && e.result == AuditOutcome::Denied));
    // Closing the session automatically releases the lease.
    service.close_session("dut-a").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(mgr.status("dut-a").phase, LeasePhase::Free);
    // Once released, the agent can acquire control immediately.
    mgr.acquire("dut-a", &agent("claude"), None).unwrap();
}

/// Symmetry check: once the agent acquires the lease after GUI closes, GUI writes are blocked until takeover.
#[test]
fn agent_controlled_gui_write_denied_until_takeover() {
    let (service, mgr, _sink) = service_with_lease("agent-holds");
    // Agent acquires control while no GUI session is open.
    let grant = mgr.acquire("dut-a", &agent("claude"), None).unwrap();
    // Opening a GUI session still succeeds (read-only is unrestricted) without acquiring the lease.
    service
        .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(mgr.status("dut-a").holder.as_deref(), Some("Agent claude"));
    // GUI write is rejected, naming the holding agent.
    let err = service
        .write_line("dut-a", WriteIdentity::gui(), "gui-cmd")
        .unwrap_err();
    assert!(err.to_string().contains("claude"), "{err}");
    // Agent write with matching `(lease_id, generation)` passes.
    service
        .write_line(
            "dut-a",
            WriteIdentity::agent(Some(grant.lease_id.clone()), Some(grant.generation)),
            "agent-cmd",
        )
        .unwrap();
    // GUI force takeover increments generation; stale agent writes are rejected at admission (INV-04 [D]).
    let takeover = mgr.force_takeover("dut-a").unwrap();
    assert!(takeover.generation > grant.generation);
    let stale = service.write_line(
        "dut-a",
        WriteIdentity::agent(Some(grant.lease_id.clone()), Some(grant.generation)),
        "stale-agent-cmd",
    );
    assert!(
        stale.is_err(),
        "stale generation write must be rejected at admission"
    );
    // After takeover, GUI writes pass.
    service
        .write_line("dut-a", WriteIdentity::gui(), "gui-cmd-2")
        .unwrap();
    service.close_session("dut-a").unwrap();
}

/// `ControlService` enforces the same lease rules: agent control actions are rejected
/// while a GUI session is present, GUI actions pass, and failed admission does not
/// consume the approval token (M10).
#[test]
fn control_entry_lease_admission_gates_agent_actions() {
    // Recording mock backend to verify the driver is only called after admission succeeds.
    #[derive(Default)]
    struct MockBackend {
        power_calls: std::sync::Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl duthub_core::ControlBackend for MockBackend {
        async fn power(
            &self,
            _target: &duthub_core::ControlTarget,
            _on: bool,
        ) -> duthub_core::DeviceResult<()> {
            *self.power_calls.lock().unwrap() += 1;
            Ok(())
        }
        async fn reset(
            &self,
            _target: &duthub_core::ControlTarget,
        ) -> duthub_core::DeviceResult<()> {
            Err(duthub_core::DeviceError::Unsupported)
        }
        async fn power_key(
            &self,
            _target: &duthub_core::ControlTarget,
            _long_press: bool,
        ) -> duthub_core::DeviceResult<()> {
            Err(duthub_core::DeviceError::Unsupported)
        }
    }
    let (service, mgr, _sink) = service_with_lease("control-entry");
    service
        .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    let backend = Arc::new(MockBackend::default());
    let approval = Arc::new(ApprovalManager::new());
    let control = ControlService::new(Some(backend.clone()), approval.clone())
        .with_lease_authority(mgr.clone());
    control.register_dut(DutConfig {
        id: "dut-a".into(),
        name: "Prototype".into(),
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
            host: String::new(),
            node: "COM25".into(),
            driver: String::new(),
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
        a2a: duthub_core::model::DutA2aConfig::default(),
        flash: duthub_core::model::FlashConfig::default(),
        media: duthub_core::model::DutMediaConfig::default(),
        heartbeat: duthub_core::heartbeat::HeartbeatConfig::default(),
    });
    // GUI session active: agent control action is denied by lease (INV-01 on control channel).
    let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
    let err = control
        .execute_by_id_as(
            "dut-a",
            ControlAction::Power { on: true },
            &token.token,
            &WriteIdentity::agent(None, None),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("GUI"),
        "control denial must indicate GUI presence: {err}"
    );
    assert_eq!(
        *backend.power_calls.lock().unwrap(),
        0,
        "driver must not be invoked"
    );
    // The same token remains valid because failed admission does not consume it (M10).
    // GUI path (human present) passes and invokes the driver.
    control
        .execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
        .unwrap();
    assert_eq!(*backend.power_calls.lock().unwrap(), 1);
    service.close_session("dut-a").unwrap();
}

/// Legacy `DeviceService` without lease authority wired preserves existing write behavior.
#[test]
fn legacy_service_without_lease_behaves_unchanged() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target/test-tmp/lease-admission")
        .join(format!("legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let service = DeviceService::custom(64, dir, 0);
    service
        .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    // Without lease checks, agent writes without a lease pass directly (legacy path).
    service
        .write_line("dut-a", WriteIdentity::agent(None, None), "uname -a")
        .unwrap();
    service
        .write_bytes("dut-a", WriteIdentity::gui(), b"key")
        .unwrap();
    service.close_session("dut-a").unwrap();
}
