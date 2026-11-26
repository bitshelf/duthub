//! Source contracts for the tray / hidden-alert wiring (A2, `TRAY_ALERT_DESIGN.md`).
//!
//! These pin ownership *shapes* rather than exact lines: which function
//! decides a close, which one destroys windows, where the tray is installed,
//! and that the 50 ms tick is the single owner of the hidden-state work. The
//! pure behaviour lives in `duthub_desktop::tray_ui` and `alerts.rs` unit
//! tests; this file only guards the integration points that have no runtime
//! test (the bin target cannot be instantiated in a test).

use std::path::PathBuf;

fn source_of(relative: &str) -> String {
    // Runtime read: `env!` would bake an absolute path into the test binary
    // (AGENTS pit #4).
    let src_root =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir")).join("src");
    let path = src_root.join(relative);
    if path.is_file() {
        return std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
    }
    if let Some(stem) = relative.strip_suffix(".rs") {
        let dir = src_root.join(stem);
        if dir.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "rs"))
                .collect();
            files.sort_by(|a, b| {
                let a_mod = a.file_name().is_some_and(|n| n == "mod.rs");
                let b_mod = b.file_name().is_some_and(|n| n == "mod.rs");
                b_mod.cmp(&a_mod).then_with(|| a.cmp(b))
            });
            return files
                .into_iter()
                .map(|p| {
                    std::fs::read_to_string(&p)
                        .unwrap_or_else(|error| panic!("read {}: {error}", p.display()))
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    panic!("missing source {relative}");
}

/// Slice from `anchor` to the next item header (function or free function).
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

/// Fixed-size window after `anchor` (for constructs `fn_block` cannot cut).
fn chunk_after<'a>(source: &'a str, anchor: &str, len: usize) -> &'a str {
    let start = source
        .find(anchor)
        .unwrap_or_else(|| panic!("missing anchor `{anchor}`"));
    let end = (start + len).min(source.len());
    &source[start..end]
}

#[test]
fn every_close_entry_goes_through_one_deferred_plan() {
    let workspace = source_of("workspace.rs");
    let gate = fn_block(&workspace, "fn prepare_main_window_close(");
    assert!(
        gate.contains("close_disposition("),
        "gate must decide hide vs quit"
    );
    assert!(
        gate.contains("close_plan("),
        "gate must use the frozen plan"
    );
    assert!(
        gate.contains("cx.defer("),
        "plan runs outside the close callback"
    );
    assert!(gate.contains("false"), "the platform close must be vetoed");
    assert!(
        !gate.contains("handle_monitor_event")
            && !gate.contains("close_float_window")
            && !gate.contains("remove_window"),
        "the close callback must not touch windows; that belongs to the plan"
    );

    let plan = fn_block(&workspace, "fn run_close_plan(");
    // The executor must dispatch from the frozen plan itself; the coordinator
    // transition is only cross-checked (a release build compiles the debug
    // assertion away, so executing `transition.actions` would let a drifted
    // plan run unnoticed).
    assert!(
        plan.contains("actions_for_close_plan(plan)"),
        "the executor derives its actions from the frozen plan"
    );
    assert!(
        plan.contains("for action in actions"),
        "the executor runs the plan-derived actions"
    );
    assert!(
        !plan.contains("for action in transition.actions"),
        "the executor must not run the coordinator's independent copy"
    );
    assert!(
        plan.contains("run_lifecycle_action("),
        "the plan is executed through the lifecycle executor"
    );
}

#[test]
fn tray_is_installed_on_the_first_render_and_hidden_from_the_tick() {
    let workspace = source_of("workspace.rs");
    assert!(
        workspace.contains("tray: Option<duthub_desktop::tray::TrayHandle>"),
        "the tray handle is owned by the Workbench"
    );
    let render = fn_block(&workspace, "pub fn render_root(");
    assert!(
        render.contains("!self.agent_self_test")
            && render.contains("duthub_desktop::tray::install("),
        "tray install must be guarded by the self-test flag"
    );
    assert!(
        render.contains("self.dialog_open ="),
        "dialog state is read here"
    );
    assert!(
        render.contains("if !self.hidden"),
        "the alert dialog must stay queued while hidden"
    );
    assert!(
        !render.contains("self.refresh_snapshot();"),
        "snapshot refresh moved to the tick"
    );
    assert!(
        !render.contains("media.reconcile_bindings("),
        "media reconciliation already runs in MediaView::tick"
    );
    // The visible-state alert dialog uses a 42px squircle badge and routes
    // non-DUT suppression to the global list.
    let body = fn_block(&workspace, "fn render_alert_dialog_body(");
    assert!(body.contains("w(px(42.))"), "alert badge is 42px");
    assert!(body.contains("if this.is_saved_dut_id(&dut_id)"));
    assert!(body.contains("this.suppress_alert_source_globally(source)"));
}

