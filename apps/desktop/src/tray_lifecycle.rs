//! Window lifecycle coordinator: the single place where close, hide, restore,
//! quit, relay release and float/preview teardown are ordered.
//!
//! Pure: no GPUI types, no `windows` crate, no `AlertCenter`. The input is a
//! [`LifecycleEvent`]; the output is the next [`LifecycleState`] plus an
//! ordered [`LifecycleAction`] list. The desktop `Workbench` is only an
//! executor — it walks the list and performs the side effects
//! (`docs/design/TRAY_ALERT_DESIGN.md` §2.1/§2.2, §12).
//!
//! Ordering rules that must never move:
//!
//! - `ReleaseRelay`/`ReleaseRelayAndJoin` is always the first action: hiding
//!   or exiting must never leave a relay channel energised.
//! - `CloseFloat` runs before the main window disappears: a leftover float
//!   window keeps the process alive with no main window.
//! - `StopPreview` is emitted only on the quit paths (before `DestroyMain` /
//!   process exit). The hide path deliberately leaves capture/preview running
//!   (R5: the program keeps working while hidden).
//! - `QuitRequested` uses the bounded join (`ReleaseRelayAndJoin`) because the
//!   process is about to exit; the hide path only signals release.

use crate::tray::TrayBackend;
use crate::tray_ui::{
    AlertPresentation, AlertRoute, CloseStep, alert_route, close_disposition, close_plan,
};

/// Where the main window is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePhase {
    /// Visible, no icon, alerts handled by the in-app dialog.
    Visible,
    /// Hidden to the notification area; the 50 ms tick keeps running.
    Hidden,
    /// Shutdown started: no window may be resurrected.
    Quitting,
}

/// Everything the coordinator needs to know about the current run.
///
/// Derived by the executor from the Workbench fields (`tray.is_some()`,
/// `hidden`, `quitting`) so there is no second copy of the window state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleState {
    /// The tray backend is installed and usable.
    pub tray_ready: bool,
    pub phase: LifecyclePhase,
}

impl LifecycleState {
    /// A visible window with or without a usable tray.
    #[must_use]
    pub fn new(tray_ready: bool) -> Self {
        Self {
            tray_ready,
            phase: LifecyclePhase::Visible,
        }
    }

    /// Same tray readiness in an explicit phase.
    #[must_use]
    pub fn with_phase(self, phase: LifecyclePhase) -> Self {
        Self { phase, ..self }
    }

    /// Shutdown already latched: every further event is a no-op.
    #[must_use]
    pub fn is_quitting(&self) -> bool {
        self.phase == LifecyclePhase::Quitting
    }
}

/// Inputs from the OS close gate, the tray menu and the alert tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// The platform asked to close the main window (titlebar X, Alt+F4,
    /// system menu, taskbar close) — every entry collapses into this one.
    CloseRequested,
    /// The user asked for the main window back (icon click, balloon click,
    /// keyboard select).
    ShowRequested,
    /// The tray menu picked "quit".
    QuitRequested,
    /// `TaskbarCreated` replay failed: the icon is gone while the window is
    /// hidden.
    TaskbarLost,
    /// A queued alert is ready; `presentation` comes from
    /// [`crate::tray_ui::HiddenAlertRouter`].
    AlertArrived {
        presentation: AlertPresentation,
        count: u64,
    },
}

