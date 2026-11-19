//! HDMI monitor-window state machine (`docs/design/CAPTURE_STREAM_BACKENDS.md` §6.2).
//!
//! Pure logic only — no GPUI types. The window lifecycle (`monitor_window.rs`)
//! and the frame pump feed events in and perform the returned actions; the
//! transition table itself is unit-testable here.
//!
//! Invariants:
//! - at most one float window exists;
//! - while `Floating`, closing or switching the side panel must **not** stop
//!   the preview (the float window keeps consuming frames);
//! - the main window closes only after the float window is gone.

/// Where the monitor preview currently lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorPlacement {
    /// Preview rendered in the right side panel.
    Docked,
    /// Preview handed to the float window; the side panel shows a placeholder.
    Floating,
    /// Main window is closing; no preview work remains.
    Closing,
}

/// Events that may change the placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorEvent {
    /// User pressed "float" while docked.
    OpenFloat,
    /// User pressed "redock" in the float window.
    Redock,
    /// Float window closed by its own X button (`on_window_closed`).
    UserClosedWindow,
    /// `close_side_panel` ran.
    SidePanelClosed,
    /// `open_side_panel` switched to another panel (Shell etc.).
    PanelSwitched,
    /// Frame pump reported a preview failure.
    PreviewFailed,
    /// Main window `on_window_should_close`.
    MainWindowClosing,
    /// Main window is gone (last window closed).
    MainWindowClosed,
}

/// Actions the caller performs, in the order returned by [`MonitorAction::steps`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorAction {
    None,
    OpenFloatWindow,
    CloseFloatWindow,
    FocusMainWindow,
    StopPreview,
    ShowDockedPlaceholder,
    /// `Redock`: close the float window, then activate the main window.
    CloseFloatWindowAndFocusMain,
    /// `PreviewFailed`: stop the preview, then close the float window.
    StopPreviewAndCloseFloatWindow,
}

impl MonitorAction {
    /// Individual actions this value performs, in application order.
    ///
    /// Composite variants exist because the design table lists two actions
    /// for `Redock` and `PreviewFailed` while [`next`] returns a single value.
    pub fn steps(self) -> &'static [MonitorAction] {
        match self {
            Self::None => &[],
            Self::OpenFloatWindow => &[Self::OpenFloatWindow],
            Self::CloseFloatWindow => &[Self::CloseFloatWindow],
            Self::FocusMainWindow => &[Self::FocusMainWindow],
            Self::StopPreview => &[Self::StopPreview],
            Self::ShowDockedPlaceholder => &[Self::ShowDockedPlaceholder],
            Self::CloseFloatWindowAndFocusMain => &[Self::CloseFloatWindow, Self::FocusMainWindow],
            Self::StopPreviewAndCloseFloatWindow => &[Self::StopPreview, Self::CloseFloatWindow],
        }
    }

    /// Whether this action includes `step` at any position.
    pub fn contains(self, step: MonitorAction) -> bool {
        self.steps().contains(&step)
    }
}

/// Next placement plus the action to perform.
///
/// The ten rows of §6.2 are implemented verbatim; remaining state/event pairs
/// are total-function extensions that keep the machine deterministic:
/// - `Docked + MainWindowClosing/MainWindowClosed` → `Closing` (no float to
///   close);
/// - `Floating + MainWindowClosed` → `Closing` + close the float window (the
///   close event may arrive after `MainWindowClosing`);
/// - `Floating + OpenFloat`, `Docked + Redock/UserClosedWindow` → no-op;
/// - `Closing` swallows every event (except `PreviewFailed`, which the table
///   marks "any state").
pub fn next(state: MonitorPlacement, event: MonitorEvent) -> (MonitorPlacement, MonitorAction) {
    use MonitorAction as A;
    use MonitorEvent as E;
    use MonitorPlacement as P;

    match (state, event) {
        // §6.2 table, first ten rows.
        (P::Docked, E::OpenFloat) => (P::Floating, A::OpenFloatWindow),
        (P::Floating, E::Redock) => (P::Docked, A::CloseFloatWindowAndFocusMain),
        (P::Floating, E::UserClosedWindow) => (P::Docked, A::ShowDockedPlaceholder),
        (P::Floating, E::SidePanelClosed | E::PanelSwitched) => (P::Floating, A::None),
        (P::Docked, E::SidePanelClosed | E::PanelSwitched) => (P::Docked, A::StopPreview),
        (P::Floating, E::MainWindowClosing) => (P::Closing, A::CloseFloatWindow),
        (P::Closing, E::MainWindowClosed) => (P::Closing, A::None),
        (_, E::PreviewFailed) => (P::Docked, A::StopPreviewAndCloseFloatWindow),

        // Total-function extensions (see rustdoc).
        (P::Docked, E::MainWindowClosing | E::MainWindowClosed) => (P::Closing, A::None),
        (P::Floating, E::MainWindowClosed) => (P::Closing, A::CloseFloatWindow),
        (P::Floating, E::OpenFloat) => (P::Floating, A::None),
        (P::Docked, E::Redock | E::UserClosedWindow) => (P::Docked, A::None),
        (P::Closing, _) => (P::Closing, A::None),
    }
}
