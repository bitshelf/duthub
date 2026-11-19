use super::*;

impl Workbench {
    // ------------------------------------------------------------------
    // HDMI monitor: frame pump + float window (§6.3/§6.4)
    // ------------------------------------------------------------------

    /// One pump tick (100ms timer): poll, mirror into the float window and, on
    /// a failure, drive `PreviewFailed` with today's error priority
    /// (`media.capture_error` wins over the raw preview error).
    pub(crate) fn tick_frame_pump(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = self.frame_pump.tick(cx);
        if let Some(raw) = self.frame_pump.take_failure() {
            let detail = self
                .frame_pump
                .key()
                .cloned()
                .and_then(|(dut_id, source)| self.media.capture_error(&dut_id, Some(source)))
                .unwrap_or(raw);
            self.handle_monitor_event(duthub_desktop::capture_ui::MonitorEvent::PreviewFailed, cx);
            self.frame_pump.set_error(Some(detail));
            return true;
        }
        changed
    }

    /// Feed one event into the placement state machine and apply every action
    /// in order (§6.2). The machine is the only writer of the placement.
    pub(crate) fn handle_monitor_event(
        &mut self,
        event: duthub_desktop::capture_ui::MonitorEvent,
        cx: &mut Context<Self>,
    ) {
        let (placement, action) =
            duthub_desktop::capture_ui::next(self.frame_pump.placement(), event);
        self.frame_pump.set_placement(placement);
        for step in action.steps() {
            match *step {
                duthub_desktop::capture_ui::MonitorAction::None
                | duthub_desktop::capture_ui::MonitorAction::ShowDockedPlaceholder => {}
                duthub_desktop::capture_ui::MonitorAction::OpenFloatWindow => {
                    self.open_float_window(cx)
                }
                duthub_desktop::capture_ui::MonitorAction::CloseFloatWindow => {
                    self.close_float_window(cx)
                }
                duthub_desktop::capture_ui::MonitorAction::FocusMainWindow => {
                    self.focus_main_window(cx)
                }
                duthub_desktop::capture_ui::MonitorAction::StopPreview => {
                    self.release_monitor(None, cx)
                }
                // `steps()` expands the composites into the atomic actions above.
                duthub_desktop::capture_ui::MonitorAction::CloseFloatWindowAndFocusMain
                | duthub_desktop::capture_ui::MonitorAction::StopPreviewAndCloseFloatWindow => {}
            }
        }
        cx.notify();
    }

    /// Release the `Monitor` lease and drop the frame. Refuses while the float
    /// window owns the preview (§6.3) — that is what keeps `Floating +
    /// SidePanelClosed` from turning the float into a dead frame. A quit path
    /// has already closed the float window, so it bypasses that guard.
    ///
    /// `window` is the window being updated when called from UI code, so the
    /// App-level drop also clears that window's atlas (it is absent from
    /// `App.windows` during an update). Background paths pass `None`.
    pub(crate) fn release_monitor(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        if self.frame_pump.placement() == duthub_desktop::capture_ui::MonitorPlacement::Floating
            && !self.quitting
        {
            return;
        }
        if let Some((dut_id, source)) = self.frame_pump.key().cloned() {
            self.media.close_inline_capture(&dut_id, source);
        }
        self.frame_pump.clear(cx, window);
    }

