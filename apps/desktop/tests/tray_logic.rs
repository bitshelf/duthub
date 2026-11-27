//! Pure tray decisions (`docs/design/TRAY_ALERT_DESIGN.md` §10.1): close
//! disposition/plan, the balloon/blink matrix, flash timing, the hidden-alert
//! router and the path-derived notification-icon identity.
//!
//! Everything here goes through the lib target and needs no GPUI window and no
//! desktop shell.

use duthub_desktop::tray_ui::{
    AlertPresentation, CloseDisposition, CloseStep, FLASH_INTERVAL_MS, FLASH_MAX_MS, FlashFrame,
    FlashStop, HiddenAlertAction, HiddenAlertEnv, HiddenAlertRouter, NotificationGate,
    alert_presentation, callback_anchor, callback_carries_anchor, callback_point,
    close_disposition, close_plan, flash_frame, flash_should_stop, identity_guid_bytes,
};

// ---------------------------------------------------------------------------
// Close disposition and plan
// ---------------------------------------------------------------------------

#[test]
fn close_disposition_only_hides_when_the_tray_is_ready() {
    assert_eq!(close_disposition(true, false), CloseDisposition::HideToTray);
    assert_eq!(close_disposition(false, false), CloseDisposition::Quit);
}

#[test]
fn quitting_wins_over_a_ready_tray() {
    // After the tray menu picked "quit" the window must be destroyed, not
    // hidden again, or the process could never exit.
    assert_eq!(close_disposition(true, true), CloseDisposition::Quit);
    assert_eq!(close_disposition(false, true), CloseDisposition::Quit);
}

#[test]
fn close_plan_releases_the_relay_first_in_both_paths() {
    for disposition in [CloseDisposition::HideToTray, CloseDisposition::Quit] {
        let plan = close_plan(disposition);
        assert_eq!(
            plan.first(),
            Some(&CloseStep::ReleaseRelay),
            "hiding or exiting must never leave a relay channel energised"
        );
        assert_eq!(
            plan.get(1),
            Some(&CloseStep::CloseFloat),
            "the float window must close before the main window disappears"
        );
    }
}

#[test]
fn close_plan_order_is_locked() {
    assert_eq!(
        close_plan(CloseDisposition::HideToTray),
        &[
            CloseStep::ReleaseRelay,
            CloseStep::CloseFloat,
            CloseStep::HideWindow
        ]
    );
    assert_eq!(
        close_plan(CloseDisposition::Quit),
        &[
            CloseStep::ReleaseRelay,
            CloseStep::CloseFloat,
            CloseStep::DestroyMain
        ]
    );
}

// ---------------------------------------------------------------------------
// Presentation matrix
// ---------------------------------------------------------------------------

#[test]
fn a_visible_window_always_uses_the_in_app_dialog() {
    for animations in [true, false] {
        for gate in [
            NotificationGate::Allowed,
            NotificationGate::QuietTime,
            NotificationGate::Blocked,
        ] {
            for critical in [true, false] {
                assert_eq!(
                    alert_presentation(true, animations, gate, critical),
                    AlertPresentation::InAppDialog
                );
            }
        }
    }
}

#[test]
fn hidden_window_uses_balloon_and_flash_when_the_os_allows_both() {
    assert_eq!(
        alert_presentation(false, true, NotificationGate::Allowed, true),
        AlertPresentation::BalloonAndFlash
    );
    assert_eq!(
        alert_presentation(false, true, NotificationGate::Allowed, false),
        AlertPresentation::BalloonAndFlash
    );
}

#[test]
fn disabling_animations_never_suppresses_the_balloon() {
    // The balloon is the only channel the shell guarantees for a hidden icon,
    // so the animation preference may only remove the blink.
    assert_eq!(
        alert_presentation(false, false, NotificationGate::Allowed, false),
        AlertPresentation::BalloonOnly
    );
    assert_eq!(
        alert_presentation(false, false, NotificationGate::QuietTime, true),
        AlertPresentation::BalloonOnly
    );
}

#[test]
fn quiet_time_only_lets_critical_alerts_through() {
    assert_eq!(
        alert_presentation(false, true, NotificationGate::QuietTime, true),
        AlertPresentation::BalloonAndFlash
    );
    assert_eq!(
        alert_presentation(false, false, NotificationGate::QuietTime, true),
        AlertPresentation::BalloonOnly
    );
    assert_eq!(
        alert_presentation(false, true, NotificationGate::QuietTime, false),
        AlertPresentation::FlashOnly
    );
    assert_eq!(
        alert_presentation(false, false, NotificationGate::QuietTime, false),
        AlertPresentation::QueueOnly
    );
}

