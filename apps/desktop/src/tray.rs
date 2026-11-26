//! Windows notification-area (tray) host and the only Win32 port in the
//! desktop app.
//!
//! This module is the *only* place in the desktop crate that references the
//! `windows` crate or raw `HWND`s (`docs/design/TRAY_ALERT_DESIGN.md` §12). The rest
//! of the app talks to the small [`TrayBackend`] port (or the concrete
//! [`TrayHandle`]) and never calls Win32 itself, which keeps the platform
//! details (hidden top-level host window, `NOTIFYICON_VERSION_4` callbacks,
//! `TrackPopupMenuEx` modal loop, `HICON` ownership) in one place.
//!
//! Two implementations sit behind the port:
//!
//! - `platform` on Windows: a dedicated thread owns a hidden
//!   `WS_POPUP | WS_EX_TOOLWINDOW` window and its `GetMessageW` loop.
//!   `TrackPopupMenuEx` is a blocking modal loop, so it must never run on the
//!   GPUI thread.
//! - `platform` everywhere else: an inert handle; [`install`] fails, so the
//!   close gate (`tray_ui::close_disposition`) degrades to a real exit instead
//!   of hiding a window with no way back.
//!
//! Design: `docs/design/TRAY_ALERT_DESIGN.md` §4. Invariants the Win32 side keeps:
//!
//! - The icon is identified by a GUID derived from the executable path, so two
//!   copies of the app in different folders cannot delete each other's icon.
//! - The icon only exists while the main window is hidden. `install` creates
//!   the thread and window but adds no icon; `hide_main` adds it, `show_main`
//!   (via `set_icon_visible(false)`) removes it.
//! - Icon operations run on the tray thread and report their real result over a
//!   bounded reply channel, so `hide_main` can refuse to hide the window when
//!   the shell rejected the icon. A hidden window with no icon would be
//!   unreachable.
//! - A failed `TaskbarCreated` replay surfaces as [`TrayEvent::IconLost`] so the
//!   UI can bring the window back instead of pretending everything is fine.

#![cfg_attr(
    windows,
    expect(
        clippy::undocumented_unsafe_blocks,
        reason = "Win32 tray shell FFI isolated in tray.rs"
    )
)]

use std::fmt;

/// Icon/balloon severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayNotifyLevel {
    Info,
    Warning,
    Error,
}

/// Events the tray thread reports to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Show the main window (icon click, balloon click, keyboard select).
    Show,
    /// Quit (tray menu).
    Quit,
    /// The shell refused to re-add the icon (for example after
    /// `TaskbarCreated`). The UI must restore the main window and surface an
    /// error: the process must never stay hidden without an icon.
    IconLost,
}

/// Why tray installation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayError {
    ThreadSpawn(String),
    WindowCreate(String),
    IconAdd(String),
}

impl fmt::Display for TrayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrayError::ThreadSpawn(detail) => write!(f, "tray thread spawn failed: {detail}"),
            TrayError::WindowCreate(detail) => write!(f, "tray window creation failed: {detail}"),
            TrayError::IconAdd(detail) => write!(f, "tray icon load failed: {detail}"),
        }
    }
}

impl std::error::Error for TrayError {}

/// Localized labels for the tray context menu and the icon tooltip.
#[derive(Debug, Clone)]
pub struct TrayOptions {
    pub tooltip: String,
    pub show_label: String,
    pub quit_label: String,
}

/// Test/diagnostic snapshot of the tray state.
///
/// `icon_rect` comes from `Shell_NotifyIconGetRect`, so `Some` means the shell
/// really hosts the icon somewhere (possibly in the overflow area). The other
/// fields are the tray's own desired state plus `IsWindowVisible` for the main
/// window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayProbe {
    pub hwnd: isize,
    pub taskbar_created_msg: u32,
    pub icon_rect: Option<[i32; 4]>,
    pub icon_visible: bool,
    pub main_visible: bool,
    pub alerting: bool,
    pub balloons_sent: u32,
}

/// One balloon request handed to the tray port.
///
/// `respect_quiet_time` is `false` for critical device alerts: Windows drops a
/// balloon outright when `NIIF_RESPECT_QUIET_TIME` is set during quiet time
/// (it is not queued), so a critical alert must omit the flag or the
/// presentation policy's "QuietTime only lets critical through" could never be
/// honoured. Non-critical alerts keep the flag; the presentation matrix
/// (`tray_ui::alert_presentation`) does not ask for a balloon while the gate is
/// quiet anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayNotification {
    pub title: String,
    pub body: String,
    pub level: TrayNotifyLevel,
    pub respect_quiet_time: bool,
}

/// The port the rest of the app depends on.
///
/// [`install`] is the constructor half; the methods below are the effects the
/// UI may request. The lifecycle coordinator (`tray_lifecycle`) and its
/// executor only need these calls, so a test can replace the whole Win32 side
/// with a recording mock (`tests/tray_lifecycle.rs`). A Windows App SDK toast
/// (or any other channel) is a new implementation of the same methods, so the
/// workspace does not change when the channel does.
pub trait TrayBackend {
    /// Hides the main window and shows the notification icon.
    ///
    /// `false` means the icon could not be added; the window is then **left
    /// visible** (the icon is added before the window is hidden), so the caller
    /// must not mark the app as hidden.
    fn hide_main(&self) -> bool;
    /// Shows the main window again (keeps minimized/maximized state).
    fn show_main(&self) -> bool;
    /// Adds (`true`) or removes (`false`) the notification icon.
    fn set_visible(&self, visible: bool) -> bool;
    /// Replaces the icon tooltip text.
    fn set_tooltip(&self, text: &str);
    /// Shows (or replaces) the alert balloon.
    fn notify_balloon(&self, notification: &TrayNotification);
    /// Starts blinking the alert icon.
    fn start_flash(&self);
    /// Stops blinking and restores the normal icon.
    fn stop_flash(&self);
    /// Drains queued `Show`/`Quit`/`IconLost` events.
    fn drain_events(&self) -> Vec<TrayEvent>;
    /// Removes the icon and stops the tray thread (idempotent).
    fn remove(&self);
}

#[cfg(windows)]
pub use platform::{
    MENU_QUIT_ID, MENU_SHOW_ID, install_with_hwnd, menu_command_event, self_test_requested,
    test_menu_layout,
};
pub use platform::{TrayHandle, animations_enabled, install, notification_gate, single_instance};

#[cfg(windows)]
mod platform {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::HBRUSH;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Controls::{LIM_SMALL, LoadIconMetric};
    use windows::Win32::UI::Shell::{
        NIF_GUID, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_NONE,
        NIIF_RESPECT_QUIET_TIME, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETFOCUS, NIM_SETVERSION,
        NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT, NIN_BALLOONUSERCLICK, NIN_SELECT,
        NOTIFYICON_VERSION_4, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, QUNS_ACCEPTS_NOTIFICATIONS,
        QUNS_QUIET_TIME, SHQueryUserNotificationState, Shell_NotifyIconGetRect, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CopyIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
        DestroyMenu, DestroyWindow, DispatchMessageW, FLASHW_ALL, FLASHW_STOP, FLASHW_TIMERNOFG,
        FLASHWINFO, FlashWindowEx, GetCursorPos, GetMenuItemCount, GetMenuItemID, GetMessageW,
        HCURSOR, HICON, HMENU, IDI_WARNING, IsIconic, IsWindow, IsWindowVisible, KillTimer,
        LoadIconW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, PostThreadMessageW,
        RegisterClassW, RegisterWindowMessageW, SPI_GETCLIENTAREAANIMATION, SW_HIDE, SW_SHOW,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetForegroundWindow, SetTimer, ShowWindowAsync,
        SystemParametersInfoW, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
        TranslateMessage, WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_QUIT,
        WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASS_STYLES, WNDCLASSW, WS_EX_TOOLWINDOW,
        WS_POPUP,
    };
    use windows::core::{BOOL, GUID, PCWSTR};