/// One ordered side effect. The executor performs them in list order and does
/// not reorder or skip any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    /// Signal the press-and-hold worker to release (no join); the OFF write
    /// completes on the worker thread while the UI keeps running.
    ReleaseRelay,
    /// Signal release and wait (bounded) for the OFF write: quit paths only.
    ReleaseRelayAndJoin,
    /// Destroy the HDMI float window (its observer runs deferred).
    CloseFloat,
    /// Release the inline monitor preview / frame lease (quit paths only).
    StopPreview,
    /// Destroy the main window so `quit_on_empty` can end the process.
    DestroyMain,
    /// Hide the main window and show the notification icon.
    HideToTray,
    /// Show (and activate) the main window again.
    ShowMain,
    /// Show the main window and report the lost icon in-app.
    RecoverFromIconLoss,
    /// Stop agent sessions, A2A listeners, MCP listeners and the probe thread.
    ShutdownRuntime,
    /// Remove the notification icon and stop the tray thread.
    RemoveTray,
    /// End the process loop (`cx.quit`).
    QuitProcess,
    /// Present a queued alert with the given exact presentation.
    PresentAlert {
        presentation: AlertPresentation,
        count: u64,
    },
    /// Start (or restart) the blinking episode.
    StartFlash,
    /// Stop the blinking episode and restore the normal icon.
    StopFlash,
}

/// Result of one evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleTransition {
    pub next: LifecycleState,
    pub actions: Vec<LifecycleAction>,
}

impl LifecycleTransition {
    fn stay(state: LifecycleState) -> Self {
        Self {
            next: state,
            actions: Vec::new(),
        }
    }

    fn to(state: LifecycleState, actions: Vec<LifecycleAction>) -> Self {
        Self {
            next: state,
            actions,
        }
    }
}

/// Evaluates one event. This is the whole policy of the window lifecycle.
#[must_use]
pub fn on_event(state: LifecycleState, event: LifecycleEvent) -> LifecycleTransition {
    // Once shutdown is latched nothing may bring a window back. The platform
    // close is allowed through by the gate itself (`prepare_main_window_close`
    // returns `true` while quitting), so no action is emitted here either.
    if state.is_quitting() {
        return LifecycleTransition::stay(state);
    }
    match event {
        LifecycleEvent::CloseRequested => close_transition(state),
        LifecycleEvent::ShowRequested => LifecycleTransition::to(
            state.with_phase(LifecyclePhase::Visible),
            vec![LifecycleAction::ShowMain],
        ),
        LifecycleEvent::QuitRequested => {
            LifecycleTransition::to(state.with_phase(LifecyclePhase::Quitting), quit_actions())
        }
        LifecycleEvent::TaskbarLost => LifecycleTransition::to(
            state.with_phase(LifecyclePhase::Visible),
            vec![LifecycleAction::RecoverFromIconLoss],
        ),
        LifecycleEvent::AlertArrived {
            presentation,
            count,
        } => LifecycleTransition::to(state, alert_actions(presentation, count)),
    }
}

/// Close plan for `state`: hide when a usable tray exists, otherwise quit for
/// real (never pretend to hide with no way back).
fn close_transition(state: LifecycleState) -> LifecycleTransition {
    let disposition = close_disposition(state.tray_ready, false);
    let actions = actions_for_close_plan(close_plan(disposition));
    let next = match disposition {
        crate::tray_ui::CloseDisposition::HideToTray => state.with_phase(LifecyclePhase::Hidden),
        crate::tray_ui::CloseDisposition::Quit => state.with_phase(LifecyclePhase::Quitting),
    };
    LifecycleTransition::to(next, actions)
}

/// Expands the frozen [`CloseStep`] plan into executable actions.
///
/// This is the only mapping; `workspace::run_close_plan` passes its plan
/// through here so the order cannot drift between the decision and the
/// executor. `DestroyMain` is the point of no return, so the inline preview is
/// released first.
#[must_use]
pub fn actions_for_close_plan(plan: &[CloseStep]) -> Vec<LifecycleAction> {
    let mut actions = Vec::with_capacity(plan.len() + 1);
    for step in plan {
        match *step {
            CloseStep::ReleaseRelay => actions.push(LifecycleAction::ReleaseRelay),
            CloseStep::CloseFloat => actions.push(LifecycleAction::CloseFloat),
            CloseStep::HideWindow => actions.push(LifecycleAction::HideToTray),
            CloseStep::DestroyMain => {
                actions.push(LifecycleAction::StopPreview);
                actions.push(LifecycleAction::DestroyMain);
            }
        }
    }
    actions
}

