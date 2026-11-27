//! Architecture source contract for the tray / alert / lifecycle surface.
//!
//! `docs/design/TRAY_ALERT_DESIGN.md` §12 fixes the ports-and-adapters boundary: the
//! tray/alerts/lifecycle modules have a single Win32 port (`tray.rs`),
//! `tray_ui`/`tray_lifecycle` stay pure, the MCP notifier and the relay watcher
//! stay headless, and the desktop modules are adapters. The runtime behaviour
//! is covered elsewhere; this file pins the dependency directions that no
//! runtime test can observe.
//!
//! Source is read at test time (`env!("CARGO_MANIFEST_DIR")` would bake an
//! absolute path into the binary — AGENTS pit #4) and every assertion is a
//! token check, never a line count. No GPUI wildcard imports here (pit #36).

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
}

fn desktop_src() -> PathBuf {
    manifest_dir().join("src")
}

fn repo_root() -> PathBuf {
    manifest_dir()
        .parent()
        .and_then(Path::parent)
        .expect("apps/desktop lives two levels below the repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn read_desktop(relative: &str) -> String {
    let path = desktop_src().join(relative);
    if path.is_file() {
        return read(&path);
    }
    if let Some(stem) = relative.strip_suffix(".rs") {
        let dir = desktop_src().join(stem);
        if dir.is_dir() {
            let mut files = Vec::new();
            collect_rust_files(&dir, &mut files);
            files.sort_by(|a, b| {
                let a_mod = a.file_name().is_some_and(|n| n == "mod.rs");
                let b_mod = b.file_name().is_some_and(|n| n == "mod.rs");
                b_mod.cmp(&a_mod).then_with(|| a.cmp(b))
            });
            return files
                .into_iter()
                .map(|p| read(&p))
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    read(&path)
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read dir {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("path under root")
        .to_string_lossy()
        .replace('\\', "/")
}

/// Whether the source links the `windows` crate itself.
///
/// `std::os::windows::process::CommandExt` is a std extension trait, not the
/// crate, so the needles deliberately mention the crate's own module roots.
fn links_windows_crate(source: &str) -> bool {
    [
        "windows::Win32",
        "windows::core",
        "windows::Foundation",
        "use windows::",
    ]
    .iter()
    .any(|needle| source.contains(needle))
}

/// Slice from `anchor` to the next function header (same helper shape as the
/// pre-existing `tray_contract.rs`; bin modules cannot be instantiated here).
fn fn_block<'a>(source: &'a str, anchor: &str) -> &'a str {
    let start = source
        .find(anchor)
        .unwrap_or_else(|| panic!("missing anchor `{anchor}`"));
    let tail = &source[start + anchor.len()..];
    let mut end = tail.len();
    for marker in [
        "\n    fn ",
        "\n    pub fn ",
        "\n    pub(crate) fn ",
        "\n    pub(super) fn ",
        "\nfn ",
    ] {
        if let Some(index) = tail.find(marker) {
            end = end.min(index);
        }
    }
    &source[start..start + anchor.len() + end]
}

#[test]
fn the_tray_surface_has_a_single_win32_module() {
    const ALLOWED: &[&str] = &["tray.rs", "agent_host/job.rs"];
    let src = desktop_src();
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);
    // `tray.rs` is the tray/alert surface's Win32 port. `agent_host/job.rs` is
    // the pre-existing Job Object port from an earlier feature; it is not part
    // of this surface and is reported as the one remaining Win32 coupling
    // (design §12.6). Anything else on the tray/alert surface must go through
    // the tray port; `the_close_and_alert_surface_never_touches_win32` below
    // enforces exactly that list.
    let mut offenders = Vec::new();
    for path in files {
        let file = relative(&src, &path);
        if ALLOWED.contains(&file.as_str()) {
            continue;
        }
        if links_windows_crate(&read(&path)) {
            offenders.push(file);
        }
    }
    assert!(
        offenders.is_empty(),
        "these modules must use the tray port instead of linking the windows crate: {offenders:?}"
    );
}

#[test]
fn the_close_and_alert_surface_never_touches_win32() {
    for file in [
        "workspace.rs",
        "alerts.rs",
        "mcp_runtime.rs",
        "main.rs",
        "tray_ui.rs",
        "tray_lifecycle.rs",
    ] {
        let source = read_desktop(file);
        assert!(
            !links_windows_crate(&source),
            "{file} must stay behind the tray port"
        );
    }
}

#[test]
fn the_tray_module_carries_both_platform_implementations() {
    let tray = read_desktop("tray.rs");
    for needle in [
        "pub trait TrayBackend",
        "#[cfg(windows)]",
        "#[cfg(not(windows))]",
        "mod platform",
        "pub fn install(",
        "pub use platform::{TrayHandle",
    ] {
        assert!(tray.contains(needle), "tray.rs is missing `{needle}`");
    }
    // The port names the design froze (§8/§12) plus the balloon channel added
    // for the quiet-time contract.
    for method in [
        "fn hide_main(&self)",
        "fn show_main(&self)",
        "fn set_visible(&self",
        "fn set_tooltip(&self",
        "fn notify_balloon(&self",
        "fn start_flash(&self)",
        "fn stop_flash(&self)",
        "fn drain_events(&self)",
        "fn remove(&self)",
    ] {
        assert!(tray.contains(method), "TrayBackend is missing `{method}`");
    }
}

/// The single-instance guard's Win32 half must stay in the tray port; the
/// pre-existing `tray_contract.rs::main_guards_a_single_instance` only sees
/// `main.rs`'s doc words, so a gutted `single_instance` would slip through.
#[test]
fn the_single_instance_guard_runs_through_the_tray_port() {
    let main = read_desktop("main.rs");
    assert!(
        main.contains("duthub_desktop::tray::single_instance::acquire()"),
        "main must acquire the guard through the tray port"
    );
    assert!(
        main.contains("duthub_desktop::tray::single_instance::show_already_running("),
        "the second-instance message must come from the tray port"
    );
    assert!(
        !main.contains("fn acquire_single_instance"),
        "main.rs must not link Win32 directly"
    );
    let tray = read_desktop("tray.rs");
    for needle in ["CreateMutexW", "ERROR_ALREADY_EXISTS", "MessageBoxW"] {
        assert!(
            tray.contains(needle),
            "the guard's Win32 implementation is missing `{needle}`"
        );
    }
}

/// The close gate schedules the coordinator's plan and the executor dispatches
/// it; the old literal-only assertions could stay green with the loop removed.
#[test]
fn the_close_executor_dispatches_the_coordinator_actions() {
    let workspace = read_desktop("workspace.rs");
    let gate = fn_block(&workspace, "fn prepare_main_window_close(");
    assert!(
        gate.contains("close_disposition(") && gate.contains("close_plan("),
        "the frozen disposition contract still decides hide vs quit"
    );
    assert!(
        gate.contains("on_event(") && gate.contains("CloseRequested"),
        "the close gate must ask the lifecycle coordinator"
    );
    assert!(gate.contains("cx.defer("), "the plan runs deferred");
    let executor = fn_block(&workspace, "fn run_close_plan(");
    assert!(
        executor.contains("actions_for_close_plan(plan)"),
        "the executor must expand the frozen plan through the coordinator mapping"
    );
    assert!(
        executor.contains("run_lifecycle_action(weak, action, cx)"),
        "each action goes through the single lifecycle executor"
    );
}

/// A programmatic float close takes the handle before `remove_window`, so the
/// deferred user-close observer cannot reset the placement; without the reset
/// the following `StopPreview` is a silent no-op (design §2.1).
#[test]
fn a_programmatic_float_close_restores_the_docked_placement() {
    let workspace = read_desktop("workspace.rs");
    let close = fn_block(&workspace, "fn close_float_window(");
    assert!(
        close.contains("forget_float_window("),
        "close_float_window must run the shared placement/bookkeeping reset"
    );
    let forget = fn_block(&workspace, "fn forget_float_window(");
    assert!(
        forget.contains("MonitorPlacement::Docked"),
        "the shared reset helper must clear a stale Floating placement"
    );
}

#[test]
fn pure_modules_stay_free_of_gpui_and_os_types() {
    let tray_ui = read_desktop("tray_ui.rs");
    assert!(!tray_ui.contains("gpui"));
    // A doc link to the Win32 side is fine; an import would make the pure
    // module depend on the platform port.
    assert!(!tray_ui.contains("use crate::tray"));
    assert!(!tray_ui.contains("use crate::workspace"));
    for needle in [
        "pub enum AlertRoute",
        "pub fn alert_route(",
        "pub struct FlashPlan",
        "pub struct HiddenAlertRouter",
        "pub fn close_plan(",
    ] {
        assert!(tray_ui.contains(needle), "tray_ui is missing `{needle}`");
    }

    let lifecycle = read_desktop("tray_lifecycle.rs");
    assert!(!lifecycle.contains("gpui"));
    assert!(!lifecycle.contains("use crate::workspace"));
    assert!(!lifecycle.contains("crate::alerts"));
    for needle in [
        "pub enum LifecycleEvent",
        "pub enum LifecycleAction",
        "pub fn on_event(",
        "pub fn actions_for_close_plan(",
        "pub fn run_tray_actions(",
    ] {
        assert!(
            lifecycle.contains(needle),
            "tray_lifecycle is missing `{needle}`"
        );
    }
    // Pit #36: a wildcard GPUI import would pull the `test` attribute macro
    // into a module that has ordinary unit tests.
    for (file, source) in [("tray_ui.rs", &tray_ui), ("tray_lifecycle.rs", &lifecycle)] {
        assert!(
            !source.contains("use gpui_kit::*"),
            "{file} must not wildcard-import gpui_kit"
        );
    }
}

#[test]
fn alerts_is_a_narrow_ui_free_port() {
    let alerts = read_desktop("alerts.rs");
    assert!(!alerts.contains("gpui"));
    // A doc link to `crate::tray_ui::AlertPresentation` is the documented
    // boundary; an import of the tray port or the workbench is not.
    assert!(!alerts.contains("use crate::tray"));
    assert!(!alerts.contains("use crate::workspace"));
    for method in [
        "pub fn report_for(",
        "pub fn report_checked_for(",
        "pub fn report_checked_at_for(",
        "pub fn latest(&self)",
        "pub fn generation(&self)",
        "pub fn enabled_for(",
        "pub fn is_suppressed_for(",
        "pub fn pop_next(&mut self)",
    ] {
        assert!(
            alerts.contains(method),
            "AlertCenter's narrow port is missing `{method}`"
        );
    }
    // A new alert source only needs the enum + i18n + suppression list; the
    // tray/close machinery must not know about sources.
    let lifecycle = read_desktop("tray_lifecycle.rs");
    assert!(
        !lifecycle.contains("AlertSource"),
        "window lifecycle must not switch on alert sources"
    );
    let tray_ui = read_desktop("tray_ui.rs");
    assert!(
        !tray_ui.contains("AlertSource"),
        "presentation must not switch on alert sources"
    );
}

#[test]
fn relay_watch_is_a_headless_port() {
    let watch = read(&repo_root().join("crates/duthub-device/src/relay_watch.rs"));
    for forbidden in ["AlertCenter", "gpui", "duthub_desktop", "pending_toasts"] {
        assert!(
            !watch.contains(forbidden),
            "relay_watch must not depend on `{forbidden}`"
        );
    }
    for needle in [
        "pub struct RelayBoardEvent",
        "pub trait RelayBoardProbe",
        "pub struct RelayWatchService",
    ] {
        assert!(watch.contains(needle), "relay_watch is missing `{needle}`");
    }
    // The ControlService adapter lives in control.rs, not in the pure module.
    let control = read(&repo_root().join("crates/duthub-device/src/control.rs"));
    assert!(control.contains("impl RelayBoardProbe for ControlRelayProbe"));
}

#[test]
fn mcp_notify_is_a_port_and_the_desktop_is_only_an_adapter() {
    let notify = read(&repo_root().join("crates/duthub-mcp/src/notify.rs"));
    assert!(notify.contains("pub trait UserNotifier"));
    for forbidden in ["duthub_desktop", "gpui", "crate::tray"] {
        assert!(
            !notify.contains(forbidden),
            "the MCP notify port must not reference `{forbidden}`"
        );
    }
    let manifest = read(&repo_root().join("crates/duthub-mcp/Cargo.toml"));
    for forbidden in ["duthub-desktop", "gpui"] {
        assert!(
            !manifest.contains(forbidden),
            "duthub-mcp must not depend on `{forbidden}`"
        );
    }
    let adapter = read_desktop("mcp_runtime.rs");
    assert!(
        adapter.contains("impl UserNotifier for RuntimeMcpState"),
        "the notifier adapter lives in mcp_runtime.rs only"
    );
    assert!(
        adapter.contains("crate::alerts::agent_notice_alert_texts"),
        "the adapter turns the notice into an AgentNotice alert"
    );
}