    use super::*;
    use crate::tray_ui::{self, FLASH_INTERVAL_MS, FLASH_MAX_MS, NotificationGate};

    /// Tray icon resource id (`assets/app.ico`). `build.rs` embeds it as ID 1.
    const RESOURCE_APP_ICON: u16 = 1;
    /// Alert icon resource id expected from `assets/tray-alert.ico`, ID 2.
    ///
    /// That resource is **not embedded yet** (`build.rs` only ships ID 1), so
    /// [`load_alert_icon`] currently falls back to a private copy of the system
    /// warning icon; the resource wins automatically once it exists.
    const RESOURCE_ALERT_ICON: u16 = 2;

    /// Icon callback message (`NOTIFYICON_VERSION_4` event packing).
    const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
    /// Re-aligns the shell with the shared desired state and drains pending
    /// commands. Idempotent, so it is safe to post liberally.
    const WM_TRAY_POKE: u32 = WM_APP + 2;

    /// Blink timer id.
    const FLASH_TIMER_ID: usize = 1;

    /// `NIN_KEYSELECT` is not exported by `windows 0.62.2`; the value is stable in
    /// `shellapi.h` (`NIN_SELECT + 1`).
    const NIN_KEYSELECT: u32 = 1025;

    /// Character budgets: the shell stores the title in a 64-unit buffer and the
    /// body in a 256-unit buffer, both including the terminating NUL.
    const TITLE_MAX_UNITS: usize = 48;
    const BODY_MAX_UNITS: usize = 200;
    const TIP_MAX_UNITS: usize = 127;

    /// How long a caller waits for the tray thread to answer one command. The tray
    /// thread answers immediately unless it is inside the popup-menu modal loop,
    /// which still dispatches posted messages — so this only trips if the thread is
    /// wedged.
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
    /// Upper bound for tray thread start-up.
    const INIT_TIMEOUT: Duration = Duration::from_secs(5);
    /// Upper bound for joining the tray thread during shutdown.
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
    /// Cap on the pending UI event queue; events are idempotent hints, so dropping
    /// the oldest under a flood is safe.
    const MAX_EVENTS: usize = 64;

    /// A tray icon, the thread that owns it and the main window handle.
    ///
    /// Dropping the handle removes the icon and stops the tray thread; `remove` is
    /// idempotent so an explicit call before the process exits is also fine.
    pub struct TrayHandle {
        main_hwnd: isize,
        enabled: bool,
        shared: Arc<Shared>,
        commands: Option<Sender<TrayCommand>>,
        thread: Mutex<Option<JoinHandle<()>>>,
        done: Mutex<Option<Receiver<()>>>,
    }

    struct Shared {
        guid: GUID,
        tray_hwnd: AtomicIsize,
        tray_tid: AtomicU32,
        taskbar_created_msg: AtomicU32,
        icon_added: AtomicBool,
        alerting: AtomicBool,
        balloons_sent: AtomicU32,
        removed: AtomicBool,
        tooltip: Mutex<String>,
        /// Popup-menu labels `(show, quit)`, read on every popup so a language
        /// switch after [`install`] does not leave the menu in the old language.
        menu_labels: Mutex<(String, String)>,
        events: Mutex<VecDeque<TrayEvent>>,
    }

    impl Shared {
        fn new(guid: GUID, tooltip: String, menu_labels: (String, String)) -> Self {
            Self {
                guid,
                tray_hwnd: AtomicIsize::new(0),
                tray_tid: AtomicU32::new(0),
                taskbar_created_msg: AtomicU32::new(0),
                icon_added: AtomicBool::new(false),
                alerting: AtomicBool::new(false),
                balloons_sent: AtomicU32::new(0),
                removed: AtomicBool::new(false),
                tooltip: Mutex::new(tooltip),
                menu_labels: Mutex::new(menu_labels),
                events: Mutex::new(VecDeque::new()),
            }
        }
    }

    /// Work the UI thread hands to the tray thread.
    enum TrayCommand {
        SetIconVisible {
            visible: bool,
            reply: SyncSender<Result<(), String>>,
        },
        SetTooltip {
            text: String,
        },
        SetAlert {
            alerting: bool,
        },
        Notify {
            notification: TrayNotification,
        },
        Remove {
            reply: SyncSender<()>,
        },
    }

    /// State owned by the tray thread (and by the WndProc that runs on it).
    struct TrayCtx {
        shared: Arc<Shared>,
        base_icon: HICON,
        alert_icon: Option<HICON>,
        flash_started: Mutex<Option<Instant>>,
        flash_lit: AtomicBool,
        commands: Mutex<Option<Receiver<TrayCommand>>>,
        menu_active: AtomicBool,
    }

    thread_local! {
        /// Set on the tray thread before the host window exists. Cloned out before
        /// use so no borrow guard is ever held across a call that can pump messages
        /// (`TrackPopupMenuEx` re-enters the WndProc).
        static TRAY_CTX: RefCell<Option<Rc<TrayCtx>>> = const { RefCell::new(None) };
    }

    /// Whether the process was started with `--self-test`.
    ///
    /// The smoke test renders a few frames and exits; it must not leave a
    /// notification icon behind, so [`install`] refuses to start a tray thread in
    /// that mode and returns an inert handle instead.
    pub fn self_test_requested() -> bool {
        std::env::args_os().any(|arg| arg.to_str() == Some("--self-test"))
    }

    /// Installs the tray host and returns a handle for the main window.
    ///
    /// Only the window handle is taken from `window`; no icon is created yet (see
    /// [`TrayHandle::hide_main`]). Call this from the first `render_root` frame:
    /// that is the only place with a `&mut Window`, and the tray must not be
    /// installed earlier (the window does not exist yet).
    pub fn install(
        window: &mut gpui_kit::Window,
        options: TrayOptions,
    ) -> Result<TrayHandle, TrayError> {
        let main_hwnd = main_hwnd_from_window(window)?;
        install_with_hwnd(main_hwnd, options)
    }