#[test]
fn a_blocked_gate_never_balloons() {
    for animations in [true, false] {
        for critical in [true, false] {
            let presentation =
                alert_presentation(false, animations, NotificationGate::Blocked, critical);
            assert!(
                !matches!(
                    presentation,
                    AlertPresentation::BalloonAndFlash | AlertPresentation::BalloonOnly
                ),
                "blocked gate must not balloon (critical={critical}, animations={animations})"
            );
            let expected = if animations {
                AlertPresentation::FlashOnly
            } else {
                AlertPresentation::QueueOnly
            };
            assert_eq!(presentation, expected);
        }
    }
}

// ---------------------------------------------------------------------------
// Flash timing
// ---------------------------------------------------------------------------

#[test]
fn flash_frame_starts_lit_and_alternates_every_half_period() {
    assert_eq!(flash_frame(0, 0), FlashFrame::Lit);
    assert_eq!(flash_frame(0, FLASH_INTERVAL_MS - 1), FlashFrame::Lit);
    assert_eq!(flash_frame(0, FLASH_INTERVAL_MS), FlashFrame::Dark);
    assert_eq!(flash_frame(0, FLASH_INTERVAL_MS * 2 - 1), FlashFrame::Dark);
    assert_eq!(flash_frame(0, FLASH_INTERVAL_MS * 2), FlashFrame::Lit);
    assert_eq!(
        flash_frame(10_000, 10_000 + FLASH_INTERVAL_MS * 3),
        FlashFrame::Dark
    );
}

#[test]
fn flash_frame_stops_at_the_cap() {
    // Half-period 59 (29.999 s) is still blinking: dark frame, not stopped.
    assert_eq!(flash_frame(0, FLASH_MAX_MS - 1), FlashFrame::Dark);
    assert_eq!(flash_frame(0, FLASH_MAX_MS), FlashFrame::Stop);
    assert_eq!(flash_frame(0, FLASH_MAX_MS * 2), FlashFrame::Stop);
    // A clock that went backwards must not panic or resume blinking.
    assert_eq!(flash_frame(5_000, 1_000), FlashFrame::Lit);
}

#[test]
fn flash_stops_for_every_explicit_reason() {
    for reason in [
        FlashStop::Restored,
        FlashStop::BalloonClicked,
        FlashStop::Timeout,
        FlashStop::Acknowledged,
        FlashStop::AnimationsDisabled,
    ] {
        assert!(
            flash_should_stop(0, 0, Some(reason)),
            "{reason:?} must stop the blink immediately"
        );
    }
}

#[test]
fn flash_stops_once_the_cap_is_reached_without_a_reason() {
    assert!(!flash_should_stop(0, FLASH_MAX_MS - 1, None));
    assert!(flash_should_stop(0, FLASH_MAX_MS, None));
    assert!(flash_should_stop(0, FLASH_MAX_MS + 1, None));
}

#[test]
// The limits are compile-time constants; clippy's constant-assertion lint
// fires here but the whole point is a static policy check.
#[expect(
    clippy::assertions_on_constants,
    reason = "static policy check verifying compile-time flash limit constants"
)]
fn flash_policy_stays_within_the_agreed_limits() {
    // The design caps blinking at 30 s and alternates on whole half-periods.
    assert!(FLASH_MAX_MS <= 30_000);
    assert!(FLASH_INTERVAL_MS > 0);
    assert_eq!(FLASH_MAX_MS % FLASH_INTERVAL_MS, 0);
}

// ---------------------------------------------------------------------------
// HiddenAlertRouter
// ---------------------------------------------------------------------------

fn hidden_env(pending: u64) -> HiddenAlertEnv {
    HiddenAlertEnv {
        visible: false,
        dialog_open: false,
        pending,
        animations: true,
        gate: NotificationGate::Allowed,
        critical: true,
    }
}

#[test]
fn router_ignores_ticks_without_a_new_generation() {
    let mut router = HiddenAlertRouter::new();
    // Generation 0 is the initial cursor: nothing to present yet.
    assert_eq!(router.step(0, hidden_env(3), 0), HiddenAlertAction::None);
    assert_eq!(router.step(0, hidden_env(3), 50), HiddenAlertAction::None);
}

#[test]
fn router_presents_hidden_alerts_with_balloon_and_flash() {
    let mut router = HiddenAlertRouter::new();
    assert_eq!(
        router.step(1, hidden_env(2), 100),
        HiddenAlertAction::Present {
            presentation: AlertPresentation::BalloonAndFlash,
            count: 2,
        }
    );
    // The same generation is not presented twice.
    assert_eq!(router.step(1, hidden_env(2), 150), HiddenAlertAction::None);
}