#[test]
fn hidden_work_has_one_owner_in_the_50ms_tick() {
    let workspace = source_of("workspace.rs");
    let tick = chunk_after(&workspace, "timer(Duration::from_millis(50))", 2_400);
    for call in [
        "view.heartbeat_tick()",
        "view.agent_host_tick()",
        "view.drain_jobs()",
        "view.sync_relay_watch()",
        "view.drain_relay_events()",
        "view.alert_present_tick()",
        "view.drain_tray_events(cx)",
        "view.snapshot_tick()",
        "view.media.tick(&view.duts)",
    ] {
        assert!(tick.contains(call), "50ms tick must keep calling {call}");
    }
    // The relay probe thread only probes and queues events; the UI thread
    // alone writes alerts and toasts.
    let thread = chunk_after(&workspace, "duthub-relay-watch", 900);
    assert!(
        thread.contains("tick_at("),
        "probe thread drives the service"
    );
    assert!(
        !thread.contains("AlertCenter") && !thread.contains("pending_toasts"),
        "the probe thread must not touch UI state"
    );
    // The watch set is aligned through the monitor's own helpers, so the
    // board→channel choice and the "unconfigured board is not probed" rule
    // have one source of truth (B's `board_probe_channels`).
    let sync = fn_block(&workspace, "fn sync_relay_watch(");
    assert!(sync.contains("board_probe_channels("));
    assert!(sync.contains("is_watchable_board("));
    assert!(sync.contains("set_channels("));
    assert!(sync.contains("sync_boards("));
}

#[test]
fn hide_and_show_stay_behind_the_tray_api() {
    let workspace = source_of("workspace.rs");
    assert!(workspace.contains("tray.hide_main("));
    assert!(workspace.contains("tray.show_main("));
    for forbidden in ["ShowWindow", "Shell_NotifyIconW", "Shell_NotifyIcon("] {
        assert!(
            !workspace.contains(forbidden),
            "workspace.rs must not call {forbidden} directly (A2 uses the tray API)"
        );
    }
    let hide = fn_block(&workspace, "fn finish_hide_to_tray(");
    assert!(
        hide.contains("hide_main()"),
        "hide_main adds the icon before hiding and reports the real result"
    );
    assert!(
        hide.contains("set_icon_visible(false)"),
        "a failed hide clears any partially added icon"
    );
    assert!(
        hide.contains("self.hidden = false"),
        "hide_main == false must leave the app in the visible state"
    );
    assert!(hide.contains("trayHideFailed"), "a failure is user-visible");
    assert!(hide.contains("set_merge_duplicates(true)"));
    assert!(hide.contains("arm()"));
    assert!(
        !hide.contains("tray.notify("),
        "minimizing to the tray must be silent"
    );
    let show = fn_block(&workspace, "fn show_from_tray(");
    assert!(show.contains("set_icon_visible(false)"));
    assert!(show.contains("set_alert(false)"));
    assert!(show.contains("set_merge_duplicates(false)"));
}

#[test]
fn icon_loss_and_quit_paths_never_leave_a_hidden_process() {
    let workspace = source_of("workspace.rs");
    let recovery = fn_block(&workspace, "fn recover_from_icon_loss(");
    assert!(recovery.contains("show_from_tray(cx)"));
    assert!(recovery.contains("trayIconFailed"));
    let events = fn_block(&workspace, "fn drain_tray_events(");
    assert!(events.contains("TrayEvent::Show"));
    assert!(events.contains("TrayEvent::Quit"));
    assert!(events.contains("TrayEvent::IconLost"));

    let quit = fn_block(&workspace, "pub(crate) fn begin_quit(");
    assert!(quit.contains("release_relay_and_join("));
    assert!(quit.contains("close_session"));
    assert!(quit.contains("stop_all()"));
    assert!(quit.contains("mcp_runtime.shutdown()"));
    assert!(quit.contains("tray.remove()"));
    assert!(quit.contains("cx.quit()"));

    let release = fn_block(&workspace, "fn release_relay_and_join(");
    assert!(release.contains("Duration::from_secs(3)"), "bounded wait");
    assert!(
        release.contains("trayRelayOffUnconfirmed"),
        "timeout is reported"
    );
    let close_release = fn_block(&workspace, "fn release_relay_for_close(");
    assert!(close_release.contains("trayRelayReleaseMark"));
    let drop = chunk_after(&workspace, "impl Drop for Workbench {", 900);
    assert!(
        drop.contains("relay_press.take()"),
        "Drop also releases the relay for quit paths that bypass begin_quit"
    );
}