    /// [`install`] for callers that already own an `HWND` (Win32 integration
    /// tests). Production code must use [`install`].
    #[doc(hidden)]
    pub fn install_with_hwnd(
        main_hwnd: isize,
        options: TrayOptions,
    ) -> Result<TrayHandle, TrayError> {
        if main_hwnd == 0 {
            return Err(TrayError::WindowCreate(
                "main window handle is null".to_string(),
            ));
        }
        let exe_path = std::env::current_exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let guid = guid_from_bytes(tray_ui::identity_guid_bytes(&exe_path));
        let shared = Arc::new(Shared::new(
            guid,
            options.tooltip.clone(),
            (options.show_label.clone(), options.quit_label.clone()),
        ));

        if self_test_requested() {
            // --self-test only renders frames; installing a tray would leave an
            // icon behind after the test process exits.
            eprintln!("[tray] --self-test: tray installation skipped");
            return Ok(TrayHandle {
                main_hwnd,
                enabled: false,
                shared,
                commands: None,
                thread: Mutex::new(None),
                done: Mutex::new(None),
            });
        }

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("duthub-tray".to_string())
            .spawn(move || tray_thread_main(thread_shared, cmd_rx, ready_tx, done_tx));
        let thread = match spawned {
            Ok(thread) => thread,
            Err(err) => return Err(TrayError::ThreadSpawn(err.to_string())),
        };

        match ready_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(())) => Ok(TrayHandle {
                main_hwnd,
                enabled: true,
                shared,
                commands: Some(cmd_tx),
                thread: Mutex::new(Some(thread)),
                done: Mutex::new(Some(done_rx)),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(err) => {
                // The thread never reported readiness. Ask it to stop and give it a
                // bounded chance to exit, so no bare thread is left behind.
                let tid = shared.tray_tid.load(Ordering::SeqCst);
                if tid != 0 {
                    unsafe {
                        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
                    }
                }
                if done_rx.recv_timeout(SHUTDOWN_TIMEOUT).is_ok() {
                    let _ = thread.join();
                } else {
                    eprintln!("[tray] tray thread did not shut down after init failure: {err}");
                }
                Err(TrayError::WindowCreate(format!(
                    "tray initialisation did not complete: {err}"
                )))
            }
        }
    }

