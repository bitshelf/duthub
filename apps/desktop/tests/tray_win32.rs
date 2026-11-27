//! Real Win32 integration for the notification-area host
//! (`docs/design/TRAY_ALERT_DESIGN.md` §10.2).
//!
//! A plain Win32 parent window stands in for the GPUI window, so these tests
//! need no GPU and no gpui app loop. Assertions that need a live notification
//! area (`explorer.exe`) are `#[ignore]`d and run explicitly during acceptance:
//!
//! ```text
//! cargo test -p duthub-desktop --test tray_win32 -- --ignored --nocapture
//! ```
//!
//! The remaining tests hold *with and without* a shell, and are built around the
//! safety invariant that matters most here: the main window is never hidden
//! unless the shell really accepted the icon.

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use duthub_desktop::tray::{
    self, MENU_QUIT_ID, MENU_SHOW_ID, TrayEvent, TrayNotifyLevel, TrayOptions, menu_command_event,
    test_menu_layout,
};
use duthub_desktop::tray_ui::NotificationGate;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HCURSOR, HICON, MSG,
    PM_REMOVE, PeekMessageW, PostMessageW, RegisterClassW, SW_SHOW, ShowWindow, TranslateMessage,
    WM_DESTROY, WNDCLASS_STYLES, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
};
use windows::core::PCWSTR;

/// The notification icon is identified by a GUID derived from the executable
/// path, so every tray in this test binary shares one identity. Tests therefore
/// run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn options() -> TrayOptions {
    TrayOptions {
        tooltip: "duthub tray test".to_string(),
        show_label: "Show main window".to_string(),
        quit_label: "Quit".to_string(),
    }
}

/// Minimal top-level window that plays the role of the main window.
struct TestWindow {
    hwnd: HWND,
}

impl TestWindow {
    fn create() -> Self {
        // SAFETY: Registers a test window class with static UTF-16 names and a
        // matching `system` ABI wndproc, creates an owned top-level window on the
        // current thread, and shows it.
        let window = unsafe {
            let module = GetModuleHandleW(PCWSTR::null()).expect("GetModuleHandleW");
            let hinst = windows::Win32::Foundation::HINSTANCE(module.0);
            let class_name = windows::core::w!("DuthubTrayTestHost");
            let wnd_class = WNDCLASSW {
                style: WNDCLASS_STYLES(0),
                lpfnWndProc: Some(test_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: HICON::default(),
                hCursor: HCURSOR::default(),
                hbrBackground: HBRUSH::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class_name,
            };
            RegisterClassW(&raw const wnd_class);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name,
                windows::core::w!("duthub tray test host"),
                WS_OVERLAPPEDWINDOW,
                120,
                120,
                320,
                200,
                None,
                None,
                Some(hinst),
                None,
            )
            .expect("CreateWindowExW");
            let _ = ShowWindow(hwnd, SW_SHOW);
            Self { hwnd }
        };
        // Let the show take effect before the caller samples `main_visible`.
        TestWindow::pump(Duration::from_millis(50));
        window
    }

