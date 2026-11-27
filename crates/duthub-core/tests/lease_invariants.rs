//! Control lease invariant tests (`docs/SECURITY.md`; SPEC §6.1):
//! INV-01 single lease, INV-04 generation fencing, TTL expiry, GUI priority (Agent denied while
//! GUI holds), INV-05 Protected, INV-06 cross-DUT parallelism, and INV-07 restart invalidation.

use std::sync::Arc;
use std::time::Duration;

use duthub_core::audit::AuditSink;
use duthub_core::lease::{
    ControlLeaseManager, LeaseAuthority, LeaseError, LeaseHolder, LeasePhase, WriteIdentity,
};

fn manager() -> Arc<ControlLeaseManager> {
    // In-memory audit ring so tests can inspect events without disk I/O.
    Arc::new(
        ControlLeaseManager::new(Some(AuditSink::memory()))
            .with_wait_poll(Duration::from_millis(10)),
    )
}

fn agent(name: &str) -> LeaseHolder {
    LeaseHolder::Agent {
        agent_name: name.into(),
        credential_id: format!("cred-{name}"),
    }
}

/// INV-01: only one concurrent acquire succeeds per DUT; all others receive `Busy` with the holder.
#[test]
fn inv01_concurrent_acquire_single_winner() {
    let m = manager();
    let barrier = Arc::new(std::sync::Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|i| {
            let m = m.clone();
            let barrier = barrier.clone();
            let holder = if i == 0 {
                LeaseHolder::Gui
            } else {
                agent(&format!("bot-{i}"))
            };
            std::thread::spawn(move || {
                barrier.wait();
                m.acquire("dut-a", &holder, None)
            })
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let winners: Vec<_> = outcomes.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(
        winners.len(),
        1,
        "INV-01: concurrent acquire must produce exactly one winner"
    );
    // All 5 losers must fail with `Busy` naming the active holder.
    for err in outcomes.into_iter().filter_map(std::result::Result::err) {
        match err {
            LeaseError::Busy { holder, .. } => assert_ne!(holder, ""),
            other => panic!("Concurrent acquisition must return Busy, got {other:?}"),
        }
    }
    assert_eq!(m.status("dut-a").phase, LeasePhase::Held);
}

/// INV-01 other direction: Agent acquire is rejected with `Busy` + holder label while GUI holds.
#[test]
fn inv01_agent_acquire_rejected_while_gui_holds() {
    let m = manager();
    let g = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    assert_eq!(m.status("dut-a").holder.as_deref(), Some("GUI（人类在场）"));
    let err = m.acquire("dut-a", &agent("claude"), None).unwrap_err();
    match err {
        LeaseError::Busy { holder, .. } => {
            assert!(
                holder.contains("GUI"),
                "Busy message must name the GUI holder: {holder}"
            );
        }
        other => panic!("Expected Busy, got {other:?}"),
    }
    // Agent can acquire once the GUI releases.
    m.release("dut-a", &g.lease_id).unwrap();
    m.acquire("dut-a", &agent("claude"), None).unwrap();
}

/// INV-04: release/re-acquire increments generation monotonically and rejects stale admissions.
#[test]
fn inv04_stale_generation_rejected() {
    let m = manager();
    let g1 = m.acquire("dut-a", &agent("claude"), None).unwrap();
    assert_eq!(g1.generation, 1);
    m.release("dut-a", &g1.lease_id).unwrap();
    let g2 = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    assert_eq!(g2.generation, 2, "each acquire must increment generation");
    // Stale handle admission: lease changed hands to GUI -> Busy naming GUI.
    let stale_write = WriteIdentity::agent(Some(g1.lease_id.clone()), Some(g1.generation));
    assert!(matches!(
        m.admit_write("dut-a", &stale_write),
        Err(LeaseError::Busy { .. })
    ));
    // Current handle with stale generation -> generation fencing error.
    let wrong_gen = WriteIdentity::agent(Some(g2.lease_id.clone()), Some(1));
    assert!(matches!(
        m.admit_write("dut-a", &wrong_gen),
        Err(LeaseError::Generation { current: 2 })
    ));
    // Matching `(lease_id, generation)` succeeds; renew must not advance generation.
    let ok = WriteIdentity::agent(Some(g2.lease_id.clone()), Some(2));
    assert!(m.admit_write("dut-a", &ok).is_ok());
    let renewed = m.renew("dut-a", &g2.lease_id).unwrap();
    assert_eq!(
        renewed.generation, 2,
        "renew must not advance generation (SPEC §6.5)"
    );
    // Force takeover advances generation again and blocks subsequent writes from prior holder.
    let g3 = m.force_takeover("dut-a").unwrap();
    assert_eq!(g3.generation, 3);
    assert!(matches!(
        m.admit_write("dut-a", &ok),
        Err(LeaseError::Busy { .. })
    ));
}

/// TTL expiry: lazy reclamation transitions to `Free`, renew returns `Expired`, and new acquires
/// succeed.
#[test]
fn ttl_expiry_invalidates_and_frees() {
    let m = Arc::new(
        ControlLeaseManager::new(Some(AuditSink::memory()))
            .with_ttl(Duration::from_millis(80))
            .with_wait_poll(Duration::from_millis(10)),
    );
    let g = m.acquire("dut-a", &agent("claude"), None).unwrap();
    std::thread::sleep(Duration::from_millis(110));
    // Renew runs before lazy reclamation -> returns `Expired` (distinct from `UnknownLease`).
    assert_eq!(m.renew("dut-a", &g.lease_id), Err(LeaseError::Expired));
    assert_eq!(m.status("dut-a").phase, LeasePhase::Free);
    // If a GUI lease expires, the next GUI write admit reacquires immediately with a new generation.
    let g2 = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    assert!(g2.generation > g.generation);
    std::thread::sleep(Duration::from_millis(110));
    // GUI write automatically acquires on admit.
    let ctx = m.admit_write("dut-a", &WriteIdentity::gui()).unwrap();
    let ctx = ctx.expect("GUI write must return a new lease context");
    assert!(ctx.generation > g2.generation);
}

/// INV-05: `Protected` rejects acquire, renew, release, writes, and takeover; exit follows the
/// state machine.
#[test]
fn inv05_protected_blocks_everything_until_exit() {
    let m = manager();
    let g = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    m.enter_protected("dut-a", "烧录中").unwrap();
    assert!(matches!(
        m.acquire("dut-a", &agent("bot"), None),
        Err(LeaseError::Protected { .. })
    ));
    assert!(matches!(
        m.renew("dut-a", &g.lease_id),
        Err(LeaseError::Protected { .. })
    ));
    assert!(matches!(
        m.release("dut-a", &g.lease_id),
        Err(LeaseError::Protected { .. })
    ));
    assert!(matches!(
        m.force_takeover("dut-a"),
        Err(LeaseError::Protected { .. })
    ));
    for w in [
        WriteIdentity::gui(),
        WriteIdentity::system(),
        WriteIdentity::agent(None, None),
    ] {
        assert!(matches!(
            m.admit_write("dut-a", &w),
            Err(LeaseError::Protected { .. })
        ));
    }
    assert_eq!(
        m.status("dut-a").phase,
        LeasePhase::Protected,
        "status must report Protected phase"
    );
    // `exit_protected(true)` -> `Free`; Agent can acquire with incremented generation.
    m.exit_protected("dut-a", true).unwrap();
    assert_eq!(m.status("dut-a").phase, LeasePhase::Free);
    let g2 = m.acquire("dut-a", &agent("bot"), None).unwrap();
    assert!(g2.generation > g.generation);
    // `exit_protected(false)` -> `NeedsInspection` -> manual resolution -> `Free`.
    m.enter_protected("dut-a", "烧录中").unwrap();
    m.exit_protected("dut-a", false).unwrap();
    assert_eq!(m.status("dut-a").phase, LeasePhase::NeedsInspection);
    assert!(matches!(
        m.acquire("dut-a", &LeaseHolder::Gui, None),
        Err(LeaseError::NeedsInspection)
    ));
    m.resolve_inspection("dut-a", true);
    assert_eq!(m.status("dut-a").phase, LeasePhase::Free);
}

/// INV-06: independent DUTs do not block one another (mutual exclusion is per DUT).
#[test]
fn inv06_parallel_duts_independent() {
    let m = manager();
    let ga = m.acquire("dut-a", &agent("bot-a"), None).unwrap();
    let gb = m.acquire("dut-b", &agent("bot-b"), None).unwrap();
    let gc = m.acquire("dut-c", &LeaseHolder::Gui, None).unwrap();
    assert_ne!(ga.lease_id, gb.lease_id);
    assert_ne!(gb.lease_id, gc.lease_id);
    // All three DUTs are held concurrently.
    for dut in ["dut-a", "dut-b", "dut-c"] {
        assert_eq!(m.status(dut).phase, LeasePhase::Held, "{dut}");
    }
    // Releasing one DUT does not affect the others.
    m.release("dut-a", &ga.lease_id).unwrap();
    assert_eq!(m.status("dut-a").phase, LeasePhase::Free);
    assert_eq!(m.status("dut-b").phase, LeasePhase::Held);
}

/// INV-07: process restart (fresh manager) invalidates all previous leases.
#[test]
fn inv07_restart_invalidates_all_leases() {
    let m1 = manager();
    let g = m1.acquire("dut-a", &agent("claude"), None).unwrap();
    let m2 = manager();
    assert_eq!(
        m2.renew("dut-a", &g.lease_id),
        Err(LeaseError::UnknownLease),
        "old lease handle must return UnknownLease after restart"
    );
    assert_eq!(
        m2.validate("dut-a", &g.lease_id, g.generation),
        Err(LeaseError::UnknownLease)
    );
    assert_eq!(m2.status("dut-a").phase, LeasePhase::Free);
    // Fresh manager can immediately grant a new lease.
    m2.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
}

/// FIFO wait queue: cancelling the head request lets the next waiter acquire the lease.
#[test]
fn fifo_queue_orders_cancels_and_times_out() {
    let m = manager();
    let g1 = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    // Enqueue two agents (`bot-a` then `bot-b`).
    let ha = {
        let m = m.clone();
        let holder = agent("bot-a");
        std::thread::spawn(move || m.acquire("dut-a", &holder, Some(Duration::from_millis(2000))))
    };
    std::thread::sleep(Duration::from_millis(50));
    let hb = {
        let m = m.clone();
        let holder = agent("bot-b");
        std::thread::spawn(move || m.acquire("dut-a", &holder, Some(Duration::from_millis(2000))))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        m.status("dut-a").queue_len,
        2,
        "two waiters should be queued"
    );
    // Cancel `bot-a`'s queued wait request.
    let req_a = m.pending_wait_requests("dut-a")[0];
    assert!(m.cancel_wait("dut-a", req_a));
    assert_eq!(m.status("dut-a").queue_len, 1);
    m.release("dut-a", &g1.lease_id).unwrap();
    // `bot-a` was cancelled, so `bot-b` is now at the head of the queue.
    let gb = hb.join().unwrap().unwrap();
    assert_eq!(gb.holder, "Agent bot-b");
    // Cancelled `bot-a` exits with `Busy`; wait timeout is covered by a dedicated unit test.
    assert!(matches!(ha.join().unwrap(), Err(LeaseError::Busy { .. })));
}

/// GUI priority: GUI writes auto-acquire when `Free`, and are denied with `Busy` naming the Agent
/// while an Agent holds the lease.
#[test]
fn gui_priority_write_admit_both_directions() {
    let m = manager();
    // Free: GUI write auto-acquires.
    let ctx = m
        .admit_write("dut-a", &WriteIdentity::gui())
        .unwrap()
        .expect("GUI admit must return a lease context");
    assert_eq!(m.status("dut-a").phase, LeasePhase::Held);
    // Repeated GUI write returns the same lease context (idempotent).
    let ctx2 = m
        .admit_write("dut-a", &WriteIdentity::gui())
        .unwrap()
        .expect("GUI admit must be idempotent");
    assert_eq!(ctx, ctx2);
    // Agent holds the lease: GUI write is denied.
    m.release("dut-a", &ctx.lease_id).unwrap();
    let ga = m.acquire("dut-a", &agent("bot"), None).unwrap();
    let err = m.admit_write("dut-a", &WriteIdentity::gui()).unwrap_err();
    match err {
        LeaseError::Busy { holder, .. } => assert!(holder.contains("bot"), "{holder}"),
        other => panic!("Expected Busy, got {other:?}"),
    }
    // System (heartbeat): allowed while GUI holds; rejected while Agent controls the DUT.
    m.release("dut-a", &ga.lease_id).unwrap();
    let gg = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    assert_eq!(
        m.admit_write("dut-a", &WriteIdentity::system()),
        Ok(None),
        "system writes are allowed while GUI holds the lease"
    );
    m.release("dut-a", &gg.lease_id).unwrap();
    let _ = m.acquire("dut-a", &agent("bot-2"), None).unwrap();
    assert!(
        matches!(
            m.admit_write("dut-a", &WriteIdentity::system()),
            Err(LeaseError::Busy { .. })
        ),
        "heartbeat must be rejected while an Agent holds the lease"
    );
}

/// Auth toggle coupling: when auth is ON, leaseless Agent writes fail with `LeaseRequired`; GUI is
/// unaffected.
#[test]
fn auth_switch_gates_leaseless_agent_writes() {
    let m = manager();
    // Default auth OFF: leaseless Agent writes pass via compatibility mode.
    assert_eq!(
        m.admit_write("dut-a", &WriteIdentity::agent(None, None)),
        Ok(None)
    );
    m.set_auth_enabled(true);
    assert!(matches!(
        m.admit_write("dut-a", &WriteIdentity::agent(None, None)),
        Err(LeaseError::LeaseRequired)
    ));
    // GUI is unaffected by the auth toggle (local human operator).
    assert!(m.admit_write("dut-a", &WriteIdentity::gui()).is_ok());
    // System heartbeat is also unaffected by auth (still gated by `Protected`; see INV-05).
    assert_eq!(m.admit_write("dut-a", &WriteIdentity::system()), Ok(None));
}