    fn main_hwnd_from_window(window: &mut gpui_kit::Window) -> Result<isize, TrayError> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let handle = window
            .window_handle()
            .map_err(|err| TrayError::WindowCreate(format!("window_handle: {err}")))?;
        match handle.as_raw() {
            RawWindowHandle::Win32(win32) => Ok(win32.hwnd.get()),
            other => Err(TrayError::WindowCreate(format!(
                "unsupported raw window handle: {other:?}"
            ))),
        }
    }

    fn guid_from_bytes(bytes: [u8; 16]) -> GUID {
        GUID::from_u128(u128::from_be_bytes(bytes))
    }

    impl TrayHandle {
        /// Hides the main window and shows the notification icon.
        ///
        /// Returns `false` when the icon could not be added; in that case the
        /// window is **left visible** (the icon is added before the window is
        /// hidden), so the caller must not mark the app as hidden.
        pub fn hide_main(&self) -> bool {
            if !self.enabled || !self.main_window_valid() {
                return false;
            }
            // Add the icon first: if the shell refuses it, nothing was hidden yet
            // and the "no hidden window without a way back" invariant holds without
            // an asynchronous rollback.
            if !self.set_icon_visible(true) {
                return false;
            }
            unsafe {
                let _ = ShowWindowAsync(self.main_window(), SW_HIDE);
            }
            true
        }

        /// Shows the main window again.
        ///
        /// Uses `SW_SHOW`, not `SW_SHOWNORMAL`: the latter restores the original
        /// size and would drop a maximized window back to windowed.
        pub fn show_main(&self) -> bool {
            if !self.enabled || !self.main_window_valid() {
                return false;
            }
            unsafe {
                let _ = ShowWindowAsync(self.main_window(), SW_SHOW);
            }
            true
        }

        /// Whether the main window is currently visible.
        pub fn main_visible(&self) -> bool {
            self.main_window_valid() && unsafe { IsWindowVisible(self.main_window()).as_bool() }
        }

        /// Whether the main window is currently minimized to the taskbar.
        pub fn main_minimized(&self) -> bool {
            self.main_window_valid() && unsafe { IsIconic(self.main_window()).as_bool() }
        }

        /// Flashes the main window's taskbar button until the window comes to
        /// the foreground (no-op if the window is already foreground).
        pub fn flash_taskbar(&self) {
            if !self.main_window_valid() {
                return;
            }
            let info = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd: self.main_window(),
                dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
                uCount: 0,
                dwTimeout: 0,
            };
            unsafe {
                let _ = FlashWindowEx(&info);
            }
        }

        /// Stops any active taskbar button flash on the main window.
        pub fn stop_taskbar_flash(&self) {
            if !self.main_window_valid() {
                return;
            }
            let info = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd: self.main_window(),
                dwFlags: FLASHW_STOP,
                uCount: 0,
                dwTimeout: 0,
            };
            unsafe {
                let _ = FlashWindowEx(&info);
            }
        }

        /// Adds or removes the notification icon.
        ///
        /// `true` means the desired state took effect. Adding reports the real
        /// `NIM_ADD` result (the caller must fall back to a visible window on
        /// `false`); removing is best effort but always leaves the desired state at
        /// "no icon" so a stale shell entry can never block a later hide.
        pub fn set_icon_visible(&self, visible: bool) -> bool {
            if !self.enabled {
                return false;
            }
            let Some(sender) = self.commands.as_ref() else {
                return false;
            };
            let (reply, answer) = mpsc::sync_channel(1);
            if sender
                .send(TrayCommand::SetIconVisible { visible, reply })
                .is_err()
            {
                return false;
            }
            self.request_command_drain();
            match answer.recv_timeout(COMMAND_TIMEOUT) {
                Ok(Ok(())) => true,
                Ok(Err(err)) => {
                    eprintln!("[tray] set_icon_visible({visible}) failed: {err}");
                    false
                }
                Err(_) => {
                    eprintln!("[tray] set_icon_visible({visible}) timed out");
                    false
                }
            }
        }

        /// Replaces the icon tooltip text (fire and forget).
        pub fn set_tooltip(&self, text: &str) {
            if let Ok(mut tooltip) = self.shared.tooltip.lock() {
                *tooltip = text.to_string();
            }
            if !self.enabled {
                return;
            }
            if let Some(sender) = self.commands.as_ref()
                && sender
                    .send(TrayCommand::SetTooltip {
                        text: text.to_string(),
                    })
                    .is_ok()
            {
                self.request_command_drain();
            }
        }

        /// Replaces the popup-menu labels.
        ///
        /// The labels are captured once at [`install`]; call this when the UI
        /// language changes so the context menu is not stuck in the old one.
        /// The tray thread reads them when the menu opens, so no round trip is
        /// needed and a popup that is already open keeps its labels.
        pub fn set_menu_labels(&self, show_label: &str, quit_label: &str) {
            if let Ok(mut labels) = self.shared.menu_labels.lock() {
                *labels = (show_label.to_string(), quit_label.to_string());
            }
        }

        /// The popup-menu labels the next popup will use.
        #[doc(hidden)]
        pub fn test_menu_labels(&self) -> (String, String) {
            self.shared
                .menu_labels
                .lock()
                .map(|labels| labels.clone())
                .unwrap_or_default()
        }

        /// Starts (`true`) or stops (`false`) the blinking alert state.
        ///
        /// Blinking is capped at [`FLASH_MAX_MS`] by the tray thread itself and is
        /// skipped while the system animation preference is off or no alert icon is
        /// available — the balloon is then the only channel.
        pub fn set_alert(&self, alerting: bool) {
            if !self.enabled {
                return;
            }
            if let Some(sender) = self.commands.as_ref()
                && sender.send(TrayCommand::SetAlert { alerting }).is_ok()
            {
                self.request_command_drain();
            }
        }

        /// Shows a balloon with `title` (≤48 chars) and `body` (≤200 chars).
        ///
        /// The caller owns the wording: pass the alert's real detail/summary as the
        /// body rather than reusing the title. The shell shows one balloon at a
        /// time, so a newer call replaces a balloon that is still visible; merging
        /// and rate limiting belong to the alert queue, not here.
        ///
        /// This compatibility wrapper respects Windows quiet time; critical
        /// alerts go through [`Self::notify_balloon`] with
        /// `respect_quiet_time = false` instead.
        pub fn notify(&self, title: &str, body: &str, level: TrayNotifyLevel) {
            self.notify_balloon(&TrayNotification {
                title: title.to_string(),
                body: body.to_string(),
                level,
                respect_quiet_time: true,
            });
        }

        /// Shows (or replaces) the alert balloon.
        ///
        /// `notification.respect_quiet_time == false` omits
        /// `NIIF_RESPECT_QUIET_TIME`, which is the only way a critical alert can
        /// appear during Windows quiet time (the shell would drop it otherwise).
        pub fn notify_balloon(&self, notification: &TrayNotification) {
            if !self.enabled {
                return;
            }
            if let Some(sender) = self.commands.as_ref()
                && sender
                    .send(TrayCommand::Notify {
                        notification: notification.clone(),
                    })
                    .is_ok()
            {
                self.request_command_drain();
            }
        }

        /// Drains queued `Show`/`Quit`/`IconLost` events.
        pub fn drain_events(&self) -> Vec<TrayEvent> {
            let mut events = Vec::new();
            if let Ok(mut queue) = self.shared.events.lock() {
                events.extend(queue.drain(..));
            }
            events
        }

        /// Removes the icon and stops the tray thread. Safe to call twice.
        pub fn remove(&self) {
            if self.shared.removed.swap(true, Ordering::SeqCst) {
                return;
            }
            if self.enabled {
                if let Some(sender) = self.commands.as_ref() {
                    let (reply, answer) = mpsc::sync_channel(1);
                    if sender.send(TrayCommand::Remove { reply }).is_ok() {
                        self.request_command_drain();
                        let _ = answer.recv_timeout(COMMAND_TIMEOUT);
                    }
                }
                let tid = self.shared.tray_tid.load(Ordering::SeqCst);
                if tid != 0 {
                    unsafe {
                        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
                    }
                }
            }

            let thread = self.thread.lock().ok().and_then(|mut slot| slot.take());
            let done = self.done.lock().ok().and_then(|mut slot| slot.take());
            let Some(thread) = thread else {
                return;
            };
            // The tray thread owns both `HICON`s and destroys them in its epilogue,
            // so a timeout must leak the thread rather than free icons it may still
            // be using.
            let exited = match done {
                Some(answer) => answer.recv_timeout(SHUTDOWN_TIMEOUT).is_ok(),
                None => true,
            };
            if exited {
                let _ = thread.join();
            } else {
                eprintln!(
                    "[tray] tray thread did not exit within {SHUTDOWN_TIMEOUT:?}; leaving it to process teardown"
                );
            }
        }

        /// Diagnostic snapshot. For the Win32 integration tests and acceptance
        /// scripts only.
        #[doc(hidden)]
        pub fn test_probe(&self) -> TrayProbe {
            let tray_hwnd = self.shared.tray_hwnd.load(Ordering::SeqCst);
            let icon_visible = self.shared.icon_added.load(Ordering::SeqCst);
            TrayProbe {
                hwnd: tray_hwnd,
                taskbar_created_msg: self.shared.taskbar_created_msg.load(Ordering::SeqCst),
                icon_rect: if icon_visible && tray_hwnd != 0 {
                    query_icon_rect(tray_hwnd, self.shared.guid)
                } else {
                    None
                },
                icon_visible,
                main_visible: self.main_visible(),
                alerting: self.shared.alerting.load(Ordering::SeqCst),
                balloons_sent: self.shared.balloons_sent.load(Ordering::SeqCst),
            }
        }

        /// The tooltip text the tray is trying to show (desired state, not a shell
        /// read-back).
        #[doc(hidden)]
        pub fn test_tooltip(&self) -> String {
            self.shared
                .tooltip
                .lock()
                .map(|tooltip| tooltip.clone())
                .unwrap_or_default()
        }

        fn main_window(&self) -> HWND {
            HWND(self.main_hwnd as *mut c_void)
        }

        fn main_window_valid(&self) -> bool {
            self.main_hwnd != 0 && unsafe { IsWindow(Some(self.main_window())).as_bool() }
        }

        fn request_command_drain(&self) {
            let tray_hwnd = self.shared.tray_hwnd.load(Ordering::SeqCst);
            if tray_hwnd != 0 {
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(tray_hwnd as *mut c_void)),
                        WM_TRAY_POKE,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        }
    }

    impl Drop for TrayHandle {
        fn drop(&mut self) {
            self.remove();
        }
    }

    /// The Win32 handle behind [`TrayBackend`].
    ///
    /// Every method delegates to the inherent API above; the trait exists so the
    /// lifecycle executor and its tests depend on the port, not on this type.
    impl TrayBackend for TrayHandle {
        fn hide_main(&self) -> bool {
            TrayHandle::hide_main(self)
        }

        fn show_main(&self) -> bool {
            TrayHandle::show_main(self)
        }

        fn set_visible(&self, visible: bool) -> bool {
            self.set_icon_visible(visible)
        }

        fn set_tooltip(&self, text: &str) {
            TrayHandle::set_tooltip(self, text);
        }

        fn notify_balloon(&self, notification: &TrayNotification) {
            TrayHandle::notify_balloon(self, notification);
        }

        fn start_flash(&self) {
            self.set_alert(true);
        }

        fn stop_flash(&self) {
            self.set_alert(false);
        }

        fn drain_events(&self) -> Vec<TrayEvent> {
            TrayHandle::drain_events(self)
        }

        fn remove(&self) {
            TrayHandle::remove(self);
        }
    }

    // ---------------------------------------------------------------------------
    // Single instance (design §2.3)
    // ---------------------------------------------------------------------------

    /// Process-level single-instance guard.
    ///
    /// The tray icon identity is derived from the executable path, so two copies
    /// of the same binary would fight over one notification-area icon. `main`
    /// acquires the guard before any window exists (except under `--self-test`);
    /// dropping the returned handle would let a second instance start while this
    /// one is hidden in the tray, so the caller keeps it for the process life.
    pub mod single_instance {
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::Win32::UI::WindowsAndMessaging::{
            MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
        };
        use windows::core::HSTRING;

        /// Keeps the named mutex alive.
        #[derive(Debug)]
        pub struct InstanceGuard {
            _handle: HANDLE,
        }

        /// The guard could not be acquired.
        ///
        /// Normally this means another instance of this exact executable owns
        /// the mutex; a failure to resolve the executable path is treated the
        /// same way, matching the caller's "do not open a second window"
        /// policy.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct AlreadyRunning;

        /// Acquires `Local\duthub-<64-bit hash of the lowercased exe path>`.
        ///
        /// `Err(AlreadyRunning)` means another instance of this exact
        /// executable is running.
        pub fn acquire() -> Result<InstanceGuard, AlreadyRunning> {
            use std::hash::{Hash, Hasher};

            let exe = std::env::current_exe().map_err(|_| AlreadyRunning)?;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            exe.to_string_lossy().to_lowercase().hash(&mut hasher);
            let name = HSTRING::from(format!("Local\\duthub-{:016x}", hasher.finish()));
            // SAFETY: `name` is a valid NUL-terminated wide string that outlives
            // the call, and no security attributes are requested.
            let handle = unsafe { CreateMutexW(None, false, &name) }.map_err(|_| AlreadyRunning)?;
            // SAFETY: reads the calling thread's last-error value, which
            // CreateMutexW just set.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                return Err(AlreadyRunning);
            }
            Ok(InstanceGuard { _handle: handle })
        }

        /// Tells the user why the second instance exits instead of opening a
        /// window.
        pub fn show_already_running(title: &str, body: &str) {
            let body = HSTRING::from(body);
            let title = HSTRING::from(title);
            // SAFETY: both strings are valid NUL-terminated wide strings that
            // outlive the call; no owner window is required.
            let _ = unsafe {
                MessageBoxW(
                    None,
                    &body,
                    &title,
                    MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
                )
            };
        }
    }

    // ---------------------------------------------------------------------------
    // System integration queries (A2 calls these, it never touches the windows crate)
    // ---------------------------------------------------------------------------

    /// Whether client-area animations are enabled (`SPI_GETCLIENTAREAANIMATION`).
    ///
    /// A failed query is treated as "enabled": the balloon is unaffected either way
    /// and a missing blink is less bad than silently dropping the alert motion for
    /// people who did not turn it off.
    pub fn animations_enabled() -> bool {
        let mut enabled = BOOL(1);
        let result = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some(&mut enabled as *mut BOOL as *mut c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        match result {
            Ok(()) => enabled.as_bool(),
            Err(_) => true,
        }
    }

    /// Whether the OS currently allows notification balloons
    /// (`SHQueryUserNotificationState`).
    ///
    /// Anything other than "accepts notifications" and "quiet time" — busy,
    /// presentation mode, full-screen D3D, locked/screen-saver, unknown — maps to
    /// [`NotificationGate::Blocked`], which suppresses the balloon and keeps the
    /// alert queued.
    pub fn notification_gate() -> NotificationGate {
        match unsafe { SHQueryUserNotificationState() } {
            Ok(state) if state == QUNS_ACCEPTS_NOTIFICATIONS => NotificationGate::Allowed,
            Ok(state) if state == QUNS_QUIET_TIME => NotificationGate::QuietTime,
            _ => NotificationGate::Blocked,
        }
    }

    // ---------------------------------------------------------------------------
    // Tray thread
    // ---------------------------------------------------------------------------

    fn tray_thread_main(
        shared: Arc<Shared>,
        commands: Receiver<TrayCommand>,
        ready: SyncSender<Result<(), TrayError>>,
        done: SyncSender<()>,
    ) {
        let setup = unsafe { tray_setup(&shared, commands) };
        match setup {
            Ok(ctx) => {
                let _ = ready.send(Ok(()));
                unsafe { tray_run(ctx) };
            }
            Err(err) => {
                let _ = ready.send(Err(err));
            }
        }
        let _ = done.send(());
    }

    unsafe fn tray_setup(
        shared: &Arc<Shared>,
        commands: Receiver<TrayCommand>,
    ) -> Result<Rc<TrayCtx>, TrayError> {
        unsafe {
            let hmodule = GetModuleHandleW(PCWSTR::null())
                .map_err(|err| TrayError::WindowCreate(format!("GetModuleHandleW: {err}")))?;
            let hinst = HINSTANCE(hmodule.0);
            let class_name = windows::core::w!("DuthubTrayHost");
            let wnd_class = WNDCLASSW {
                style: WNDCLASS_STYLES(0),
                lpfnWndProc: Some(tray_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: HICON::default(),
                hCursor: HCURSOR::default(),
                hbrBackground: HBRUSH::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class_name,
            };
            // A zero atom usually means the class already exists (a second install
            // in this process); CreateWindowExW below reports a real failure.
            if RegisterClassW(&wnd_class) == 0 {
                eprintln!(
                    "[tray] RegisterClassW returned 0; assuming the class is already registered"
                );
            }

            // Top-level (not HWND_MESSAGE) so broadcasts such as TaskbarCreated
            // reach it; tool window + popup keeps it out of the taskbar.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name,
                windows::core::w!("duthub tray host"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                None,
            )
            .map_err(|err| TrayError::WindowCreate(format!("CreateWindowExW: {err}")))?;

            // `TaskbarCreated` must be registered before the first NIM_ADD, which
            // happens later on this same thread.
            let taskbar_created = RegisterWindowMessageW(windows::core::w!("TaskbarCreated"));
            if taskbar_created == 0 {
                eprintln!(
                    "[tray] RegisterWindowMessageW(TaskbarCreated) failed; icon loss cannot be recovered"
                );
            }

            let base_icon = load_icon(hinst, RESOURCE_APP_ICON).ok_or_else(|| {
                TrayError::IconAdd(format!("resource {RESOURCE_APP_ICON} failed to load"))
            })?;
            let alert_icon = load_alert_icon(hinst);

            shared.tray_hwnd.store(hwnd.0 as isize, Ordering::SeqCst);
            shared
                .tray_tid
                .store(GetCurrentThreadId(), Ordering::SeqCst);
            shared
                .taskbar_created_msg
                .store(taskbar_created, Ordering::SeqCst);

            let ctx = Rc::new(TrayCtx {
                shared: Arc::clone(shared),
                base_icon,
                alert_icon,
                flash_started: Mutex::new(None),
                flash_lit: AtomicBool::new(false),
                commands: Mutex::new(Some(commands)),
                menu_active: AtomicBool::new(false),
            });
            TRAY_CTX.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&ctx)));
            Ok(ctx)
        }
    }

    unsafe fn load_icon(hinst: HINSTANCE, resource_id: u16) -> Option<HICON> {
        // MAKEINTRESOURCEW: the low word carries the resource id, no string table.
        let name = PCWSTR(resource_id as usize as *const u16);
        match unsafe { LoadIconMetric(Some(hinst), name, LIM_SMALL) } {
            Ok(icon) => Some(icon),
            Err(err) => {
                eprintln!("[tray] LoadIconMetric({resource_id}) failed: {err}");
                None
            }
        }
    }

    /// Loads the blinking frame.
    ///
    /// The dedicated `assets/tray-alert.ico` resource (id 2) wins when it exists.
    /// Otherwise a private copy of the system warning icon is used so the blink
    /// still reads as a warning without ever painting the normal app icon as the
    /// "alert" frame. `None` means "no blink, balloon only".
    unsafe fn load_alert_icon(hinst: HINSTANCE) -> Option<HICON> {
        if let Some(icon) = unsafe { load_icon(hinst, RESOURCE_ALERT_ICON) } {
            return Some(icon);
        }
        let shared = unsafe { LoadIconW(None, IDI_WARNING) };
        match shared.map(|icon| unsafe { CopyIcon(icon) }) {
            Ok(Ok(copy)) => Some(copy),
            _ => {
                eprintln!("[tray] no alert icon available; the alert will use the balloon only");
                None
            }
        }
    }

    unsafe fn tray_run(ctx: Rc<TrayCtx>) {
        let mut msg = MSG::default();
        loop {
            let received = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if received.0 == 0 || received.0 == -1 {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Detach the context before touching the window: DestroyWindow below sends
        // messages that must not reach a WndProc whose icons are being freed.
        let taken = TRAY_CTX.with(|slot| slot.borrow_mut().take());
        let hwnd = HWND(ctx.shared.tray_hwnd.load(Ordering::SeqCst) as *mut c_void);
        unsafe {
            let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID);
            if ctx.shared.icon_added.load(Ordering::SeqCst) {
                let nid = ctx.nid_with_flags(NIF_GUID);
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                ctx.shared.icon_added.store(false, Ordering::SeqCst);
            }
            let _ = DestroyWindow(hwnd);
            // Only the tray thread owns these HICONs; freeing them here (rather
            // than after a join from the UI thread) means a wedged thread can never
            // free an icon it is still painting with.
            let _ = DestroyIcon(ctx.base_icon);
            if let Some(alert_icon) = ctx.alert_icon {
                let _ = DestroyIcon(alert_icon);
            }
        }
        drop(taken);
    }

    unsafe extern "system" fn tray_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let ctx = TRAY_CTX.with(|slot| slot.borrow().clone());
        if let Some(ctx) = ctx
            && let Some(result) = ctx.handle_message(hwnd, msg, wparam, lparam)
        {
            return result;
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    impl TrayCtx {
        fn handle_message(
            &self,
            _hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> Option<LRESULT> {
            let taskbar_created = self.shared.taskbar_created_msg.load(Ordering::SeqCst);
            if taskbar_created != 0 && msg == taskbar_created {
                self.replay_icon();
                return Some(LRESULT(0));
            }
            match msg {
                WM_TRAY_CALLBACK => {
                    // NOTIFYICON_VERSION_4: low word of lParam = event, high word =
                    // icon id; wParam carries the anchor point for that event.
                    self.on_icon_callback(wparam, lparam.0 as u32);
                    Some(LRESULT(0))
                }
                WM_TIMER if wparam.0 == FLASH_TIMER_ID => {
                    self.on_flash_timer();
                    Some(LRESULT(0))
                }
                WM_TRAY_POKE => {
                    self.run_commands();
                    self.realign();
                    Some(LRESULT(0))
                }
                WM_SETTINGCHANGE => {
                    // SPI_SETCLIENTAREAANIMATION arrives as this broadcast.
                    self.on_settings_changed();
                    Some(LRESULT(0))
                }
                WM_DESTROY => {
                    unsafe { PostQuitMessage(0) };
                    Some(LRESULT(0))
                }
                _ => None,
            }
        }

        /// Stops an in-flight blink when the user turns client-area animations
        /// off mid-episode. The balloon channel is unaffected.
        fn on_settings_changed(&self) {
            let flash_active = self
                .flash_started
                .lock()
                .map(|started| started.is_some())
                .unwrap_or(false);
            if tray_ui::flash_stops_for_animation_change(flash_active, animations_enabled()) {
                self.stop_flash();
            }
        }

        fn tray_hwnd(&self) -> Option<HWND> {
            let raw = self.shared.tray_hwnd.load(Ordering::SeqCst);
            if raw == 0 {
                None
            } else {
                Some(HWND(raw as *mut c_void))
            }
        }

        /// Current popup-menu labels `(show, quit)`.
        fn menu_labels(&self) -> (String, String) {
            self.shared
                .menu_labels
                .lock()
                .map(|labels| labels.clone())
                .unwrap_or_default()
        }

        /// Bottom-left corner of the hosted icon, used to place the menu when the
        /// callback carries no anchor (keyboard-invoked popup).
        fn icon_anchor(&self) -> Option<(i32, i32)> {
            let hwnd = self.tray_hwnd()?;
            query_icon_rect(hwnd.0 as isize, self.shared.guid).map(|rect| (rect[0], rect[3]))
        }

        fn nid_with_flags(
            &self,
            flags: windows::Win32::UI::Shell::NOTIFY_ICON_DATA_FLAGS,
        ) -> NOTIFYICONDATAW {
            // GUID-identified icons always use uID 0.
            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.tray_hwnd().unwrap_or_default(),
                uID: 0,
                uFlags: flags,
                uCallbackMessage: WM_TRAY_CALLBACK,
                hIcon: self.base_icon,
                guidItem: self.shared.guid,
                ..Default::default()
            };
            if let Ok(tooltip) = self.shared.tooltip.lock() {
                copy_utf16(&mut nid.szTip, &tooltip, TIP_MAX_UNITS);
            }
            nid
        }

        fn on_icon_callback(&self, wparam: WPARAM, lparam: u32) {
            let event = lparam & 0xffff;
            match event {
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP => self.push_event(TrayEvent::Show),
                WM_CONTEXTMENU | WM_RBUTTONUP => {
                    // Use the anchor the shell sent: for a keyboard-invoked menu
                    // it is the icon's corner, while the mouse may be anywhere.
                    let anchor = tray_ui::callback_anchor(event, wparam.0);
                    self.show_menu(anchor);
                }
                NIN_BALLOONUSERCLICK => {
                    self.stop_flash();
                    self.push_event(TrayEvent::Show);
                }
                NIN_BALLOONTIMEOUT | NIN_BALLOONHIDE => self.stop_flash(),
                _ => {}
            }
        }

        fn show_menu(&self, anchor: Option<(i32, i32)>) {
            if self.menu_active.swap(true, Ordering::SeqCst) {
                return;
            }
            let Some(hwnd) = self.tray_hwnd() else {
                self.menu_active.store(false, Ordering::SeqCst);
                return;
            };
            // Clone the labels and drop the guard: the popup below runs a modal
            // loop that re-enters this WndProc.
            let (show_label, quit_label) = self.menu_labels();
            let Some(menu) = build_tray_menu(&show_label, &quit_label) else {
                self.menu_active.store(false, Ordering::SeqCst);
                return;
            };
            let (x, y) = anchor
                .or_else(|| self.icon_anchor())
                .unwrap_or_else(cursor_position);
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
            let command = unsafe {
                TrackPopupMenuEx(
                    menu,
                    (TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD).0,
                    x,
                    y,
                    hwnd,
                    None,
                )
            };
            unsafe {
                // Required so the menu closes when the user clicks elsewhere.
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                let _ = DestroyMenu(menu);
            }
            let command = command.0 as usize;
            if command == 0 {
                // Dismissed: hand focus back to the notification area.
                let nid = self.nid_with_flags(NIF_GUID);
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_SETFOCUS, &nid);
                }
            } else if let Some(event) = menu_command_event(command) {
                self.push_event(event);
            }
            self.menu_active.store(false, Ordering::SeqCst);
        }

        fn run_commands(&self) {
            let mut pending = Vec::new();
            let mut disconnected = false;
            if let Ok(guard) = self.commands.lock()
                && let Some(receiver) = guard.as_ref()
            {
                loop {
                    match receiver.try_recv() {
                        Ok(command) => pending.push(command),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
            }
            for command in pending {
                self.apply_command(command);
            }
            if disconnected {
                // The UI side dropped its handle without removing the tray; end the
                // loop instead of spinning on a dead channel.
                unsafe { PostQuitMessage(0) };
            }
        }

        fn apply_command(&self, command: TrayCommand) {
            match command {
                TrayCommand::SetIconVisible { visible, reply } => {
                    let _ = reply.send(self.apply_icon_visible(visible));
                }
                TrayCommand::SetTooltip { text } => {
                    if let Ok(mut tooltip) = self.shared.tooltip.lock() {
                        *tooltip = text;
                    }
                    self.apply_tooltip();
                }
                TrayCommand::SetAlert { alerting } => self.apply_alert(alerting),
                TrayCommand::Notify { notification } => self.apply_notify(&notification),
                TrayCommand::Remove { reply } => {
                    let _ = self.apply_icon_visible(false);
                    let _ = reply.send(());
                }
            }
        }

        fn apply_icon_visible(&self, visible: bool) -> Result<(), String> {
            if visible == self.shared.icon_added.load(Ordering::SeqCst) {
                return Ok(());
            }
            if visible {
                if !self.try_add_icon() {
                    return Err("Shell_NotifyIconW(NIM_ADD) failed".to_string());
                }
                self.shared.icon_added.store(true, Ordering::SeqCst);
                if self.shared.alerting.load(Ordering::SeqCst) {
                    self.sync_flash_timer();
                }
                Ok(())
            } else {
                self.stop_flash();
                let deleted = self.delete_icon();
                // Even if the delete failed (the shell may have dropped the icon
                // already), the desired state is "no icon": keeping it at "added"
                // would let a later hide skip the add and strand the window.
                self.shared.icon_added.store(false, Ordering::SeqCst);
                if deleted {
                    Ok(())
                } else {
                    Err("Shell_NotifyIconW(NIM_DELETE) failed".to_string())
                }
            }
        }

        /// Adds the icon and applies `NOTIFYICON_VERSION_4` (the version setting
        /// does not survive a logon or an explorer restart, so it is always paired
        /// with the add).
        fn try_add_icon(&self) -> bool {
            let nid =
                self.nid_with_flags(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID | NIF_SHOWTIP);
            if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool() {
                return false;
            }
            let mut version = self.nid_with_flags(NIF_GUID);
            version.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            unsafe {
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &version);
            }
            true
        }

        fn delete_icon(&self) -> bool {
            let nid = self.nid_with_flags(NIF_GUID);
            unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) }.as_bool()
        }

        fn apply_alert(&self, alerting: bool) {
            self.shared.alerting.store(alerting, Ordering::SeqCst);
            if alerting {
                self.sync_flash_timer();
            } else {
                self.stop_flash();
            }
        }

        fn sync_flash_timer(&self) {
            let Some(hwnd) = self.tray_hwnd() else {
                return;
            };
            if !self.shared.icon_added.load(Ordering::SeqCst) {
                return;
            }
            // No alert icon at all (neither the resource nor a copy of the system
            // warning icon): degrade to balloon only instead of painting the normal
            // app icon as if it were an alert frame.
            if self.alert_icon.is_none() {
                return;
            }
            // A user who turned client-area animations off must not get a blinking
            // icon; the balloon still carries the alert. The UI already filters
            // this via `tray_ui::alert_presentation`, so this is a second guard for
            // any other caller.
            if !animations_enabled() {
                return;
            }
            let running = self
                .flash_started
                .lock()
                .map(|started| started.is_some())
                .unwrap_or(false);
            if running {
                return;
            }
            if let Ok(mut started) = self.flash_started.lock() {
                *started = Some(Instant::now());
            }
            self.flash_lit.store(false, Ordering::SeqCst);
            unsafe {
                SetTimer(Some(hwnd), FLASH_TIMER_ID, FLASH_INTERVAL_MS as u32, None);
            }
        }

        fn stop_flash(&self) {
            self.shared.alerting.store(false, Ordering::SeqCst);
            if let Some(hwnd) = self.tray_hwnd() {
                unsafe {
                    let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID);
                }
            }
            if let Ok(mut started) = self.flash_started.lock() {
                *started = None;
            }
            self.flash_lit.store(false, Ordering::SeqCst);
            if self.shared.icon_added.load(Ordering::SeqCst) {
                self.modify_icon(self.base_icon);
            }
        }

        fn on_flash_timer(&self) {
            let started = self.flash_started.lock().ok().and_then(|started| *started);
            let Some(started) = started else {
                if let Some(hwnd) = self.tray_hwnd() {
                    unsafe {
                        let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID);
                    }
                }
                return;
            };
            if started.elapsed().as_millis() as u64 >= FLASH_MAX_MS {
                self.stop_flash();
                return;
            }
            let lit = !self.flash_lit.load(Ordering::SeqCst);
            self.flash_lit.store(lit, Ordering::SeqCst);
            let icon = match (lit, self.alert_icon) {
                (true, Some(alert_icon)) => alert_icon,
                _ => self.base_icon,
            };
            self.modify_icon(icon);
        }

        fn modify_icon(&self, icon: HICON) {
            if !self.shared.icon_added.load(Ordering::SeqCst) {
                return;
            }
            let mut nid = self.nid_with_flags(NIF_ICON | NIF_GUID);
            nid.hIcon = icon;
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            }
        }

        fn apply_tooltip(&self) {
            if !self.shared.icon_added.load(Ordering::SeqCst) {
                return;
            }
            let nid = self.nid_with_flags(NIF_TIP | NIF_SHOWTIP | NIF_GUID);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            }
        }

        fn apply_notify(&self, notification: &TrayNotification) {
            if !self.shared.icon_added.load(Ordering::SeqCst) {
                return;
            }
            let mut nid = self.nid_with_flags(NIF_INFO | NIF_GUID);
            // Omit legacy Win32 warning/error triangles so Windows 11 renders
            // a clean modern toast with the app's own brand icon in the header.
            let mut flags = NIIF_NONE;
            if notification.respect_quiet_time {
                flags |= NIIF_RESPECT_QUIET_TIME;
            }
            nid.dwInfoFlags = flags;
            copy_utf16(&mut nid.szInfoTitle, &notification.title, TITLE_MAX_UNITS);
            copy_utf16(&mut nid.szInfo, &notification.body, BODY_MAX_UNITS);
            if unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool() {
                self.shared.balloons_sent.fetch_add(1, Ordering::SeqCst);
            }
        }

        fn replay_icon(&self) {
            if !self.shared.icon_added.load(Ordering::SeqCst) {
                return;
            }
            // After a real explorer restart the icon is gone and NIM_ADD succeeds.
            // A stale registration for the same GUID (a spurious broadcast) makes
            // NIM_ADD fail, so drop the entry and retry once before giving up.
            let added = self.try_add_icon() || {
                let _ = self.delete_icon();
                self.try_add_icon()
            };
            if !added {
                // Without the icon a hidden window could never be restored, so stop
                // pretending: clear the state and let the UI bring the window back.
                self.shared.icon_added.store(false, Ordering::SeqCst);
                eprintln!("[tray] icon replay after TaskbarCreated failed; reporting IconLost");
                self.push_event(TrayEvent::IconLost);
                return;
            }
            self.apply_tooltip();
            if self.shared.alerting.load(Ordering::SeqCst) {
                self.sync_flash_timer();
            } else {
                self.modify_icon(self.base_icon);
            }
        }

        fn realign(&self) {
            if !self.shared.icon_added.load(Ordering::SeqCst) {
                return;
            }
            self.apply_tooltip();
            if self.shared.alerting.load(Ordering::SeqCst) {
                self.sync_flash_timer();
            } else {
                self.modify_icon(self.base_icon);
            }
        }

        fn push_event(&self, event: TrayEvent) {
            if let Ok(mut queue) = self.shared.events.lock() {
                if queue.len() >= MAX_EVENTS {
                    queue.pop_front();
                }
                queue.push_back(event);
            }
        }
    }

    fn build_tray_menu(show_label: &str, quit_label: &str) -> Option<HMENU> {
        let menu = unsafe { CreatePopupMenu() }.ok()?;
        let show: Vec<u16> = show_label
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let quit: Vec<u16> = quit_label
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let built = unsafe {
            AppendMenuW(menu, MF_STRING, MENU_SHOW_ID, PCWSTR(show.as_ptr())).is_ok()
                && AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()).is_ok()
                && AppendMenuW(menu, MF_STRING, MENU_QUIT_ID, PCWSTR(quit.as_ptr())).is_ok()
        };
        if !built {
            unsafe {
                let _ = DestroyMenu(menu);
            }
            return None;
        }
        Some(menu)
    }

    /// Copies `text` into a NUL-terminated UTF-16 buffer, truncating to at most
    /// `max_units` code units without ever splitting a surrogate pair.
    fn copy_utf16(dst: &mut [u16], text: &str, max_units: usize) {
        let limit = max_units.min(dst.len().saturating_sub(1));
        let mut written = 0;
        for ch in text.chars() {
            let mut scratch = [0u16; 2];
            let units = ch.encode_utf16(&mut scratch);
            if written + units.len() > limit {
                break;
            }
            dst[written..written + units.len()].copy_from_slice(units);
            written += units.len();
        }
        if written < dst.len() {
            dst[written] = 0;
        }
    }

    /// Last-resort popup position when neither the callback nor the icon
    /// rectangle gave one. A failed query leaves `(0, 0)`; the menu still opens
    /// and the user can move it, so this is not worth aborting.
    fn cursor_position() -> (i32, i32) {
        let mut cursor = POINT::default();
        let _ = unsafe { GetCursorPos(&mut cursor) };
        (cursor.x, cursor.y)
    }

    fn query_icon_rect(tray_hwnd: isize, guid: GUID) -> Option<[i32; 4]> {
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: HWND(tray_hwnd as *mut c_void),
            uID: 0,
            guidItem: guid,
        };
        match unsafe { Shell_NotifyIconGetRect(&identifier) } {
            Ok(rect) => Some([rect.left, rect.top, rect.right, rect.bottom]),
            Err(_) => None,
        }
    }

    // ---------------------------------------------------------------------------
    // Menu command mapping (shared with tests; no modal loop required)
    // ---------------------------------------------------------------------------

    /// Popup menu command id for "show main window".
    #[doc(hidden)]
    pub const MENU_SHOW_ID: usize = 1;
    /// Popup menu command id for "quit".
    #[doc(hidden)]
    pub const MENU_QUIT_ID: usize = 2;

    /// Maps a `TrackPopupMenuEx(TPM_RETURNCMD)` command id to a tray event.
    ///
    /// `0` (dismissed) maps to `None`; the caller then returns focus to the
    /// notification area instead of acting.
    #[doc(hidden)]
    pub fn menu_command_event(command: usize) -> Option<TrayEvent> {
        match command {
            MENU_SHOW_ID => Some(TrayEvent::Show),
            MENU_QUIT_ID => Some(TrayEvent::Quit),
            _ => None,
        }
    }

    /// Builds the real popup menu and reports the command ids it contains.
    ///
    /// Used by `tests/tray_win32.rs` to verify the menu wiring without opening a
    /// modal menu (which needs a human). Returns `None` if the menu could not be
    /// created.
    #[doc(hidden)]
    pub fn test_menu_layout(options: &TrayOptions) -> Option<Vec<usize>> {
        let menu = build_tray_menu(&options.show_label, &options.quit_label)?;
        let mut ids = Vec::new();
        unsafe {
            let count = GetMenuItemCount(Some(menu));
            for index in 0..count {
                ids.push(GetMenuItemID(menu, index) as usize);
            }
            let _ = DestroyMenu(menu);
        }
        Some(ids)
    }
}