#[test]
fn window_closed_observer_runs_deferred() {
    let workspace = source_of("workspace.rs");
    assert_eq!(
        workspace.matches("cx.on_window_closed(").count(),
        1,
        "there is exactly one window-closed observer (float + main branches)"
    );
    let observer = chunk_after(&workspace, "cx.on_window_closed(", 700);
    assert!(
        observer.contains("cx.defer("),
        "the observer body must defer before re-entering the Workbench"
    );
}

#[test]
fn non_dut_suppression_persists_without_a_settings_panel() {
    let workspace = source_of("workspace.rs");
    let helper = fn_block(&workspace, "fn is_saved_dut_id(");
    assert!(
        helper.contains("new_connection"),
        "drafts are still editable DUTs"
    );
    assert!(workspace.contains("pub fn suppress_alert_source_globally("));
    let suppress = fn_block(&workspace, "pub fn suppress_alert_source_globally(");
    assert!(suppress.contains("suppressed_sources"));
    assert!(suppress.contains("persist_settings()"));
    assert!(suppress.contains("drop_source("));
    let mute_action = chunk_after(&workspace, "\"alert-suppress\"", 1_350);
    assert!(mute_action.contains("if this.is_saved_dut_id(&dut_id)"));
    assert!(mute_action.contains("this.suppress_alert_source_globally(source)"));
    let pages = source_of("pages.rs");
    assert!(!pages.contains("global_alerts_panel"));
    assert!(!pages.contains("reset_global_alert_suppression()"));
}

#[test]
fn main_guards_a_single_instance() {
    // The guard itself lives in `tray::single_instance` (A1), so pin the call
    // and the implementation there instead of matching comment text. This test
    // would still pass if only a doc comment named `CreateMutexW`, which is
    // exactly what it used to do.
    let main = source_of("main.rs");
    assert!(
        main.contains("single_instance::acquire()"),
        "main must actually acquire the single-instance mutex"
    );
    assert!(main.contains("if self_test"), "--self-test skips the guard");
    assert!(
        main.contains("show_already_running"),
        "a second instance must tell the user why it exits"
    );
    assert!(main.contains("alreadyRunningBody"));
    let tray = source_of("tray.rs");
    assert!(tray.contains("pub mod single_instance"));
    assert!(tray.contains("CreateMutexW("));
    assert!(tray.contains("ERROR_ALREADY_EXISTS"));
    assert!(tray.contains("MessageBoxW("));
}

#[test]
fn settings_import_resyncs_the_alert_center() {
    let workspace = source_of("workspace.rs");
    let import = chunk_after(&workspace, "pub fn import_config(", 2_600);
    assert!(
        import.contains("set_suppressed_sources("),
        "importing settings must replace the shared suppression mirror"
    );
    assert!(
        import.contains("self.alerts"),
        "the import path must touch the shared AlertCenter"
    );
    let alerts = source_of("alerts.rs");
    let setter = fn_block(&alerts, "pub fn set_suppressed_sources(");
    assert!(setter.contains("self.suppressed ="));
    assert!(
        setter.contains("AlertSource::from_key"),
        "unknown keys stay forward-compatible"
    );
}

/// `garbledByDut` is a shared runtime gate (AGENTS #133 family): replacing the
/// in-memory DUT list from `duts.json` must re-derive it, or the imported
/// switches show as on while the device layer keeps observing nothing.
#[test]
fn duts_import_resyncs_the_garbled_detection_gate() {
    let workspace = source_of("workspace.rs");
    // Anchor on the wholesale replacement and slice to the branch's own toast:
    // the settings branch above it never touches `garbledByDut`, and a fixed
    // window would silently shrink if the branch grows.
    let start = workspace
        .find("DutsState::from_file(d)")
        .expect("the duts.json import must replace the in-memory DUT list");
    let tail = &workspace[start..];
    let end = tail
        .find("\"dutsImported\"")
        .expect("the duts.json import must end with its own toast");
    let branch = &tail[..end];
    let mirror = branch
        .find("set_dut_settings(")
        .expect("the duts.json import must refresh the per-DUT alert mirror");
    let gate = branch
        .find("self.sync_garbled_runtime();")
        .expect("the duts.json import must re-derive the garbled detection gate");
    assert!(
        gate > mirror,
        "the gate is derived from the imported DUT list, so it runs after the mirror"
    );
}