    fn raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    fn visible(&self) -> bool {
        // SAFETY: `self.hwnd` is owned by `TestWindow` and live for `&self`.
        unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(self.hwnd).as_bool() }
    }

    /// Drains the window message queue; `ShowWindowAsync` from the tray thread
    /// only takes effect once this thread pumps.
    fn pump(duration: Duration) {
        let deadline = Instant::now() + duration;
        loop {
            let mut msg = MSG::default();
            // SAFETY: Pumps queued Win32 messages for windows owned by the current thread.
            unsafe {
                while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            Self::pump(Duration::from_millis(25));
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        // SAFETY: `self.hwnd` is exclusively owned by `TestWindow` and destroyed once.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Test window procedure for the hidden tray-test host window.
///
/// # Safety
///
/// Invoked only by the Windows message dispatcher with valid Win32 window
/// procedure arguments.
unsafe extern "system" fn test_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    match msg {
        WM_DESTROY => windows::Win32::Foundation::LRESULT(0),
        // SAFETY: The window manager supplied the handle and message parameters.
        // Forward them unchanged to the default procedure on this thread.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ---------------------------------------------------------------------------
// Tests that need no desktop shell
// ---------------------------------------------------------------------------

#[test]
fn menu_items_map_to_show_and_quit() {
    let layout = test_menu_layout(&options()).expect("popup menu");
    assert_eq!(layout.len(), 3, "show / separator / quit");
    assert_eq!(layout[0], MENU_SHOW_ID);
    assert_eq!(layout[2], MENU_QUIT_ID);
    assert!(
        layout[1] != MENU_SHOW_ID && layout[1] != MENU_QUIT_ID,
        "the middle entry is a separator, not a command"
    );

    assert_eq!(menu_command_event(MENU_SHOW_ID), Some(TrayEvent::Show));
    assert_eq!(menu_command_event(MENU_QUIT_ID), Some(TrayEvent::Quit));
    assert_eq!(
        menu_command_event(0),
        None,
        "dismissed menu maps to nothing"
    );
    assert_eq!(menu_command_event(99), None);
}

#[test]
fn system_notification_gates_are_queryable() {
    // Both queries must work from any thread, without a window and without
    // panicking; A2 calls them from the 50 ms tick.
    let first = tray::animations_enabled();
    let second = tray::animations_enabled();
    assert_eq!(first, second, "the animation preference must be stable");
    assert!(matches!(
        tray::notification_gate(),
        NotificationGate::Allowed | NotificationGate::QuietTime | NotificationGate::Blocked
    ));
    assert!(
        !tray::self_test_requested(),
        "the test binary is not a --self-test run"
    );
}

#[test]
fn menu_labels_follow_a_language_switch() {
    let _guard = serial();
    let window = TestWindow::create();
    let handle = tray::install_with_hwnd(window.raw(), options()).expect("tray install");

    assert_eq!(
        handle.test_menu_labels(),
        ("Show main window".to_string(), "Quit".to_string()),
        "install seeds the menu from TrayOptions"
    );
    // The labels are read when the popup opens, so switching the UI language
    // only has to rewrite them.
    handle.set_menu_labels("显示主窗口", "退出");
    assert_eq!(
        handle.test_menu_labels(),
        ("显示主窗口".to_string(), "退出".to_string())
    );
    let layout = test_menu_layout(&TrayOptions {
        tooltip: String::new(),
        show_label: "显示主窗口".to_string(),
        quit_label: "退出".to_string(),
    })
    .expect("popup menu");
    assert_eq!(layout.first().copied(), Some(MENU_SHOW_ID));
    assert_eq!(layout.last().copied(), Some(MENU_QUIT_ID));
    handle.remove();
}

#[test]
fn commands_before_the_icon_exists_are_harmless() {
    let _guard = serial();
    let window = TestWindow::create();
    let handle = tray::install_with_hwnd(window.raw(), options()).expect("tray install");

    let probe = handle.test_probe();
    assert_ne!(probe.hwnd, 0, "the tray host window exists");
    assert_ne!(
        probe.taskbar_created_msg, 0,
        "TaskbarCreated must be registered before the first icon add"
    );
    assert!(!probe.icon_visible, "install must not add an icon");
    assert_eq!(probe.balloons_sent, 0);

    handle.set_tooltip("updated tooltip");
    assert_eq!(handle.test_tooltip(), "updated tooltip");

    // No icon yet: these must be inert instead of touching the shell.
    handle.set_alert(true);
    handle.notify("title", "body", TrayNotifyLevel::Warning);
    assert_eq!(handle.test_probe().balloons_sent, 0);
    assert_eq!(
        handle.drain_events(),
        [] as [duthub_desktop::tray::TrayEvent; 0]
    );

    assert!(
        handle.set_icon_visible(false),
        "removing an icon that was never added is a no-op success"
    );
    handle.set_alert(false);
    handle.remove();
}

#[test]
fn hide_main_never_strands_the_window() {
    let _guard = serial();
    let window = TestWindow::create();
    let handle = tray::install_with_hwnd(window.raw(), options()).expect("tray install");
    let visible_before = handle.main_visible();

    let hidden = handle.hide_main();
    if hidden {
        // A real shell accepted the icon: the window must actually go away.
        assert!(
            TestWindow::wait_until(Duration::from_millis(3000), || !handle.main_visible()),
            "hide_main returned true but the window stayed visible"
        );
        assert!(
            handle.test_probe().icon_visible,
            "a hidden window must keep its icon"
        );
    } else {
        // No shell (CI / session 0): the icon was refused, so nothing may have
        // been hidden -- otherwise the window would be unreachable.
        assert_eq!(
            handle.main_visible(),
            visible_before,
            "hide_main must leave the window state untouched when the icon cannot be added"
        );
        assert!(!handle.test_probe().icon_visible);
    }

    assert!(handle.show_main());
    if visible_before {
        assert!(
            TestWindow::wait_until(Duration::from_millis(3000), || handle.main_visible()),
            "show_main must bring the window back"
        );
    }
    // Ordering used by the desktop layer: show first, then drop the icon.
    let _ = handle.set_icon_visible(false);
    assert!(!handle.test_probe().icon_visible);
    assert!(handle.test_probe().icon_rect.is_none());

    handle.remove();
    handle.remove(); // idempotent
    assert!(!handle.test_probe().icon_visible);
    // Dropping the handle also calls remove(): must not panic or hang.
}

// ---------------------------------------------------------------------------
// Tests that need an interactive desktop shell
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires an interactive desktop shell (explorer.exe); run explicitly for acceptance"]
fn icon_lifecycle_against_a_real_shell() {
    let _guard = serial();
    let window = TestWindow::create();
    let handle = tray::install_with_hwnd(window.raw(), options()).expect("tray install");

    assert!(handle.hide_main(), "a desktop session must accept NIM_ADD");
    assert!(
        TestWindow::wait_until(Duration::from_millis(3000), || !handle.main_visible()),
        "the hidden window must not stay visible"
    );
    let probe = handle.test_probe();
    assert!(probe.icon_visible);
    match probe.icon_rect {
        Some(rect) => println!("tray icon hosted at {rect:?}"),
        None => panic!("icon is not hosted anywhere; an icon in the overflow area still counts"),
    }

    // Tooltip and balloon.
    handle.set_tooltip("duthub tray test");
    handle.notify("duthub test", "body text", TrayNotifyLevel::Warning);
    TestWindow::pump(Duration::from_millis(400));
    assert_eq!(
        handle.test_probe().balloons_sent,
        1,
        "the shell must accept the balloon"
    );

    // Blink on then off restores the normal icon.
    handle.set_alert(true);
    TestWindow::pump(Duration::from_millis(1200));
    assert!(handle.test_probe().alerting);
    handle.set_alert(false);
    TestWindow::pump(Duration::from_millis(200));
    assert!(!handle.test_probe().alerting);

    // A TaskbarCreated replay must either keep the icon reachable or report
    // IconLost so the UI can bring the window back.
    let taskbar_created = handle.test_probe().taskbar_created_msg;
    // SAFETY: `handle` is still alive, so its tray host HWND is valid; `PostMessageW`
    // is thread-safe and simulates a `TaskbarCreated` broadcast.
    unsafe {
        let _ = PostMessageW(
            Some(HWND(handle.test_probe().hwnd as *mut c_void)),
            taskbar_created,
            WPARAM(0),
            LPARAM(0),
        );
    }
    TestWindow::pump(Duration::from_millis(500));
    let lost = handle.drain_events().contains(&TrayEvent::IconLost);
    assert!(
        handle.test_probe().icon_rect.is_some() || lost,
        "after TaskbarCreated the icon must be reachable again or IconLost must be reported"
    );

    // Restore.
    assert!(handle.show_main());
    let _ = handle.set_icon_visible(false);
    TestWindow::pump(Duration::from_millis(400));
    assert!(handle.test_probe().icon_rect.is_none());
    handle.remove();
    assert!(handle.test_probe().icon_rect.is_none());
    assert!(!handle.test_probe().icon_visible);
}

#[test]
#[ignore = "requires an interactive desktop shell; waits for the 30 s blink cap"]
fn flash_stops_itself_after_the_cap() {
    let _guard = serial();
    let window = TestWindow::create();
    let handle = tray::install_with_hwnd(window.raw(), options()).expect("tray install");
    assert!(handle.hide_main(), "a desktop session must accept NIM_ADD");
    handle.set_alert(true);
    assert!(handle.test_probe().alerting);
    let stopped = TestWindow::wait_until(Duration::from_secs(35), || !handle.test_probe().alerting);
    assert!(
        stopped,
        "the tray thread must stop blinking after the 30 s cap on its own"
    );
    let _ = handle.set_icon_visible(false);
    handle.remove();
}

#[test]
#[ignore = "requires an interactive desktop shell; checks that hiding leaves no visible window"]
fn hide_then_show_restores_visibility() {
    let _guard = serial();
    let window = TestWindow::create();
    let handle = tray::install_with_hwnd(window.raw(), options()).expect("tray install");
    assert!(handle.hide_main());
    assert!(TestWindow::wait_until(Duration::from_millis(3000), || {
        !window.visible()
    }));
    assert!(handle.show_main());
    assert!(TestWindow::wait_until(Duration::from_millis(3000), || {
        window.visible()
    }));
    let _ = handle.set_icon_visible(false);
    handle.remove();
}