// ---------------------------------------------------------------------------
// Non-Windows no-op implementation
// ---------------------------------------------------------------------------

/// Inert tray port for platforms without a notification area implementation.
///
/// Everything reports "unavailable" and [`install`] fails, which makes
/// `tray_ui::close_disposition` choose a real exit. Nothing is ever
/// half-installed, so the "never hidden without a way back" invariant cannot
/// be violated on these targets.
#[cfg(not(windows))]
mod platform {
    use super::*;

    /// Inert handle. It owns no resources and every operation is a no-op.
    #[derive(Debug, Default)]
    pub struct TrayHandle;

    impl TrayHandle {
        /// Always `false`: there is no notification area to hide into.
        pub fn hide_main(&self) -> bool {
            false
        }

        /// Always `false`: there is no tray-driven window to show.
        pub fn show_main(&self) -> bool {
            false
        }

        /// Always `false`: the main window is not managed by this port.
        pub fn main_visible(&self) -> bool {
            false
        }

        /// Always `false`: no icon can be added.
        pub fn set_icon_visible(&self, _visible: bool) -> bool {
            false
        }

        pub fn set_tooltip(&self, _text: &str) {}

        /// No popup menu exists on this platform; accepted for API parity.
        pub fn set_menu_labels(&self, _show_label: &str, _quit_label: &str) {}