    /// `OpenFloatWindow`: one normal window, `focus: false`, sharing the pump's
    /// frames (§6.4). The system titlebar is hidden (`appears_transparent`) so
    /// the window draws its own icon strip; the title still feeds the taskbar
    /// and Alt-Tab.
    pub(super) fn open_float_window(&mut self, cx: &mut Context<Self>) {
        if self.float_window.is_some() {
            return;
        }
        let frame = self.frame_pump.frame();
        let title = self.float_title();
        let workbench = cx.weak_entity();
        let p = self.p;
        let options = gpui_kit::WindowOptions {
            window_bounds: Some(gpui_kit::WindowBounds::centered(
                gpui_kit::size(gpui_kit::px(960.), gpui_kit::px(600.)),
                cx,
            )),
            titlebar: Some(gpui_kit::TitlebarOptions {
                title: Some(title),
                appears_transparent: true,
                ..Default::default()
            }),
            focus: false,
            show: true,
            kind: gpui_kit::WindowKind::Normal,
            is_resizable: true,
            ..Default::default()
        };
        let opened = gpui_kit::open_window(options, cx, move |_window, cx| {
            cx.new(|_cx| crate::monitor_window::FloatMonitor::new(frame, p, workbench.clone()))
        });
        match opened {
            Ok((handle, view)) => {
                self.frame_pump.set_float(Some(view.downgrade()));
                self.float_window = Some(crate::monitor_window::FloatWindow { handle });
            }
            Err(error) => {
                eprintln!("[capture] failed to open HDMI float window: {error}");
                self.frame_pump
                    .set_placement(duthub_desktop::capture_ui::MonitorPlacement::Docked);
            }
        }
    }

