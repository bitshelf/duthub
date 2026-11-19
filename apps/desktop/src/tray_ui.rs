//! Pure decisions behind the tray: close disposition, alert presentation,
//! flash timing and the hidden-state alert router.
//!
//! Nothing here touches GPUI or the `windows` crate, so every rule can be
//! unit-tested without a desktop session. The Win32 side lives in
//! [`crate::tray`] and the desktop wiring lives in the bin target
//! (`workspace.rs`); both call into this module rather than re-deriving the
//! policy.
//!
//! The single owner of [`HiddenAlertRouter`] is the 50 ms service tick; see
//! `docs/design/TRAY_ALERT_DESIGN.md` §5.1.

/// What closing the main window should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDisposition {
    /// The tray is ready: hide the window and keep the process running.
    HideToTray,
    /// Tray unavailable (or the app is already quitting): native exit.
    Quit,
}

/// Decides whether a close request hides to the tray or quits.
///
/// `tray_ready` is `false` while the tray failed to install or was never
/// installed; in that case hiding would strand the window with no way back, so
/// the close degrades to a real exit. `quitting` wins over everything: once the
/// user picked "quit" from the tray menu, further close requests must destroy
/// the window instead of hiding it again.
pub fn close_disposition(tray_ready: bool, quitting: bool) -> CloseDisposition {
    if quitting || !tray_ready {
        CloseDisposition::Quit
    } else {
        CloseDisposition::HideToTray
    }
}

/// One ordered step of a close plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseStep {
    /// Drop any relay hold (press-and-hold test) and let the OFF write finish.
    ReleaseRelay,
    /// Close the monitor/float window before the main window goes away.
    CloseFloat,
    /// Hide the main window and show the notification icon.
    HideWindow,
    /// Destroy the main window so `quit_on_empty` can end the process.
    DestroyMain,
}

const HIDE_PLAN: [CloseStep; 3] = [
    CloseStep::ReleaseRelay,
    CloseStep::CloseFloat,
    CloseStep::HideWindow,
];

const QUIT_PLAN: [CloseStep; 3] = [
    CloseStep::ReleaseRelay,
    CloseStep::CloseFloat,
    CloseStep::DestroyMain,
];

/// The ordered steps for `disposition`.
///
/// `ReleaseRelay` is always first: hiding or exiting must never leave a relay
/// channel energised (`docs/design/TRAY_ALERT_DESIGN.md` §3.1). `CloseFloat` always
/// runs before the main window disappears, because a leftover float window
/// keeps `windows` non-empty and the process alive with no main window.
pub fn close_plan(disposition: CloseDisposition) -> &'static [CloseStep] {
    match disposition {
        CloseDisposition::HideToTray => &HIDE_PLAN,
        CloseDisposition::Quit => &QUIT_PLAN,
    }
}

/// How badly the OS currently wants notifications suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationGate {
    /// `QUNS_ACCEPTS_NOTIFICATIONS`: balloons may be shown.
    Allowed,
    /// `QUNS_QUIET_TIME`: only critical alerts may show a balloon.
    QuietTime,
    /// Busy / presentation / full-screen / locked / unknown: no balloon.
    Blocked,
}

/// How an alert should be surfaced right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertPresentation {
    /// The window is visible: the in-app dialog handles it.
    InAppDialog,
    /// Hidden, balloon allowed, animation allowed: balloon plus blinking icon.
    BalloonAndFlash,
    /// Hidden, balloon allowed, animation disabled: balloon only.
    BalloonOnly,
    /// Hidden, balloon suppressed but animation allowed: blink only.
    FlashOnly,
    /// Hidden and the OS refuses both channels: keep it queued.
    QueueOnly,
}

/// Blink half-period in milliseconds.
pub const FLASH_INTERVAL_MS: u64 = 500;

/// Hard cap for a single blinking episode in milliseconds.
pub const FLASH_MAX_MS: u64 = 30_000;