/// Turning the gate off must clear the UI state here: the device only
/// publishes a snapshot on new output, so a quiet session would otherwise keep
/// a stale amber tab dot (and a stale `garbled` stat in the last snapshot).
#[test]
fn turning_off_the_garbled_gate_clears_the_stale_indicator() {
    let workspace = source_of("workspace.rs");
    let sync = fn_block(&workspace, "fn sync_garbled_runtime(");
    assert!(
        sync.contains("if effective {"),
        "the reset only applies when the gate is off"
    );
    assert!(
        sync.contains("last_garbled_level = GarbleLevel::Clean"),
        "the sync must reset the session tab warning dot"
    );
    assert!(
        sync.contains("make_mut(") && sync.contains(".garbled = Default::default()"),
        "the sync must clear the stale garbled stats in the last snapshot"
    );
}

#[test]
fn quit_paths_release_the_monitor_lease_and_keep_the_tray_tooltip_current() {
    let workspace = source_of("workspace.rs");
    // A float window closes before StopPreview on the quit path; the pump must
    // leave `Floating` or `release_monitor` refuses and the lease leaks.
    let close = fn_block(&workspace, "fn close_float_window(");
    assert!(close.contains("forget_float_window("));
    let forget = fn_block(&workspace, "fn forget_float_window(");
    assert!(forget.contains("set_placement("));
    let release = fn_block(&workspace, "pub(crate) fn release_monitor(");
    assert!(
        release.contains("!self.quitting"),
        "a quit path may release even while the pump still says Floating"
    );
    // The tray tooltip follows a language switch.
    assert!(workspace.contains("tray.set_tooltip("));
    // A blink stops when the system animation preference turns off mid-run.
    let tick = fn_block(&workspace, "fn alert_present_tick(");
    assert!(tick.contains("animations_enabled()"));
    assert!(tick.contains("stop_alert_flash()"));
}

#[test]
fn notify_user_is_wired_only_for_user_configured_listeners() {
    let runtime = source_of("mcp_runtime.rs");
    assert!(runtime.contains("impl UserNotifier for RuntimeMcpState"));
    assert!(
        runtime.contains("notifier: Option<Arc<dyn duthub_mcp::notify::UserNotifier>>"),
        "spawn_mcp_runtime takes the notifier"
    );
    assert!(runtime.contains("crate::alerts::agent_notice_alert_texts"));
    let workspace = source_of("workspace.rs");
    assert!(workspace.contains("Some(runtime_state as Arc<dyn duthub_mcp::notify::UserNotifier>)"));

    // Desktop-initiated Agent sessions (CLI profiles) share the single source
    // of truth `duthub_mcp::PLAN_TOOL_NAMES`: 13 read-only Plan tools and no `notify_user`.
    let cli = source_of("agent_host/cli.rs");
    assert!(
        cli.contains("duthub_mcp::PLAN_TOOL_NAMES"),
        "cli.rs must alias PLAN_TOOLS to duthub_mcp::PLAN_TOOL_NAMES"
    );
    let plan_tools = duthub_desktop::agent_host::cli::PLAN_TOOLS;
    assert!(!plan_tools.contains(&"notify_user"));
    assert_eq!(
        plan_tools.len(),
        13,
        "PLAN_TOOLS stays at 13 read-only tools"
    );
}

#[test]
fn alert_center_keeps_the_legacy_bool_api_and_adds_checked_reports() {
    let alerts = source_of("alerts.rs");
    assert!(alerts.contains("pub const MAX_PENDING: usize = 64"));
    assert!(alerts.contains("pub const MAX_PENDING_PER_SOURCE: usize = 16"));
    assert!(alerts.contains("pub enum ReportRejection"));
    assert!(alerts.contains("pub fn report_checked_at_for("));
    assert!(alerts.contains("pub fn report_checked_for("));
    assert!(alerts.contains("pub fn latest(&self)"));
    assert!(alerts.contains("pub fn generation(&self)"));
    for source in ["\"heartbeat\"", "\"relay\"", "\"agentNotice\""] {
        assert!(alerts.contains(source), "source key {source}");
    }
    let legacy = fn_block(&alerts, "pub fn report_for(");
    assert!(
        legacy.contains("-> bool"),
        "existing call sites keep `-> bool`"
    );
    let legacy_at = fn_block(&alerts, "fn report_at_for(");
    assert!(legacy_at.contains("-> bool"));
    assert!(
        legacy_at.contains("report_checked_at_for("),
        "the bool variants delegate to the checked implementation"
    );
}
