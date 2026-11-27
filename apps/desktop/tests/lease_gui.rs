//! W3 acceptance (F10): desktop lease chain must enable GUI session lifetime.
//!
//! Two assertions:
//! 1. `src/main.rs` calls `.with_gui_session_lifetime()` on the shared `ControlLeaseManager`
//!    consistent with `DeviceService` — omitting it lets automatic GUI leases expire on TTL,
//!    breaking the GUI-present agent-rejection guard (`--open-port` direct sessions).
//! 2. With this mode enabled, GUI leases never expire by TTL, remaining time stays `None`,
//!    agent takeover stays Busy, GUI write admission passes, and release returns to Free.

use std::path::PathBuf;
use std::time::Duration;

use duthub_core::{
    ControlLeaseManager, LeaseAuthority, LeaseError, LeaseHolder, LeasePhase, WriteIdentity,
};

fn manifest_dir() -> PathBuf {
    // Read at runtime rather than env! (compile-time paths break if the directory is moved).
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
}

fn agent_holder() -> LeaseHolder {
    LeaseHolder::Agent {
        agent_name: "test-agent".into(),
        credential_id: "cred-test".into(),
    }
}

#[test]
fn main_wires_gui_session_lifetime_onto_the_shared_lease_manager() {
    let main = std::fs::read_to_string(manifest_dir().join("src").join("main.rs"))
        .expect("read apps/desktop/src/main.rs");
    let construction = main
        .find("ControlLeaseManager::new")
        .expect("main.rs must construct ControlLeaseManager");
    let gui_lifetime = main.find(".with_gui_session_lifetime()").expect(
        "F10: ControlLeaseManager chain in main.rs is missing .with_gui_session_lifetime()",
    );
    assert!(
        gui_lifetime > construction,
        "with_gui_session_lifetime must be chained onto ControlLeaseManager construction"
    );
    // main.rs injects the shared ControlLeaseManager (with GUI session lifetime)
    // directly into DeviceService via `with_lease_authority`, keeping one unified
    // lease authority across desktop and device layers.
    assert!(
        main.contains(".with_lease_authority(lease.clone())"),
        "main.rs must wire the shared ControlLeaseManager into DeviceService"
    );
}

#[test]
fn gui_session_lease_survives_ttl_and_keeps_blocking_agent_takeover() {
    let manager = ControlLeaseManager::new(None)
        .with_ttl(Duration::from_millis(40))
        .with_gui_session_lifetime();
    let grant = manager
        .acquire("dut-w3", &LeaseHolder::Gui, None)
        .expect("GUI acquire");
    // First safeguard: GUI grant expiration timestamp is u64::MAX.
    assert_eq!(grant.expires_at_unix_ms, u64::MAX);

    std::thread::sleep(Duration::from_millis(80));

    // Lazy cleanup in status must not evict GUI leases; remaining time does not expose TTL.
    let status = manager.status("dut-w3");
    assert_eq!(status.phase, LeasePhase::Held);
    assert_eq!(status.remaining_secs, None);

    // Agent takeover stays Busy (guard remains active past TTL).
    let error = manager
        .acquire("dut-w3", &agent_holder(), None)
        .expect_err("agent must get Busy while GUI holds the lease");
    assert!(matches!(error, LeaseError::Busy { .. }), "{error:?}");

    // GUI write admission still passes (admit path is unaffected by TTL).
    assert!(
        manager.admit_write("dut-w3", &WriteIdentity::gui()).is_ok(),
        "GUI write must be admitted"
    );

    // Release path unchanged: GUI release returns to Free.
    manager.release_for_holder("dut-w3", &LeaseHolder::Gui);
    assert_eq!(manager.status("dut-w3").phase, LeasePhase::Free);
}