/// Evaluates the presentation for one alert.
///
/// `critical` is the caller's judgement about the alert that is being
/// presented (device alerts are critical, `agentNotice` is not). A blocked gate
/// never produces a balloon; quiet time only lets critical alerts through.
/// Disabling animations never suppresses a balloon, it only removes the blink,
/// because the balloon is the only channel the OS guarantees for a hidden icon.
pub fn alert_presentation(
    window_visible: bool,
    animations_enabled: bool,
    gate: NotificationGate,
    critical: bool,
) -> AlertPresentation {
    if window_visible {
        return AlertPresentation::InAppDialog;
    }
    let balloon = match gate {
        NotificationGate::Allowed => true,
        NotificationGate::QuietTime => critical,
        NotificationGate::Blocked => false,
    };
    match (balloon, animations_enabled) {
        (true, true) => AlertPresentation::BalloonAndFlash,
        (true, false) => AlertPresentation::BalloonOnly,
        (false, true) => AlertPresentation::FlashOnly,
        (false, false) => AlertPresentation::QueueOnly,
    }
}

/// Coarse destination of one alert, matching the vocabulary of the design and
/// the architecture report (`docs/design/TRAY_ALERT_DESIGN.md` §12).
///
/// [`AlertPresentation`] stays the exact decision the renderer needs; this is
/// the summary the lifecycle coordinator uses to pick the ordered tray
/// actions. `Tray` covers both balloon-only and flash-only episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertRoute {
    /// The visible window shows the in-app dialog.
    Dialog,
    /// The notification area carries it (balloon or blink).
    Tray,
    /// Balloon plus blinking icon.
    Both,
    /// Neither channel is available now; the alert stays queued.
    Queue,
}

/// Collapses a presentation into the routing summary.
pub fn alert_route(presentation: AlertPresentation) -> AlertRoute {
    match presentation {
        AlertPresentation::InAppDialog => AlertRoute::Dialog,
        AlertPresentation::BalloonAndFlash => AlertRoute::Both,
        AlertPresentation::BalloonOnly | AlertPresentation::FlashOnly => AlertRoute::Tray,
        AlertPresentation::QueueOnly => AlertRoute::Queue,
    }
}

/// Why a blinking episode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashStop {
    /// The main window came back.
    Restored,
    /// The user clicked the balloon.
    BalloonClicked,
    /// `FLASH_MAX_MS` elapsed.
    Timeout,
    /// The alert was acknowledged inside the app.
    Acknowledged,
    /// The system animation preference is off.
    AnimationsDisabled,
}

/// Whether blinking must stop now.
///
/// Any explicit reason stops immediately; without one, the episode stops once
/// it has run for [`FLASH_MAX_MS`].
pub fn flash_should_stop(started_ms: u64, now_ms: u64, reason: Option<FlashStop>) -> bool {
    if reason.is_some() {
        return true;
    }
    now_ms.saturating_sub(started_ms) >= FLASH_MAX_MS
}

/// Whether a settings change must stop the blink (`SPI_SETCLIENTAREAANIMATION`
/// turned off mid-episode).
///
/// The tray thread reacts to `WM_SETTINGCHANGE` with this; the 50 ms tick
/// cannot see the preference change because it only re-evaluates on a new
/// alert generation or a flash timeout.
pub fn flash_stops_for_animation_change(flash_active: bool, animations_enabled: bool) -> bool {
    flash_active && !animations_enabled
}

/// The icon state for the current blink tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashFrame {
    /// Alert icon shown.
    Lit,
    /// Normal app icon shown.
    Dark,
    /// The episode is over; restore the normal icon and stop the timer.
    Stop,
}

/// Which icon a blink tick should show.
///
/// The first half-period is `Lit` so the alert icon appears immediately after
/// the balloon.
pub fn flash_frame(started_ms: u64, now_ms: u64) -> FlashFrame {
    let elapsed = now_ms.saturating_sub(started_ms);
    if elapsed >= FLASH_MAX_MS {
        return FlashFrame::Stop;
    }
    if (elapsed / FLASH_INTERVAL_MS).is_multiple_of(2) {
        FlashFrame::Lit
    } else {
        FlashFrame::Dark
    }
}

/// Pure bookkeeping for one blinking episode.
///
/// The tray thread owns the actual timer; this value only records when the
/// episode started so both the UI tick (`HiddenAlertRouter`) and tests can ask
/// "must it stop?" and "which frame?" without duplicating the timing rules.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlashPlan {
    started_ms: Option<u64>,
}

