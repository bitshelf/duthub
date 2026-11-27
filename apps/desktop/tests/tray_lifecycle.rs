//! Pure lifecycle/tray logic through the public library surface.
//!
//! These tests exercise the exported port (`tray::TrayBackend`,
//! `tray_lifecycle::on_event`, `tray_ui::FlashPlan`/`AlertRoute`) the way the
//! desktop executor does. The Win32 backend itself is covered by
//! `tests/tray_win32.rs`; nothing here needs a window, a shell or a tray.

use std::sync::Mutex;

use duthub_desktop::tray::{TrayBackend, TrayEvent, TrayNotification, TrayNotifyLevel};
use duthub_desktop::tray_lifecycle::{
    LifecycleAction, LifecycleEvent, LifecyclePhase, LifecycleState, TrayRunOutcome, alert_actions,
    balloon_notification, on_event, run_tray_actions,
};
use duthub_desktop::tray_ui::{
    AlertPresentation, AlertRoute, FLASH_INTERVAL_MS, FLASH_MAX_MS, FlashFrame, FlashPlan,
    alert_route, flash_stops_for_animation_change,
};

// ---------------------------------------------------------------------------
// FlashPlan
// ---------------------------------------------------------------------------

#[test]
fn flash_plan_starts_lit_alternates_and_stops_at_the_cap() {
    let mut plan = FlashPlan::default();
    assert!(!plan.is_active());
    assert_eq!(plan.frame(0), FlashFrame::Stop, "no episode, no frame");

    plan.start(1_000);
    assert!(plan.is_active());
    assert_eq!(plan.started(), Some(1_000));
    assert_eq!(plan.frame(1_000), FlashFrame::Lit);
    assert_eq!(plan.frame(1_000 + FLASH_INTERVAL_MS), FlashFrame::Dark);
    assert!(!plan.should_stop(1_000 + FLASH_MAX_MS - 1));
    assert_eq!(plan.frame(1_000 + FLASH_MAX_MS), FlashFrame::Stop);
    assert!(plan.should_stop(1_000 + FLASH_MAX_MS));

    plan.stop();
    assert!(!plan.is_active());
    assert!(!plan.should_stop(1_000 + FLASH_MAX_MS * 2));
}

#[test]
fn flash_plan_restart_moves_the_deadline() {
    let mut plan = FlashPlan::default();
    plan.start(0);
    plan.start(20_000);
    assert_eq!(plan.started(), Some(20_000));
    assert!(
        !plan.should_stop(20_000 + FLASH_MAX_MS - 1),
        "a restart begins a fresh episode"
    );
}

// ---------------------------------------------------------------------------
// AlertRoute
// ---------------------------------------------------------------------------

#[test]
fn every_presentation_has_one_route() {
    assert_eq!(
        alert_route(AlertPresentation::InAppDialog),
        AlertRoute::Dialog
    );
    assert_eq!(
        alert_route(AlertPresentation::BalloonAndFlash),
        AlertRoute::Both
    );
    assert_eq!(
        alert_route(AlertPresentation::BalloonOnly),
        AlertRoute::Tray
    );
    assert_eq!(alert_route(AlertPresentation::FlashOnly), AlertRoute::Tray);
    assert_eq!(alert_route(AlertPresentation::QueueOnly), AlertRoute::Queue);
}

#[test]
fn dialog_and_queue_never_emit_tray_actions() {
    assert_eq!(
        alert_actions(AlertPresentation::InAppDialog, 3),
        [] as [duthub_desktop::tray_lifecycle::LifecycleAction; 0]
    );
    assert_eq!(
        alert_actions(AlertPresentation::QueueOnly, 3),
        [] as [duthub_desktop::tray_lifecycle::LifecycleAction; 0]
    );
}

// ---------------------------------------------------------------------------
// Lifecycle events
// ---------------------------------------------------------------------------

fn hidden() -> LifecycleState {
    LifecycleState::new(true).with_phase(LifecyclePhase::Hidden)
}

#[test]
fn close_always_releases_the_relay_before_touching_windows() {
    for tray_ready in [true, false] {
        let transition = on_event(
            LifecycleState::new(tray_ready),
            LifecycleEvent::CloseRequested,
        );
        assert_eq!(transition.actions[0], LifecycleAction::ReleaseRelay);
        assert_eq!(transition.actions[1], LifecycleAction::CloseFloat);
    }
}

#[test]
fn quit_joins_the_relay_and_hides_nothing() {
    let transition = on_event(hidden(), LifecycleEvent::QuitRequested);
    assert_eq!(transition.next.phase, LifecyclePhase::Quitting);
    assert!(
        !transition.actions.contains(&LifecycleAction::HideToTray),
        "a quit must never hide the window again"
    );
    assert_eq!(
        transition.actions.last(),
        Some(&LifecycleAction::QuitProcess)
    );
}