        /// No popup menu exists on this platform.
        #[doc(hidden)]
        pub fn test_menu_labels(&self) -> (String, String) {
            (String::new(), String::new())
        }

        pub fn set_alert(&self, _alerting: bool) {}

        pub fn notify(&self, _title: &str, _body: &str, _level: TrayNotifyLevel) {}

        pub fn notify_balloon(&self, _notification: &TrayNotification) {}

        pub fn main_minimized(&self) -> bool {
            false
        }

        pub fn flash_taskbar(&self) {}

        pub fn stop_taskbar_flash(&self) {}

        pub fn drain_events(&self) -> Vec<TrayEvent> {
            Vec::new()
        }

        pub fn remove(&self) {}

        /// Diagnostic snapshot: nothing is installed and nothing is visible.
        #[doc(hidden)]
        pub fn test_probe(&self) -> TrayProbe {
            TrayProbe {
                hwnd: 0,
                taskbar_created_msg: 0,
                icon_rect: None,
                icon_visible: false,
                main_visible: false,
                alerting: false,
                balloons_sent: 0,
            }
        }

        /// Diagnostic snapshot: no tooltip is ever set.
        #[doc(hidden)]
        pub fn test_tooltip(&self) -> String {
            String::new()
        }
    }

    impl TrayBackend for TrayHandle {
        fn hide_main(&self) -> bool {
            TrayHandle::hide_main(self)
        }