impl FlashPlan {
    /// Starts a new episode at `now_ms` (restarts one that is already running).
    pub fn start(&mut self, now_ms: u64) {
        self.started_ms = Some(now_ms);
    }

    /// Ends the episode. Idempotent.
    pub fn stop(&mut self) {
        self.started_ms = None;
    }

    /// When the current episode started, if any.
    pub fn started(&self) -> Option<u64> {
        self.started_ms
    }

    /// Whether an episode is running.
    pub fn is_active(&self) -> bool {
        self.started_ms.is_some()
    }

    /// Whether the episode must end now (only an active episode can expire).
    pub fn should_stop(&self, now_ms: u64) -> bool {
        self.started_ms
            .is_some_and(|started| flash_should_stop(started, now_ms, None))
    }

    /// The frame to paint now; `Stop` when no episode is running.
    pub fn frame(&self, now_ms: u64) -> FlashFrame {
        match self.started_ms {
            Some(started) => flash_frame(started, now_ms),
            None => FlashFrame::Stop,
        }
    }
}

/// Everything the router needs for one evaluation, gathered by the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HiddenAlertEnv {
    /// The main window is currently visible.
    pub visible: bool,
    /// An in-app modal dialog is open (written by `render_root` every frame).
    pub dialog_open: bool,
    /// Number of alerts waiting to be presented.
    pub pending: u64,
    /// `SPI_GETCLIENTAREAANIMATION` says animations are on.
    pub animations: bool,
    /// Current OS notification gate.
    pub gate: NotificationGate,
    /// The alert being presented is device-level (not an agent notice).
    pub critical: bool,
}

/// What the tick must do after an evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenAlertAction {
    /// Nothing to do; do not move the generation cursor.
    None,
    /// Present the pending alerts with `presentation`; `count` is how many.
    Present {
        presentation: AlertPresentation,
        count: u64,
    },
    /// Stop the blinking episode and restore the normal icon.
    StopFlash,
}

/// State machine that decides when a queued alert becomes visible.
///
/// It only tracks three things: the last generation it acted on, whether a
/// forced evaluation was requested, and the current blinking episode
/// ([`FlashPlan`]). All I/O (balloons, timers, dialogs) belongs to the caller.
#[derive(Debug, Default)]
pub struct HiddenAlertRouter {
    last_generation: u64,
    flash: FlashPlan,
    armed: bool,
}

impl HiddenAlertRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluates one tick.
    ///
    /// A generation change (or [`Self::arm`]) is required to present again, so
    /// a steady alert queue does not re-notify on every tick. While the window
    /// is visible **and** a dialog is open the generation cursor is left alone:
    /// the alert is presented again once the dialog closes instead of being
    /// silently consumed behind it.
    pub fn step(&mut self, generation: u64, env: HiddenAlertEnv, now_ms: u64) -> HiddenAlertAction {
        let armed = std::mem::replace(&mut self.armed, false);
        let generation_changed = generation != self.last_generation;
        let flash_expired = self.flash.should_stop(now_ms);

        if !armed && !generation_changed {
            if flash_expired {
                self.flash.stop();
                return HiddenAlertAction::StopFlash;
            }
            return HiddenAlertAction::None;
        }

        // Blocked behind an open dialog: consume nothing, so the alert is
        // presented on a later tick.
        if env.visible && env.dialog_open {
            return HiddenAlertAction::None;
        }

        if env.pending == 0 {
            if flash_expired {
                self.flash.stop();
                return HiddenAlertAction::StopFlash;
            }
            return HiddenAlertAction::None;
        }

        self.last_generation = generation;
        let presentation = if env.visible {
            AlertPresentation::InAppDialog
        } else {
            alert_presentation(false, env.animations, env.gate, env.critical)
        };
        HiddenAlertAction::Present {
            presentation,
            count: env.pending,
        }
    }

    /// Forces one unconditional evaluation on the next tick.
    ///
    /// Called when the window becomes hidden: at that moment the alert queue
    /// may already hold entries whose generation was consumed while visible.
    pub fn arm(&mut self) {
        self.armed = true;
    }

    /// Records the blinking episode start (`Some`) or its end (`None`).
    ///
    /// The caller passes `Some(now_ms)` right after `set_alert(true)` and
    /// `None` after `set_alert(false)`; the router uses it to honour
    /// [`FLASH_MAX_MS`] even if the tray thread's own timer is lost.
    pub fn set_flash_started(&mut self, now_ms: Option<u64>) {
        match now_ms {
            Some(now_ms) => self.flash.start(now_ms),
            None => self.flash.stop(),
        }
    }

    /// When the current blinking episode started, if one is running.
    pub fn flash_started(&self) -> Option<u64> {
        self.flash.started()
    }

    /// The blinking plan (pure state, used by the tick's flash decisions).
    pub fn flash_plan(&self) -> FlashPlan {
        self.flash
    }
}

