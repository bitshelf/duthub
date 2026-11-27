//! Router-to-tray wiring (`docs/design/TRAY_ALERT_DESIGN.md` §10.2): drive
//! [`HiddenAlertRouter`] through one presentable alert and apply the decision to
//! a real [`TrayHandle`].
//!
//! This is the only place where the pure presentation policy and the Win32
//! balloon/blink side meet without the desktop UI, so it catches "the tick
//! decided `BalloonAndFlash` but nothing was actually shown".
//!
//! Needs an interactive desktop shell (the shell must host the icon), therefore
//! `#[ignore]`; acceptance runs it explicitly:
//!
//! ```text
//! cargo test -p duthub-desktop --test tray_alert_route -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use duthub_desktop::tray::{self, TrayOptions};
use duthub_desktop::tray_ui::{
    AlertPresentation, HiddenAlertAction, HiddenAlertEnv, HiddenAlertRouter, NotificationGate,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HCURSOR, HICON, MSG,
    PM_REMOVE, PeekMessageW, RegisterClassW, SW_SHOW, ShowWindow, TranslateMessage, WM_DESTROY,
    WNDCLASS_STYLES, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
};
use windows::core::PCWSTR;

/// All tray tests in this binary share one notification-icon GUID, so they run
/// one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn options() -> TrayOptions {
    TrayOptions {
        tooltip: "duthub tray alert route".to_string(),
        show_label: "Show main window".to_string(),
        quit_label: "Quit".to_string(),
    }
}

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
            let class_name = windows::core::w!("DuthubTrayAlertRouteHost");
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
            RegisterClassW(&wnd_class);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name,
                windows::core::w!("duthub tray alert route host"),
                WS_OVERLAPPEDWINDOW,
                140,
                140,
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
        window.pump(Duration::from_millis(50));
        window
    }

    fn raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    fn pump(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        loop {
            let mut msg = MSG::default();
            // SAFETY: Pumps queued Win32 messages for windows owned by the current thread.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
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

/// Test window procedure for the hidden alert-route host window.
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
        // SAFETY: Forwards unhandled messages to Win32's default window procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

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
#[ignore = "requires an interactive desktop shell (explorer.exe); run explicitly for acceptance"]
fn router_present_decision_reaches_the_tray() {
    let _guard = serial();
    let window = TestWindow::create();
    let handle = tray::install_with_hwnd(window.raw(), options()).expect("tray install");
    assert!(handle.hide_main(), "a desktop session must accept NIM_ADD");
    window.pump(Duration::from_millis(200));

    let mut router = HiddenAlertRouter::new();
    let action = router.step(1, hidden_env(3), 0);
    let presentation = match action {
        HiddenAlertAction::Present {
            presentation,
            count,
        } => {
            assert_eq!(count, 3, "the count is the pending length");
            presentation
        }
        other => panic!("a hidden window with an allowed gate must present, got {other:?}"),
    };
    assert_eq!(presentation, AlertPresentation::BalloonAndFlash);

    // Apply exactly what the tick would: the balloon carries the alert detail,
    // not the title, and the blink starts.
    let balloons_before = handle.test_probe().balloons_sent;
    handle.notify(
        "duthub 需要处理 3 条告警",
        "RELAY-1 offline",
        tray::TrayNotifyLevel::Error,
    );
    handle.set_alert(true);
    window.pump(Duration::from_millis(400));

    let probe = handle.test_probe();
    assert!(
        probe.balloons_sent > balloons_before,
        "the shell must accept the balloon"
    );
    assert!(probe.alerting, "the icon must be blinking");

    // The next tick of the same generation presents nothing new.
    assert_eq!(
        router.step(1, hidden_env(3), 100),
        HiddenAlertAction::None,
        "one generation is presented once"
    );

    // Restoring stops the blink and drops the icon.
    handle.set_alert(false);
    assert!(handle.show_main());
    let _ = handle.set_icon_visible(false);
    window.pump(Duration::from_millis(200));
    assert!(!handle.test_probe().alerting);
    assert!(!handle.test_probe().icon_visible);
    handle.remove();
}