    pub(super) fn float_title(&self) -> SharedString {
        let Some((dut_id, source)) = self.frame_pump.key() else {
            return crate::i18n::label("monitorFloatTitle").into();
        };
        let name = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| &dut.id == dut_id)
            .map(|dut| dut.name.clone())
            .unwrap_or_else(|| dut_id.clone());
        let source_name = self
            .duts
            .saved_media(dut_id)
            .map_or_else(|| source.label(), |media| source.named_label(media));
        SharedString::from(format!("{name} · {source_name}"))
    }

    pub(super) fn close_float_window(&mut self, cx: &mut Context<Self>) {
        if let Some(float) = self.float_window.take() {
            let _ = float
                .handle
                .update(cx, |_, window, _| window.remove_window());
        }
        self.forget_float_window();
    }

    /// Drop the float-window bookkeeping after the window itself is gone and
    /// return the pump to `Docked` when it still says `Floating`.
    ///
    /// The deferred `on_window_closed` observer matches on `float_window`, so a
    /// programmatic close (which clears it first) never runs its
    /// `Floating → Docked` transition; without this reset a subsequent
    /// `release_monitor` would be a silent no-op.
    pub(super) fn forget_float_window(&mut self) {
        self.float_window = None;
        self.frame_pump.set_float(None);
        if self.frame_pump.placement() == duthub_desktop::capture_ui::MonitorPlacement::Floating {
            self.frame_pump
                .set_placement(duthub_desktop::capture_ui::MonitorPlacement::Docked);
        }
    }

    pub(crate) fn focus_main_window(&mut self, cx: &mut Context<Self>) {
        if let Some(main) = self.main_window {
            let _ = main.update(cx, |_, window, _| window.activate_window());
        }
    }

    pub(crate) fn focus_float_window(&mut self, cx: &mut Context<Self>) {
        if let Some(float) = self.float_window.as_ref() {
            let _ = float
                .handle
                .update(cx, |_, window, _| window.activate_window());
        }
    }

    /// Open the monitor float window (if docked) and switch it into fullscreen.
    pub(crate) fn open_monitor_fullscreen(&mut self, cx: &mut Context<Self>) {
        if self.frame_pump.placement() != duthub_desktop::capture_ui::MonitorPlacement::Floating {
            self.handle_monitor_event(duthub_desktop::capture_ui::MonitorEvent::OpenFloat, cx);
        }
        if let Some(float) = self.float_window.as_ref() {
            let _ = float.handle.update(cx, |_, window, _| {
                window.activate_window();
                if !window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
                window.refresh();
            });
        }
    }

    /// Float window closed by its toolbar X or the OS button (`on_window_closed`).
    /// `UserClosedWindow` → `Docked` + `ShowDockedPlaceholder`; frames keep
    /// flowing into the docked pane when the Monitor panel is on screen, and
    /// the lease is released when nothing displays them.
    pub(super) fn on_float_window_closed(&mut self, cx: &mut Context<Self>) {
        self.float_window = None;
        self.frame_pump.set_float(None);
        let (placement, action) = duthub_desktop::capture_ui::next(
            self.frame_pump.placement(),
            duthub_desktop::capture_ui::MonitorEvent::UserClosedWindow,
        );
        self.frame_pump.set_placement(placement);
        if action.contains(duthub_desktop::capture_ui::MonitorAction::ShowDockedPlaceholder)
            && self.side_panel.visible != Some(crate::side_panel::Panel::Monitor)
        {
            self.release_monitor(None, cx);
        }
        cx.notify();
    }

    /// Main window close gate (design §2.1). The single decision point for
    /// every close entry (titlebar X, Alt+F4, system menu, taskbar close):
    ///
    /// - `quitting` (tray Quit) lets the close through: no window should be
    ///   resurrected while the process is leaving.
    /// - otherwise the pure `close_disposition` decides hide-vs-quit, and the
    ///   ordered plan runs one frame later through `run_close_plan` — never
    ///   inside this callback, which holds a Workbench lease while the window
    ///   trail is still running.
    ///
    /// Always returns `false`: the window is destroyed only by an explicit
    /// `remove_window` step, never by the platform close path.
    pub(super) fn prepare_main_window_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.quitting {
            return true;
        }
        let disposition =
            duthub_desktop::tray_ui::close_disposition(self.tray.is_some(), self.quitting);
        let plan = duthub_desktop::tray_ui::close_plan(disposition);
        // The coordinator owns the decision (state + ordered actions); the
        // frozen `close_plan` is passed along so the executor can cross-check
        // the order it was derived from (design §2.1).
        let transition = duthub_desktop::tray_lifecycle::on_event(
            self.lifecycle_state(),
            LifecycleEvent::CloseRequested,
        );
        let weak = cx.weak_entity();
        cx.defer(move |cx| run_close_plan(&weak, plan, transition, cx));
        false
    }

    /// Current lifecycle state, derived from the Workbench fields (there is no
    /// second copy of `hidden`/`quitting`).
    pub(super) fn lifecycle_state(&self) -> LifecycleState {
        let phase = if self.quitting {
            LifecyclePhase::Quitting
        } else if self.hidden {
            LifecyclePhase::Hidden
        } else {
            LifecyclePhase::Visible
        };
        LifecycleState::new(self.tray.is_some()).with_phase(phase)
    }

    /// Release the press-and-hold test before leaving the window behind, and
    /// leave an audit line in the session log (the OFF itself lands later, in
    /// the worker, after it observes the released sender).
    pub(super) fn release_relay_for_close(&mut self, cx: &mut Context<Self>) {
        let Some((dut_id, channel)) = self
            .relay_press
            .as_ref()
            .map(|press| (press.dut_id.clone(), press.channel))
        else {
            return;
        };
        let label =
            crate::i18n::message("trayRelayReleaseMark", &[("channel", &channel.to_string())]);
        let _ = self.device.log_mark(&dut_id, &label);
        self.release_relay(cx);
    }

    /// Final hide step. `TrayHandle::hide_main` adds the icon *before* hiding
    /// the window and reports the real `NIM_ADD` result, so a `false` here
    /// means nothing was hidden yet; the window stays visible and an error is
    /// surfaced. A hidden window without a tray icon would be unreachable
    /// (decision 6).
    pub(super) fn finish_hide_to_tray(&mut self, cx: &mut Context<Self>) {
        if self.tray.is_none() {
            return;
        }
        let hidden_ok = {
            let Some(tray) = self.tray.as_ref() else {
                return;
            };
            tray.hide_main()
        };
        if !hidden_ok {
            // Best effort: drop a partially added icon so a later hide starts
            // from a clean desired state.
            if let Some(tray) = self.tray.as_ref() {
                let _ = tray.set_icon_visible(false);
            }
            self.hidden = false;
            self.notify_toast(crate::i18n::label("trayHideFailed"));
            cx.notify();
            return;
        }
        self.hidden = true;
        // Observability for the one-frame lag: the close plan removes the
        // float window before hiding, but its `on_window_closed` observer
        // (which clears `float_window` and returns the placement to Docked)
        // runs deferred. A `true` here is expected and resolves next frame.
        if self.float_window.is_some() {
            eprintln!("[tray] hidden; HDMI float window close is pending");
        }
        if let Ok(mut center) = self.alerts.lock() {
            center.set_merge_duplicates(true);
        }
        self.alert_router.arm();
        // Force the next tick to evaluate even if the generation/pending key
        // did not change (a re-hide of an already-hidden window).
        self.alert_eval_key = None;
        cx.notify();
    }

    /// Restore the main window: show first, then activate (activate only
    /// restores an iconic window), then drop the hidden-state wiring.
    pub(super) fn show_from_tray(&mut self, cx: &mut Context<Self>) {
        if let Some(tray) = self.tray.as_ref() {
            let _ = tray.show_main();
            let _ = tray.set_icon_visible(false);
            tray.set_alert(false);
        }
        if let Some(main) = self.main_window {
            let _ = main.update(cx, |_, window, _| window.activate_window());
        }
        self.hidden = false;
        self.alert_router.set_flash_started(None);
        if let Ok(mut center) = self.alerts.lock() {
            center.set_merge_duplicates(false);
        }
        cx.notify();
    }

    /// `TaskbarCreated` replay failed: never keep a hidden window without an
    /// icon — restore it and report the failure in-app.
    pub(super) fn recover_from_icon_loss(&mut self, cx: &mut Context<Self>) {
        self.show_from_tray(cx);
        self.notify_toast(crate::i18n::label("trayIconFailed"));
    }

    /// Tray menu "Quit": release the relay first (and wait for its OFF), then
    /// tear the runtime down and remove the icon. Bounded by design: the UI
    /// thread budget for the relay join is 3 s.
    ///
    /// The order comes from `tray_lifecycle::on_event(QuitRequested)`; the
    /// match below executes it (the shutdown order is pinned here by the
    /// tray-contract test, so it stays inline instead of hiding in a helper).
    pub(crate) fn begin_quit(&mut self, cx: &mut Context<Self>) {
        if self.boot_learning_dut.is_some() {
            self.notify_toast(i18n::label("bootLearningRunning"));
            cx.notify();
            return;
        }
        if self.quitting {
            return;
        }
        let transition = duthub_desktop::tray_lifecycle::on_event(
            self.lifecycle_state(),
            LifecycleEvent::QuitRequested,
        );
        self.quitting = true;
        self.hidden = false;
        for action in transition.actions {
            match action {
                LifecycleAction::ReleaseRelayAndJoin => self.release_relay_and_join(cx),
                LifecycleAction::CloseFloat => self.close_float_window(cx),
                LifecycleAction::StopPreview => self.release_monitor(None, cx),
                LifecycleAction::ShutdownRuntime => {
                    let sessions = std::mem::take(&mut self.sessions);
                    for session in sessions {
                        let _ = self.device.close_session(&session.dut_id);
                    }
                    self.active_session = 0;
                    if let Some(mut host) = self.agent_host.take() {
                        host.stop_all();
                    }
                    if let Some(a2a) = self.a2a.as_mut() {
                        a2a.shutdown();
                    }
                    self.mcp_runtime.shutdown();
                    self.relay_watch_quit.store(true, Ordering::Relaxed);
                }
                LifecycleAction::RemoveTray => {
                    if let Some(tray) = self.tray.take() {
                        tray.remove();
                    }
                }
                LifecycleAction::QuitProcess => cx.quit(),
                other => debug_assert!(false, "unexpected action in the quit plan: {other:?}"),
            }
        }
    }

    /// Signal OFF and wait up to 3 s for the hold-test worker, which performs
    /// the actual `power(off)` after observing the released sender. A timeout
    /// is logged honestly and never re-issues ON; the worker stays owned by
    /// `RelayPress`, whose `Drop` joins it on the way out.
    pub(super) fn release_relay_and_join(&mut self, cx: &mut Context<Self>) {
        // Same audit trail as the hide path: the mark lands before the worker
        // observes the release and writes OFF.
        self.release_relay_for_close(cx);
        let Some(dut_id) = self.relay_press.as_ref().map(|press| press.dut_id.clone()) else {
            return;
        };
        let finished = {
            let Some(worker) = self
                .relay_press
                .as_ref()
                .and_then(|press| press.worker.as_ref())
            else {
                return;
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !worker.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            worker.is_finished()
        };
        if !finished {
            let _ = self
                .device
                .log_mark(&dut_id, crate::i18n::label("trayRelayOffUnconfirmed"));
            return;
        }
        let Some(mut press) = self.relay_press.take() else {
            return;
        };
        let Some(worker) = press.worker.take() else {
            return;
        };
        let result = worker
            .join()
            .unwrap_or_else(|_| Err(crate::i18n::label("relayWorkerFailed").into()));
        if let Err(error) = result {
            let _ = self.device.log_mark(&press.dut_id, &error);
        }
    }

    /// 50 ms tick: consume tray events. The coordinator maps each event to its
    /// ordered actions: `Show` restores, `Quit` runs the full shutdown,
    /// `IconLost` recovers from a failed icon replay.
    pub(super) fn drain_tray_events(&mut self, cx: &mut Context<Self>) -> bool {
        let events = self
            .tray
            .as_ref()
            .map(duthub_desktop::tray::TrayHandle::drain_events)
            .unwrap_or_default();
        let changed = !events.is_empty();
        for event in events {
            let event = match event {
                duthub_desktop::tray::TrayEvent::Show => LifecycleEvent::ShowRequested,
                duthub_desktop::tray::TrayEvent::Quit => LifecycleEvent::QuitRequested,
                duthub_desktop::tray::TrayEvent::IconLost => LifecycleEvent::TaskbarLost,
            };
            if matches!(event, LifecycleEvent::QuitRequested) {
                self.begin_quit(cx);
                continue;
            }
            let transition =
                duthub_desktop::tray_lifecycle::on_event(self.lifecycle_state(), event);
            for action in transition.actions {
                match action {
                    LifecycleAction::ShowMain => self.show_from_tray(cx),
                    LifecycleAction::RecoverFromIconLoss => self.recover_from_icon_loss(cx),
                    other => debug_assert!(false, "unexpected tray event action: {other:?}"),
                }
            }
        }
        changed
    }

    /// 50 ms tick: align the relay watch set with `self.duts.file` (idempotent
    /// and lock-only, so it is safe here — never enumerates persistence
    /// points). Only boards bound by a saved DUT and with an assigned channel
    /// participate; the service re-checks driver/host/node. The board→channel
    /// choice comes from the same helper the monitor tests cover, so a board
    /// whose binding has no channel assigned is never probed (no periodic
    /// "offline" noise for an unconfigured board).
    pub(super) fn sync_relay_watch(&mut self) -> bool {
        let channels = duthub_device::relay_watch::board_probe_channels(
            &self.duts.file.relay_boards,
            &self.duts.file.duts,
        );
        let mut watchable = Vec::new();
        let mut signature = Vec::new();
        for board in &self.duts.file.relay_boards {
            let Some(channel) = channels
                .iter()
                .find(|(board_id, _)| board_id == &board.id)
                .map(|(_, channel)| *channel)
            else {
                continue;
            };
            if !duthub_device::relay_watch::is_watchable_board(board) {
                continue;
            }
            watchable.push(board.clone());
            signature.push((
                board.id.clone(),
                board.node.clone(),
                channel,
                board.driver.clone(),
            ));
        }
        if signature == self.relay_watch_signature {
            return false;
        }
        self.relay_watch_signature = signature;
        self.relay_probe.set_channels(&channels);
        self.relay_watch.sync_boards(&watchable);
        false
    }

    /// 50 ms tick: relay board events → AlertCenter (offline) or toast
    /// (recovered / unobservable). The probe thread never touches either.
    pub(super) fn drain_relay_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.relay_events.try_recv() {
            changed = true;
            match event.state {
                duthub_device::relay_watch::RelayBoardState::Offline => {
                    let (title, detail) =
                        crate::alerts::relay_board_alert_texts(&event.name, &event.detail);
                    if let Ok(mut center) = self.alerts.lock() {
                        let _ = center.report_checked_for(
                            &event.board_id,
                            AlertSource::Relay,
                            crate::alerts::AlertLevel::Warning,
                            title,
                            detail,
                        );
                    }
                }
                duthub_device::relay_watch::RelayBoardState::Recovered => {
                    self.notify_toast(crate::i18n::message(
                        "relayRecoveredToast",
                        &[("board", &event.name)],
                    ));
                }
                duthub_device::relay_watch::RelayBoardState::Unobservable => {
                    self.notify_toast(crate::i18n::message(
                        "relayUnobservableToast",
                        &[("board", &event.name), ("detail", &event.detail)],
                    ));
                }
                duthub_device::relay_watch::RelayBoardState::Online => {}
            }
        }
        changed
    }

    /// 50 ms tick: hidden-state alert presentation (balloon + icon flash).
    /// The router is the only owner of that state; `latest()` is read-only.
    pub(super) fn alert_present_tick(&mut self) -> bool {
        if self.tray.is_none() {
            return false;
        }
        let now = crate::alerts::now_ms();
        // A running blink must stop as soon as the user turns the system
        // animation preference off. The tray timer only reads the preference
        // when an episode starts, and `FlashStop::AnimationsDisabled` has no
        // other production trigger; this polls only while a blink is active
        // (bounded by `FLASH_MAX_MS`).
        if self.alert_router.flash_started().is_some()
            && !duthub_desktop::tray::animations_enabled()
        {
            return self.stop_alert_flash();
        }
        let (generation, latest, pending) = {
            let Ok(center) = self.alerts.lock() else {
                return false;
            };
            (
                center.generation(),
                center.latest(),
                center.pending_len() as u64,
            )
        };
        // System notification state is only queried when there is something
        // new to evaluate (or a blink episode must be stopped on timeout);
        // polling `SHQueryUserNotificationState` at tick rate would be waste.
        let eval_key = (generation, self.hidden, pending);
        let key_changed = self.alert_eval_key != Some(eval_key);
        let flash_expired = self.alert_router.flash_started().is_some_and(|started| {
            now.saturating_sub(started) >= duthub_desktop::tray_ui::FLASH_MAX_MS
        });
        if !key_changed && !flash_expired {
            return false;
        }
        self.alert_eval_key = Some(eval_key);
        let env = HiddenAlertEnv {
            visible: !self.hidden,
            dialog_open: self.dialog_open,
            pending,
            animations: duthub_desktop::tray::animations_enabled(),
            gate: duthub_desktop::tray::notification_gate(),
            critical: latest
                .as_ref()
                .is_some_and(|summary| summary.source != AlertSource::AgentNotice),
        };
        match self.alert_router.step(generation, env, now) {
            HiddenAlertAction::None => false,
            HiddenAlertAction::Present {
                presentation,
                count,
            } => {
                // The coordinator expands the presentation into the ordered
                // tray effects (`PresentAlert` then `StartFlash` for `Both`).
                let transition = duthub_desktop::tray_lifecycle::on_event(
                    self.lifecycle_state(),
                    LifecycleEvent::AlertArrived {
                        presentation,
                        count,
                    },
                );
                let mut changed = false;
                for action in transition.actions {
                    match action {
                        LifecycleAction::PresentAlert {
                            presentation,
                            count,
                        } => changed |= self.present_hidden_alert(presentation, count),
                        LifecycleAction::StartFlash => changed |= self.start_alert_flash(now),
                        other => debug_assert!(false, "unexpected alert action: {other:?}"),
                    }
                }
                changed
            }
            HiddenAlertAction::StopFlash => self.stop_alert_flash(),
        }
    }

    /// Presents the newest queued alert as a tray balloon.
    ///
    /// The visible-window case is rendered by `render_root` next frame and
    /// never reaches this path. The balloon body is the alert detail (the
    /// title only stands in when the detail is empty), capped at 200
    /// characters; the level drives the shell icon.
    pub(super) fn present_hidden_alert(
        &mut self,
        presentation: AlertPresentation,
        count: u64,
    ) -> bool {
        debug_assert!(matches!(
            presentation,
            AlertPresentation::BalloonAndFlash | AlertPresentation::BalloonOnly
        ));
        let Some(summary) = self.alerts.lock().ok().and_then(|center| center.latest()) else {
            return false;
        };
        let body = if summary.detail.trim().is_empty() {
            summary.title
        } else {
            summary.detail
        };
        let body: String = body.chars().take(200).collect();
        let title = crate::i18n::message("trayAlertTitle", &[("count", &count.to_string())]);
        let level = match summary.level {
            crate::alerts::AlertLevel::Info => duthub_desktop::tray::TrayNotifyLevel::Info,
            crate::alerts::AlertLevel::Warning => duthub_desktop::tray::TrayNotifyLevel::Warning,
            crate::alerts::AlertLevel::Error => duthub_desktop::tray::TrayNotifyLevel::Error,
        };
        // Critical (device-level) alerts must still show during Windows quiet
        // time; the coordinator builds the request with the quiet-time policy.
        let critical = summary.source != AlertSource::AgentNotice;
        let notification =
            duthub_desktop::tray_lifecycle::balloon_notification(title, body, level, critical);
        if let Some(tray) = self.tray.as_ref() {
            tray.notify_balloon(&notification);
        }
        true
    }

    /// Starts (or restarts) the blinking episode and records its start.
    pub(super) fn start_alert_flash(&mut self, now: u64) -> bool {
        if let Some(tray) = self.tray.as_ref() {
            duthub_desktop::tray_lifecycle::run_tray_actions(tray, &[LifecycleAction::StartFlash]);
        }
        self.alert_router.set_flash_started(Some(now));
        true
    }

    /// Stops the blinking episode and restores the normal icon.
    pub(super) fn stop_alert_flash(&mut self) -> bool {
        if let Some(tray) = self.tray.as_ref() {
            duthub_desktop::tray_lifecycle::run_tray_actions(tray, &[LifecycleAction::StopFlash]);
        }
        self.alert_router.set_flash_started(None);
        true
    }

    /// 50 ms tick: refresh the MCP snapshot on a 250 ms cadence. `refresh_snapshot`
    /// clones each session's log tail, so running it per frame (the old
    /// placement) was the dominant hidden-state cost.
    pub(super) fn snapshot_tick(&mut self) -> bool {
        let now = crate::alerts::now_ms();
        if now < self.snapshot_due_ms {
            return false;
        }
        self.snapshot_due_ms = now.saturating_add(SNAPSHOT_INTERVAL_MS);
        self.refresh_snapshot();
        false
    }

    /// Whether `dut_id` identifies a DUT the user can edit (saved or the
    /// in-progress draft). Everything else is a non-DUT alert subject and its
    /// "mute this type" must go to the global suppression list.
    pub(super) fn is_saved_dut_id(&self, dut_id: &str) -> bool {
        self.duts.file.duts.iter().any(|dut| dut.id == dut_id)
            || self
                .duts
                .new_connection
                .as_ref()
                .is_some_and(|draft| draft.dut.id == dut_id)
    }

    /// "Mute this type" for a non-DUT subject (relay board, generic-listener
    /// Agent notice): persist to `settings.alerts.suppressed_sources` and
    /// suppress in the shared center.
    pub fn suppress_alert_source_globally(&mut self, source: AlertSource) {
        if let Ok(mut center) = self.alerts.lock() {
            if !center.is_suppressed(source) {
                center.suppress(source);
            }
            center.drop_source(source);
        }
        if !self
            .settings
            .alerts
            .suppressed_sources
            .iter()
            .any(|key| key == source.key())
        {
            self.settings
                .alerts
                .suppressed_sources
                .push(source.key().to_owned());
            self.persist_settings();
        }
    }
}