#[test]
fn a_failed_icon_replay_always_shows_the_window() {
    let transition = on_event(hidden(), LifecycleEvent::TaskbarLost);
    assert_eq!(transition.next.phase, LifecyclePhase::Visible);
    assert_eq!(
        transition.actions,
        vec![LifecycleAction::RecoverFromIconLoss]
    );
}

// ---------------------------------------------------------------------------
// Backend adapter with a mock
// ---------------------------------------------------------------------------

/// Recording backend: proves the executor only reaches for the tray on tray
/// actions. The real backend is `tray::TrayHandle` (Win32) or the inert
/// non-Windows handle.
#[derive(Default)]
struct RecordingTray {
    calls: Mutex<Vec<&'static str>>,
}

impl RecordingTray {
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl TrayBackend for RecordingTray {
    fn hide_main(&self) -> bool {
        self.calls.lock().expect("calls lock").push("hide_main");
        true
    }

    fn show_main(&self) -> bool {
        self.calls.lock().expect("calls lock").push("show_main");
        true
    }

    fn set_visible(&self, visible: bool) -> bool {
        self.calls
            .lock()
            .expect("calls lock")
            .push(if visible { "icon_on" } else { "icon_off" });
        true
    }

    fn set_tooltip(&self, _text: &str) {
        self.calls.lock().expect("calls lock").push("tooltip");
    }

    fn notify_balloon(&self, _notification: &TrayNotification) {
        self.calls.lock().expect("calls lock").push("notify");
    }

    fn start_flash(&self) {
        self.calls.lock().expect("calls lock").push("start_flash");
    }

    fn stop_flash(&self) {
        self.calls.lock().expect("calls lock").push("stop_flash");
    }

    fn drain_events(&self) -> Vec<TrayEvent> {
        Vec::new()
    }

    fn remove(&self) {
        self.calls.lock().expect("calls lock").push("remove");
    }
}

#[test]
fn the_plan_only_touches_the_tray_for_tray_actions() {
    let backend = RecordingTray::default();
    let transition = on_event(LifecycleState::new(true), LifecycleEvent::CloseRequested);
    let outcome: TrayRunOutcome = run_tray_actions(&backend, &transition.actions);
    assert!(outcome.hidden);
    assert_eq!(
        backend.calls(),
        vec!["hide_main"],
        "release/float actions belong to the workbench, not the tray"
    );
}

#[test]
fn show_maps_to_the_backend_and_icon_loss_stays_a_workbench_effect() {
    let backend = RecordingTray::default();
    let show = on_event(hidden(), LifecycleEvent::ShowRequested);
    run_tray_actions(&backend, &show.actions);
    // `RecoverFromIconLoss` is a compound workbench effect (show the window
    // *and* surface the in-app error), so the stateless tray helper must not
    // partially apply it.
    let lost = on_event(hidden(), LifecycleEvent::TaskbarLost);
    run_tray_actions(&backend, &lost.actions);
    assert_eq!(backend.calls(), vec!["show_main"]);
}

#[test]
fn flash_actions_use_the_backend() {
    let backend = RecordingTray::default();
    let transition = on_event(
        hidden(),
        LifecycleEvent::AlertArrived {
            presentation: AlertPresentation::FlashOnly,
            count: 1,
        },
    );
    run_tray_actions(&backend, &transition.actions);
    assert_eq!(backend.calls(), vec!["start_flash"]);
    run_tray_actions(&backend, &[LifecycleAction::StopFlash]);
    assert_eq!(backend.calls(), vec!["start_flash", "stop_flash"]);
}

#[test]
fn critical_alerts_do_not_respect_quiet_time() {
    // Quiet time drops `NIIF_RESPECT_QUIET_TIME` balloons without queueing
    // them, so a critical device alert must omit the flag or the presentation
    // policy's "only critical through" is unreachable.
    let critical = balloon_notification(
        "title".into(),
        "body".into(),
        TrayNotifyLevel::Warning,
        true,
    );
    assert!(!critical.respect_quiet_time);
    let notice = balloon_notification("title".into(), "body".into(), TrayNotifyLevel::Info, false);
    assert!(notice.respect_quiet_time);
}

#[test]
fn turning_animations_off_stops_an_active_blink() {
    assert!(flash_stops_for_animation_change(true, false));
    assert!(!flash_stops_for_animation_change(true, true));
    assert!(!flash_stops_for_animation_change(false, false));
}