        fn show_main(&self) -> bool {
            TrayHandle::show_main(self)
        }

        fn set_visible(&self, _visible: bool) -> bool {
            false
        }

        fn set_tooltip(&self, _text: &str) {}

        fn notify_balloon(&self, _notification: &TrayNotification) {}

        fn start_flash(&self) {}

        fn stop_flash(&self) {}

        fn drain_events(&self) -> Vec<TrayEvent> {
            Vec::new()
        }

        fn remove(&self) {}
    }

    /// There is no tray on this platform: report a real installation failure so
    /// the caller latches `tray_failed` and close falls back to quitting.
    pub fn install(
        _window: &mut gpui_kit::Window,
        _options: TrayOptions,
    ) -> Result<TrayHandle, TrayError> {
        Err(TrayError::WindowCreate(
            "the notification area is only implemented on Windows".to_string(),
        ))
    }

    /// Without a tray there is no blink; report animations as allowed so the
    /// pure presentation matrix still evaluates deterministically.
    pub fn animations_enabled() -> bool {
        true
    }

    /// No shell to ask: treat notifications as blocked (the alert stays
    /// queued; there is no hidden state to present it in anyway).
    pub fn notification_gate() -> crate::tray_ui::NotificationGate {
        crate::tray_ui::NotificationGate::Blocked
    }

    /// Single-instance guard: nothing to fight over without a tray identity,
    /// so the guard always succeeds.
    pub mod single_instance {
        #[derive(Debug, Default)]
        pub struct InstanceGuard;

        /// Never produced on this platform; present so the port has the same
        /// shape everywhere.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct AlreadyRunning;

        pub fn acquire() -> Result<InstanceGuard, AlreadyRunning> {
            Ok(InstanceGuard)
        }

        pub fn show_already_running(_title: &str, _body: &str) {}
    }
}