/// Full shutdown order for the tray menu.
fn quit_actions() -> Vec<LifecycleAction> {
    vec![
        LifecycleAction::ReleaseRelayAndJoin,
        LifecycleAction::CloseFloat,
        LifecycleAction::StopPreview,
        LifecycleAction::ShutdownRuntime,
        LifecycleAction::RemoveTray,
        LifecycleAction::QuitProcess,
    ]
}

/// Tray actions for one alert presentation.
///
/// The visible window is rendered by `render_root` (dialog), so `Dialog` needs
/// no action here. `QueueOnly` keeps the alert queued. `Both` is a balloon
/// plus a blink; `Tray` is either channel alone.
#[must_use]
pub fn alert_actions(presentation: AlertPresentation, count: u64) -> Vec<LifecycleAction> {
    match alert_route(presentation) {
        AlertRoute::Dialog | AlertRoute::Queue => Vec::new(),
        AlertRoute::Both => vec![
            LifecycleAction::PresentAlert {
                presentation,
                count,
            },
            LifecycleAction::StartFlash,
        ],
        AlertRoute::Tray => match presentation {
            AlertPresentation::FlashOnly => vec![LifecycleAction::StartFlash],
            _ => vec![LifecycleAction::PresentAlert {
                presentation,
                count,
            }],
        },
    }
}

/// Builds the balloon request for one presentation.
///
/// A critical (device-level) alert must bypass Windows quiet time: the shell
/// drops a balloon that carries `NIIF_RESPECT_QUIET_TIME` during quiet time
/// without queueing it, so the presentation policy's "quiet time only lets
/// critical alerts through" would be impossible otherwise. Agent notices (not
/// critical) keep the flag.
#[must_use]
pub fn balloon_notification(
    title: String,
    body: String,
    level: crate::tray::TrayNotifyLevel,
    critical: bool,
) -> crate::tray::TrayNotification {
    crate::tray::TrayNotification {
        title,
        body,
        level,
        respect_quiet_time: !critical,
    }
}

/// Outcome of applying the tray-facing actions of a plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrayRunOutcome {
    /// `hide_main` succeeded (the window is hidden and the icon exists).
    pub hidden: bool,
    /// `show_main` succeeded.
    pub shown: bool,
}