/// Derives the stable 128-bit notification-icon identity for an executable
/// path.
///
/// The GUID is stored in the RFC 4122 byte order with version 5 and the RFC
/// variant bits set. It is deterministic per path (and case-insensitive, like
/// Windows path comparison), so the shell keeps the icon's placement across
/// restarts of the same executable. Two copies of the app in different
/// directories get different GUIDs and therefore never delete each other's
/// icons — which is why this must not be a compile-time constant.
pub fn identity_guid_bytes(exe_path: &str) -> [u8; 16] {
    let normalized = exe_path.to_lowercase();
    // Two independent FNV-1a 64 accumulators give the 128 bits. The second one
    // mixes the length first so that strings differing only by trailing bytes
    // do not collapse into the same prefix.
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut b: u64 = 0x9e37_79b9_7f4a_7c15 ^ normalized.len() as u64;
    for byte in normalized.as_bytes() {
        a = (a ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        b = (b ^ u64::from(*byte).wrapping_add(0x51ed_2701)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_be_bytes());
    bytes[8..].copy_from_slice(&b.to_be_bytes());
    // RFC 4122 version 5 + variant 10xx.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

// ---------------------------------------------------------------------------
// Icon callback anchor (`NOTIFYICON_VERSION_4` wParam packing)
// ---------------------------------------------------------------------------

// Event values as plain numbers so this module stays free of the `windows`
// crate (`shellapi.h`: `WM_LBUTTONUP`, `WM_RBUTTONUP`, `WM_CONTEXTMENU`,
// `NIN_SELECT`, `NIN_KEYSELECT`).
const WM_LBUTTONUP_EVENT: u32 = 0x0202;
const WM_RBUTTONUP_EVENT: u32 = 0x0205;
const WM_CONTEXTMENU_EVENT: u32 = 0x007b;
const NIN_SELECT_EVENT: u32 = 1024;
const NIN_KEYSELECT_EVENT: u32 = 1025;

/// Splits the point packed into `wParam` of a `NOTIFYICON_VERSION_4` callback
/// (`GET_X_LPARAM`/`GET_Y_LPARAM`: signed 16-bit halves, little-endian order).
pub fn callback_point(wparam: usize) -> (i32, i32) {
    let x = (wparam & 0xffff) as u16 as i16 as i32;
    let y = ((wparam >> 16) & 0xffff) as u16 as i16 as i32;
    (x, y)
}

/// Whether `event` (the low word of `lParam`) carries a screen position in
/// `wParam`.
///
/// Icon selection and mouse events do. A popup menu invoked from the keyboard
/// reports the icon's anchor, which is why the menu must not be placed at the
/// cursor for those events.
pub fn callback_carries_anchor(event: u32) -> bool {
    matches!(
        event,
        WM_LBUTTONUP_EVENT
            | WM_RBUTTONUP_EVENT
            | WM_CONTEXTMENU_EVENT
            | NIN_SELECT_EVENT
            | NIN_KEYSELECT_EVENT
    )
}

/// The anchor a `NOTIFYICON_VERSION_4` callback wants its popup placed at.
///
/// Returns `None` when the event carries no anchor, or when the packed point is
/// `(0, 0)` — that is what an undefined `wParam` looks like, and trusting it
/// would open the menu in the screen corner. Callers then fall back to the icon
/// rectangle (keyboard invocation) or the cursor.
pub fn callback_anchor(event: u32, wparam: usize) -> Option<(i32, i32)> {
    if !callback_carries_anchor(event) {
        return None;
    }
    match callback_point(wparam) {
        (0, 0) => None,
        point => Some(point),
    }
}
