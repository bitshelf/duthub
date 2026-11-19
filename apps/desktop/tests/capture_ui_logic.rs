//! Lane B regressions for `duthub_desktop::capture_ui`
//! (`docs/design/CAPTURE_STREAM_BACKENDS.md` §6.2): every row of the monitor-window
//! transition table, the two-action rows, and the total-function extensions.

use duthub_desktop::capture_ui::{
    MonitorAction as A, MonitorEvent as E, MonitorPlacement as P, next,
};

#[test]
fn docked_open_float_moves_to_floating_and_opens_one_window() {
    assert_eq!(
        next(P::Docked, E::OpenFloat),
        (P::Floating, A::OpenFloatWindow)
    );
}

#[test]
fn floating_redock_closes_the_float_then_focuses_main() {
    let (state, action) = next(P::Floating, E::Redock);
    assert_eq!(state, P::Docked);
    assert_eq!(action, A::CloseFloatWindowAndFocusMain);
    assert_eq!(
        action.steps(),
        [A::CloseFloatWindow, A::FocusMainWindow].as_slice()
    );
}

#[test]
fn floating_user_closed_returns_to_docked_placeholder() {
    assert_eq!(
        next(P::Floating, E::UserClosedWindow),
        (P::Docked, A::ShowDockedPlaceholder)
    );
    // User-closing the float window must not steal focus.
    assert!(!A::ShowDockedPlaceholder.contains(A::FocusMainWindow));
}

#[test]
fn floating_side_panel_closed_or_switched_keeps_floating_without_stopping() {
    for event in [E::SidePanelClosed, E::PanelSwitched] {
        let (state, action) = next(P::Floating, event);
        assert_eq!(state, P::Floating, "{event:?}");
        assert_eq!(action, A::None, "{event:?}");
        // The float window keeps consuming frames: never stop the preview.
        assert!(!action.contains(A::StopPreview), "{event:?}");
        assert!(!action.contains(A::CloseFloatWindow), "{event:?}");
    }
}

#[test]
fn docked_side_panel_closed_or_switched_stops_the_preview() {
    for event in [E::SidePanelClosed, E::PanelSwitched] {
        assert_eq!(
            next(P::Docked, event),
            (P::Docked, A::StopPreview),
            "{event:?}"
        );
    }
}

#[test]
fn floating_main_window_closing_closes_the_float_first() {
    let (state, action) = next(P::Floating, E::MainWindowClosing);
    assert_eq!(state, P::Closing);
    assert_eq!(action, A::CloseFloatWindow);
    assert_eq!(action.steps().first(), Some(&A::CloseFloatWindow));
    // No StopPreview here: closing the float releases the monitor lease.
    assert!(!action.contains(A::StopPreview));
}

#[test]
fn preview_failed_anywhere_returns_docked_and_stops_the_preview() {
    for state in [P::Docked, P::Floating, P::Closing] {
        let (next_state, action) = next(state, E::PreviewFailed);
        assert_eq!(next_state, P::Docked, "{state:?}");
        assert_eq!(action, A::StopPreviewAndCloseFloatWindow, "{state:?}");
        assert_eq!(
            action.steps(),
            [A::StopPreview, A::CloseFloatWindow].as_slice(),
            "{state:?}"
        );
    }
}

#[test]
fn closing_main_window_closed_is_a_noop() {
    assert_eq!(next(P::Closing, E::MainWindowClosed), (P::Closing, A::None));
}

#[test]
fn closing_swallows_irrelevant_events() {
    for event in [
        E::OpenFloat,
        E::Redock,
        E::UserClosedWindow,
        E::SidePanelClosed,
        E::PanelSwitched,
        E::MainWindowClosing,
    ] {
        assert_eq!(next(P::Closing, event), (P::Closing, A::None), "{event:?}");
    }
}

#[test]
fn docked_main_window_events_transition_to_closing() {
    for event in [E::MainWindowClosing, E::MainWindowClosed] {
        assert_eq!(next(P::Docked, event), (P::Closing, A::None), "{event:?}");
    }
}

#[test]
fn floating_main_window_closed_still_closes_the_float() {
    assert_eq!(
        next(P::Floating, E::MainWindowClosed),
        (P::Closing, A::CloseFloatWindow)
    );
}

#[test]
fn redundant_events_are_noops() {
    for (state, event) in [
        (P::Docked, E::Redock),
        (P::Docked, E::UserClosedWindow),
        (P::Floating, E::OpenFloat),
    ] {
        assert_eq!(next(state, event), (state, A::None), "{state:?} {event:?}");
    }
}

#[test]
fn action_steps_cover_every_variant() {
    assert!(A::None.steps().is_empty());
    assert_eq!(A::OpenFloatWindow.steps(), [A::OpenFloatWindow].as_slice());
    assert_eq!(
        A::CloseFloatWindow.steps(),
        [A::CloseFloatWindow].as_slice()
    );
    assert_eq!(A::FocusMainWindow.steps(), [A::FocusMainWindow].as_slice());
    assert_eq!(A::StopPreview.steps(), [A::StopPreview].as_slice());
    assert_eq!(
        A::ShowDockedPlaceholder.steps(),
        [A::ShowDockedPlaceholder].as_slice()
    );
    assert_eq!(
        A::CloseFloatWindowAndFocusMain.steps(),
        [A::CloseFloatWindow, A::FocusMainWindow].as_slice()
    );
    assert_eq!(
        A::StopPreviewAndCloseFloatWindow.steps(),
        [A::StopPreview, A::CloseFloatWindow].as_slice()
    );
    assert!(A::CloseFloatWindowAndFocusMain.contains(A::FocusMainWindow));
    assert!(!A::CloseFloatWindowAndFocusMain.contains(A::StopPreview));
}

#[test]
fn every_state_event_pair_is_handled_and_invariants_hold() {
    let states = [P::Docked, P::Floating, P::Closing];
    let events = [
        E::OpenFloat,
        E::Redock,
        E::UserClosedWindow,
        E::SidePanelClosed,
        E::PanelSwitched,
        E::PreviewFailed,
        E::MainWindowClosing,
        E::MainWindowClosed,
    ];
    for state in states {
        for event in events {
            let (next_state, action) = next(state, event);
            // StopPreview only ever accompanies a return to the docked view.
            if action.contains(A::StopPreview) {
                assert_eq!(next_state, P::Docked, "{state:?} {event:?}");
            }
            // A float window opens only when entering Floating.
            if action.contains(A::OpenFloatWindow) {
                assert_eq!(next_state, P::Floating, "{state:?} {event:?}");
            }
            // Closing a float window never comes with a focus grab on redock
            // unless it is the explicit Redock row.
            if action == A::CloseFloatWindowAndFocusMain {
                assert_eq!((state, event), (P::Floating, E::Redock));
            }
        }
    }
}