/// Applies the tray-facing subset of `actions` to `backend`.
///
/// This helper covers the stateless tray effects (`RemoveTray`, `StartFlash`,
/// `StopFlash`, and the simple hide/show outcomes). `PresentAlert` carries no
/// text: the workspace builds a [`crate::tray::TrayNotification`] from
/// `AlertCenter::latest()` and calls [`crate::tray::TrayBackend::notify_balloon`]
/// directly, which is still a port call — swapping the notification channel
/// only changes the `TrayBackend` implementation, not the workspace. Tests
/// replace the backend with a recording mock to prove a plan only touches the
/// tray for tray actions.
pub fn run_tray_actions(backend: &dyn TrayBackend, actions: &[LifecycleAction]) -> TrayRunOutcome {
    let mut outcome = TrayRunOutcome::default();
    for action in actions {
        match *action {
            LifecycleAction::HideToTray => outcome.hidden = backend.hide_main(),
            LifecycleAction::ShowMain => outcome.shown = backend.show_main(),
            LifecycleAction::StartFlash => backend.start_flash(),
            LifecycleAction::StopFlash => backend.stop_flash(),
            LifecycleAction::RemoveTray => backend.remove(),
            _ => {}
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critical_alerts_bypass_quiet_time_and_notices_respect_it() {
        let critical = balloon_notification(
            "title".into(),
            "body".into(),
            crate::tray::TrayNotifyLevel::Warning,
            true,
        );
        assert!(
            !critical.respect_quiet_time,
            "a device alert must survive Windows quiet time"
        );
        let notice = balloon_notification(
            "title".into(),
            "body".into(),
            crate::tray::TrayNotifyLevel::Info,
            false,
        );
        assert!(notice.respect_quiet_time);
    }

    fn hidden() -> LifecycleState {
        LifecycleState::new(true).with_phase(LifecyclePhase::Hidden)
    }

    #[test]
    fn close_hides_when_the_tray_is_ready() {
        let transition = on_event(LifecycleState::new(true), LifecycleEvent::CloseRequested);
        assert_eq!(transition.next.phase, LifecyclePhase::Hidden);
        assert_eq!(
            transition.actions,
            vec![
                LifecycleAction::ReleaseRelay,
                LifecycleAction::CloseFloat,
                LifecycleAction::HideToTray
            ]
        );
    }

    #[test]
    fn close_without_a_tray_quits_for_real() {
        let transition = on_event(LifecycleState::new(false), LifecycleEvent::CloseRequested);
        assert_eq!(transition.next.phase, LifecyclePhase::Quitting);
        assert_eq!(
            transition.actions,
            vec![
                LifecycleAction::ReleaseRelay,
                LifecycleAction::CloseFloat,
                LifecycleAction::StopPreview,
                LifecycleAction::DestroyMain
            ]
        );
    }

    #[test]
    fn every_close_path_releases_the_relay_first() {
        for tray_ready in [true, false] {
            let transition = on_event(
                LifecycleState::new(tray_ready),
                LifecycleEvent::CloseRequested,
            );
            assert_eq!(
                transition.actions.first(),
                Some(&LifecycleAction::ReleaseRelay)
            );
            assert_eq!(
                transition.actions.get(1),
                Some(&LifecycleAction::CloseFloat),
                "the float window goes before the main window"
            );
        }
    }

    #[test]
    fn quitting_swallows_every_further_event() {
        let quitting = LifecycleState::new(true).with_phase(LifecyclePhase::Quitting);
        for event in [
            LifecycleEvent::CloseRequested,
            LifecycleEvent::ShowRequested,
            LifecycleEvent::QuitRequested,
            LifecycleEvent::TaskbarLost,
            LifecycleEvent::AlertArrived {
                presentation: AlertPresentation::BalloonAndFlash,
                count: 3,
            },
        ] {
            let transition = on_event(quitting, event);
            assert_eq!(transition.next, quitting);
            assert!(transition.actions.is_empty(), "{event:?}");
        }
    }

    #[test]
    fn show_restores_the_main_window() {
        let transition = on_event(hidden(), LifecycleEvent::ShowRequested);
        assert_eq!(transition.next.phase, LifecyclePhase::Visible);
        assert_eq!(transition.actions, vec![LifecycleAction::ShowMain]);
    }

    #[test]
    fn quit_joins_the_relay_and_shuts_the_runtime_down_in_order() {
        let transition = on_event(hidden(), LifecycleEvent::QuitRequested);
        assert_eq!(transition.next.phase, LifecyclePhase::Quitting);
        assert_eq!(
            transition.actions,
            vec![
                LifecycleAction::ReleaseRelayAndJoin,
                LifecycleAction::CloseFloat,
                LifecycleAction::StopPreview,
                LifecycleAction::ShutdownRuntime,
                LifecycleAction::RemoveTray,
                LifecycleAction::QuitProcess
            ]
        );
    }

    #[test]
    fn a_lost_icon_always_restores_the_window() {
        // The process must never stay hidden with no icon (decision 6).
        let transition = on_event(hidden(), LifecycleEvent::TaskbarLost);
        assert_eq!(transition.next.phase, LifecyclePhase::Visible);
        assert_eq!(
            transition.actions,
            vec![LifecycleAction::RecoverFromIconLoss]
        );
    }

    #[test]
    fn alert_actions_follow_the_presentation() {
        assert_eq!(
            alert_actions(AlertPresentation::InAppDialog, 2),
            Vec::<LifecycleAction>::new()
        );
        assert_eq!(
            alert_actions(AlertPresentation::QueueOnly, 2),
            Vec::<LifecycleAction>::new()
        );
        assert_eq!(
            alert_actions(AlertPresentation::BalloonAndFlash, 2),
            vec![
                LifecycleAction::PresentAlert {
                    presentation: AlertPresentation::BalloonAndFlash,
                    count: 2
                },
                LifecycleAction::StartFlash
            ]
        );
        assert_eq!(
            alert_actions(AlertPresentation::BalloonOnly, 2),
            vec![LifecycleAction::PresentAlert {
                presentation: AlertPresentation::BalloonOnly,
                count: 2
            }]
        );
        assert_eq!(
            alert_actions(AlertPresentation::FlashOnly, 2),
            vec![LifecycleAction::StartFlash]
        );
    }

    #[test]
    fn alert_event_keeps_the_phase_and_emits_the_presentation_actions() {
        let transition = on_event(
            hidden(),
            LifecycleEvent::AlertArrived {
                presentation: AlertPresentation::BalloonAndFlash,
                count: 4,
            },
        );
        assert_eq!(transition.next, hidden());
        assert_eq!(transition.actions.len(), 2);
        assert!(matches!(
            transition.actions[0],
            LifecycleAction::PresentAlert { count: 4, .. }
        ));
    }

    #[test]
    fn close_plan_expansion_is_ordered_and_total() {
        let hide = actions_for_close_plan(crate::tray_ui::close_plan(
            crate::tray_ui::CloseDisposition::HideToTray,
        ));
        assert_eq!(
            hide,
            vec![
                LifecycleAction::ReleaseRelay,
                LifecycleAction::CloseFloat,
                LifecycleAction::HideToTray
            ]
        );
        let quit = actions_for_close_plan(crate::tray_ui::close_plan(
            crate::tray_ui::CloseDisposition::Quit,
        ));
        assert_eq!(
            quit,
            vec![
                LifecycleAction::ReleaseRelay,
                LifecycleAction::CloseFloat,
                LifecycleAction::StopPreview,
                LifecycleAction::DestroyMain
            ]
        );
        assert_eq!(
            actions_for_close_plan(&[
                CloseStep::ReleaseRelay,
                CloseStep::CloseFloat,
                CloseStep::HideWindow
            ]),
            hide
        );
    }

    #[test]
    fn only_tray_actions_reach_the_backend() {
        #[derive(Default)]
        struct RecordingTray {
            calls: std::sync::Mutex<Vec<&'static str>>,
        }

        impl TrayBackend for RecordingTray {
            fn hide_main(&self) -> bool {
                self.calls.lock().unwrap().push("hide_main");
                true
            }
            fn show_main(&self) -> bool {
                self.calls.lock().unwrap().push("show_main");
                true
            }
            fn set_visible(&self, _visible: bool) -> bool {
                true
            }
            fn set_tooltip(&self, _text: &str) {}
            fn notify_balloon(&self, _notification: &crate::tray::TrayNotification) {}
            fn start_flash(&self) {
                self.calls.lock().unwrap().push("start_flash");
            }
            fn stop_flash(&self) {
                self.calls.lock().unwrap().push("stop_flash");
            }
            fn drain_events(&self) -> Vec<crate::tray::TrayEvent> {
                Vec::new()
            }
            fn remove(&self) {
                self.calls.lock().unwrap().push("remove");
            }
        }

        let backend = RecordingTray::default();
        let outcome = run_tray_actions(
            &backend,
            &[
                LifecycleAction::ReleaseRelay,
                LifecycleAction::CloseFloat,
                LifecycleAction::HideToTray,
            ],
        );
        assert!(outcome.hidden);
        assert!(!outcome.shown);
        assert_eq!(*backend.calls.lock().unwrap(), vec!["hide_main"]);

        let backend = RecordingTray::default();
        run_tray_actions(
            &backend,
            &[
                LifecycleAction::StartFlash,
                LifecycleAction::StopFlash,
                LifecycleAction::RemoveTray,
                LifecycleAction::CloseFloat,
            ],
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["start_flash", "stop_flash", "remove"]
        );
    }
}