#[test]
fn router_presents_with_balloon_only_when_animations_are_off() {
    let mut router = HiddenAlertRouter::new();
    let env = HiddenAlertEnv {
        animations: false,
        ..hidden_env(1)
    };
    assert_eq!(
        router.step(1, env, 0),
        HiddenAlertAction::Present {
            presentation: AlertPresentation::BalloonOnly,
            count: 1,
        }
    );
}

#[test]
fn router_presents_in_the_app_when_the_window_is_visible() {
    let mut router = HiddenAlertRouter::new();
    let env = HiddenAlertEnv {
        visible: true,
        ..hidden_env(4)
    };
    assert_eq!(
        router.step(1, env, 0),
        HiddenAlertAction::Present {
            presentation: AlertPresentation::InAppDialog,
            count: 4,
        }
    );
}

#[test]
fn router_holds_the_generation_while_a_dialog_is_open() {
    let mut router = HiddenAlertRouter::new();
    let blocked = HiddenAlertEnv {
        visible: true,
        dialog_open: true,
        ..hidden_env(5)
    };
    // Blocked by the dialog: nothing presented and the generation stays
    // unconsumed, so the alert is not silently swallowed behind the dialog.
    assert_eq!(router.step(7, blocked, 0), HiddenAlertAction::None);
    assert_eq!(router.step(7, blocked, 50), HiddenAlertAction::None);

    // Hiding arms the router: the same generation is now presented.
    let mut env = hidden_env(5);
    env.visible = false;
    router.arm();
    assert_eq!(
        router.step(7, env, 100),
        HiddenAlertAction::Present {
            presentation: AlertPresentation::BalloonAndFlash,
            count: 5,
        }
    );
}

#[test]
fn router_clears_armed_after_one_evaluation() {
    let mut router = HiddenAlertRouter::new();
    // Consume generation 1 while visible.
    let visible = HiddenAlertEnv {
        visible: true,
        ..hidden_env(1)
    };
    assert!(matches!(
        router.step(1, visible, 0),
        HiddenAlertAction::Present { .. }
    ));
    router.set_flash_started(Some(0));
    router.arm();
    // Armed forces one evaluation even though the generation did not change.
    assert!(matches!(
        router.step(1, hidden_env(1), 10),
        HiddenAlertAction::Present { .. }
    ));
    // Not armed any more, same generation: silence.
    assert_eq!(router.step(1, hidden_env(1), 20), HiddenAlertAction::None);
}

#[test]
fn router_present_count_mirrors_the_pending_length() {
    for pending in [1u64, 2, 9, 64] {
        let mut router = HiddenAlertRouter::new();
        match router.step(1, hidden_env(pending), 0) {
            HiddenAlertAction::Present { count, .. } => assert_eq!(count, pending),
            other => panic!("expected Present for pending={pending}, got {other:?}"),
        }
    }
}

#[test]
fn router_does_not_present_an_empty_queue() {
    let mut router = HiddenAlertRouter::new();
    assert_eq!(router.step(1, hidden_env(0), 0), HiddenAlertAction::None);
    // Nothing was consumed: the next real alert with the same generation still
    // gets presented once pending is non-zero... which requires a new
    // generation in practice, but the armed path proves the state is intact.
    router.arm();
    assert!(matches!(
        router.step(1, hidden_env(1), 10),
        HiddenAlertAction::Present { .. }
    ));
}

#[test]
fn router_stops_the_flash_after_the_cap() {
    let mut router = HiddenAlertRouter::new();
    // Consume generation 1 and record the blinking episode start.
    assert!(matches!(
        router.step(1, hidden_env(1), 0),
        HiddenAlertAction::Present { .. }
    ));
    router.set_flash_started(Some(0));
    assert_eq!(
        router.step(1, hidden_env(1), FLASH_MAX_MS - 1),
        HiddenAlertAction::None
    );
    assert_eq!(
        router.step(1, hidden_env(1), FLASH_MAX_MS),
        HiddenAlertAction::StopFlash
    );
    // StopFlash is reported exactly once.
    assert_eq!(
        router.step(1, hidden_env(1), FLASH_MAX_MS + 50),
        HiddenAlertAction::None
    );
    assert_eq!(router.flash_started(), None);
}

#[test]
fn router_restarts_the_flash_for_a_new_generation() {
    let mut router = HiddenAlertRouter::new();
    assert!(matches!(
        router.step(1, hidden_env(1), 0),
        HiddenAlertAction::Present { .. }
    ));
    router.set_flash_started(Some(0));
    // A new alert past the old cap is a new episode and must be presented.
    assert!(matches!(
        router.step(2, hidden_env(2), FLASH_MAX_MS + 10),
        HiddenAlertAction::Present { .. }
    ));
}

// ---------------------------------------------------------------------------
// Notification icon identity
// ---------------------------------------------------------------------------

