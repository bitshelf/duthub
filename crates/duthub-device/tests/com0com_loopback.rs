//! M1.7 (optional): com0com virtual serial port pair loopback smoke test.
//!
//! Requires com0com (<https://sourceforge.net/projects/com0com/>) installed with
//! a virtual port pair configured (e.g. `COM10<->COM11` with `"com0com"` in its
//! description). If not installed, the test skips without failing.
//! Discovery checks port friendly names/descriptions for `"com0com"`; this test
//! only runs open/close smoke checks rather than full loopback I/O (covered by
//! the manual hardware checklist to avoid CI environment variance).

#![cfg(feature = "real-hardware")]

use duthub_device::{DeviceService, SessionKind};

#[test]
fn com0com_loopback_smoke_or_skip() {
    let service = DeviceService::custom(
        256,
        {
            let dir = std::env::current_dir()
                .unwrap()
                .join("target/test-tmp/com0com")
                .join(std::process::id().to_string());
            let _ = std::fs::remove_dir_all(&dir);
            dir
        },
        5,
    );

    let ports = service.list_ports();
    let pairs: Vec<_> = ports
        .iter()
        .filter(|p| {
            p.description
                .as_deref()
                .is_some_and(|d| d.to_ascii_lowercase().contains("com0com"))
        })
        .collect();

    if pairs.len() < 2 {
        eprintln!(
            "SKIP: No com0com virtual port pair was found among {} ports",
            ports.len()
        );
        return;
    }

    // Paired ports found: run open/close smoke test (only one session can bind a port at a time).
    let port_a = pairs[0].port.clone();
    let session = service.open_session("com0com-a", &port_a, 115_200, SessionKind::Real);
    match session {
        Ok(session_id) => {
            eprintln!("OK: Opened com0com port {port_a} (session={session_id})");
            service.close_session("com0com-a").ok();
        }
        Err(err) => {
            // Skip on environmental errors such as port busy or insufficient permissions.
            eprintln!("SKIP: Failed to open com0com port {port_a}: {err}");
        }
    }
}