#[test]
fn icon_identity_is_deterministic_and_case_insensitive() {
    let lower = identity_guid_bytes("d:/tools/duthub/duthub.exe");
    let upper = identity_guid_bytes("D:/Tools/Duthub/Duthub.EXE");
    assert_eq!(lower, upper, "Windows paths compare case-insensitively");
    assert_eq!(lower, identity_guid_bytes("d:/tools/duthub/duthub.exe"));
}

#[test]
fn icon_identity_differs_between_executable_paths() {
    // Two copies of the app in different folders must never delete each
    // other's icon, so the GUID has to depend on the path.
    assert_ne!(
        identity_guid_bytes("d:/a/duthub.exe"),
        identity_guid_bytes("d:/b/duthub.exe")
    );
    assert_ne!(
        identity_guid_bytes("d:/a/duthub.exe"),
        identity_guid_bytes("d:/a/duthub2.exe")
    );
}

#[test]
fn icon_identity_is_an_rfc4122_version_5_name() {
    let bytes = identity_guid_bytes("c:/program files/duthub/duthub.exe");
    assert_eq!(bytes[6] & 0xf0, 0x50, "version nibble must be 5");
    assert_eq!(bytes[8] & 0xc0, 0x80, "RFC 4122 variant bits");
}

// ---------------------------------------------------------------------------
// Icon callback anchor (`NOTIFYICON_VERSION_4` wParam)
// ---------------------------------------------------------------------------

/// Packs a point the way the shell does for a v4 callback `wParam`
/// (`MAKELPARAM`: low word x, high word y).
fn pack(x: i16, y: i16) -> usize {
    usize::from(u16::from_ne_bytes(x.to_ne_bytes()))
        | (usize::from(u16::from_ne_bytes(y.to_ne_bytes())) << 16)
}

const WM_LBUTTONUP_LOCAL: u32 = 0x0202;
const WM_RBUTTONUP_LOCAL: u32 = 0x0205;
const WM_CONTEXTMENU_LOCAL: u32 = 0x007b;
const NIN_SELECT_LOCAL: u32 = 1024;
const NIN_KEYSELECT_LOCAL: u32 = 1025;

#[test]
fn callback_point_unpacks_signed_halves() {
    assert_eq!(callback_point(pack(1598, 1032)), (1598, 1032));
    assert_eq!(callback_point(0), (0, 0));
    // Multi-monitor screens have negative coordinates left of the primary.
    assert_eq!(callback_point(pack(-1920, 40)), (-1920, 40));
    // The packing is little-endian within each word, not a reinterpretation of
    // the whole value.
    assert_eq!(callback_point(0x0000_0005), (5, 0));
    assert_eq!(callback_point(0x0007_0000), (0, 7));
}

#[test]
fn selection_and_mouse_events_carry_an_anchor() {
    for event in [
        WM_LBUTTONUP_LOCAL,
        WM_RBUTTONUP_LOCAL,
        WM_CONTEXTMENU_LOCAL,
        NIN_SELECT_LOCAL,
        NIN_KEYSELECT_LOCAL,
    ] {
        assert!(
            callback_carries_anchor(event),
            "event {event:#06x} must carry an anchor"
        );
    }
    // Balloon events and unknown ids do not.
    for event in [1026u32, 1027, 1028, 1029, 0, 513, 0x0200] {
        assert!(
            !callback_carries_anchor(event),
            "event {event:#06x} must not carry an anchor"
        );
    }
}

#[test]
fn keyboard_menu_anchor_comes_from_the_callback_not_the_cursor() {
    // Keyboard invocation reports the icon corner; the mouse is elsewhere.
    assert_eq!(
        callback_anchor(WM_CONTEXTMENU_LOCAL, pack(1598, 1032)),
        Some((1598, 1032))
    );
    assert_eq!(
        callback_anchor(NIN_KEYSELECT_LOCAL, pack(1600, 1040)),
        Some((1600, 1040))
    );
}

#[test]
fn a_zero_anchor_falls_back_to_the_icon_rect() {
    // `(0, 0)` is what an undefined wParam looks like; trusting it would open
    // the menu in the screen corner, so the caller must fall back.
    assert_eq!(callback_anchor(WM_CONTEXTMENU_LOCAL, 0), None);
    // Negative anchors are real (secondary monitor on the left).
    assert_eq!(
        callback_anchor(WM_RBUTTONUP_LOCAL, pack(-40, 900)),
        Some((-40, 900))
    );
}

#[test]
fn events_without_an_anchor_never_look_at_wparam() {
    for event in [1026u32, 1028, 0, 513] {
        assert_eq!(
            callback_anchor(event, pack(1200, 800)),
            None,
            "event {event:#06x} must not supply a menu anchor"
        );
    }
}
