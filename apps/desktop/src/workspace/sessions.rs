use super::{
    AlertSource, ChunkStyle, In, InstallFinishedOutcome, LineKind, LinkState, LogLine, Ordering,
    SessionView, SharedString, ToolPurpose, UiEvent, Workbench, agent, broadcast, i18n,
    parse_serial_open_baud, store, tool_failure_label,
};

impl Workbench {
    // ------------------------------------------------------------------
    // Tool downloads (settings page "Tools download"): start / cancel / drain
    // ------------------------------------------------------------------

    /// Start one tool install (download / redownload / update = force). Tools
    /// download in parallel — only starting the same tool twice is refused.
    /// Results and progress reach the UI through `drain_tool_installs`.
    pub(crate) fn start_tool_install(
        &mut self,
        spec: &'static duthub_tooldown::ToolSpec,
        purpose: duthub_desktop::tool_ui::ToolPurpose,
    ) {
        let Some(tools_root) = duthub_desktop::tool_resolver::config_tools_root() else {
            self.notify_toast(i18n::label("toolRootUnavailable").to_string());
            return;
        };
        let proxy = duthub_desktop::tool_resolver::proxy_config(&self.settings.tools.download);
        // AlreadyRunning means this exact tool is in flight (its row shows the
        // cancel action), so a duplicate start is simply ignored.
        let url = self.tool_download_url(spec).to_owned();
        let _ = self
            .tool_installs
            .start(spec, url, tools_root, proxy, purpose);
    }

    pub(super) fn tool_download_url(&self, spec: &duthub_tooldown::ToolSpec) -> &str {
        self.settings
            .tools
            .download
            .urls
            .get(spec.id)
            .map_or(spec.url, String::as_str)
    }

    pub(crate) fn save_tool_download_url(&mut self, spec: &'static duthub_tooldown::ToolSpec) {
        let id = In::ToolDownloadUrl(spec.id);
        let draft = self.input_value(id);
        let value = draft.trim();
        let mut candidate = self.settings.clone();
        if value.is_empty() || value == spec.url {
            candidate.tools.download.urls.remove(spec.id);
        } else if let Ok(url) = duthub_tooldown::validate_download_url(spec, value) {
            candidate
                .tools
                .download
                .urls
                .insert(spec.id.to_owned(), url.to_string());
        } else {
            self.notify_toast(i18n::label(if spec.id == "goose" {
                "toolGooseUrlInvalid"
            } else {
                "toolFailDownloadUrl"
            }));
            return;
        }
        if let Err(error) = store::save_settings_secured(&self.data_dir, &candidate) {
            self.notify_toast(format!("{} {error}", i18n::label("saveFailed")));
            return;
        }
        self.settings = candidate;
        self.drafts.remove(&id);
        self.notify_toast(i18n::label("toolDownloadUrlSaved"));
    }

    /// Cancel one tool's running install: sets its cancel flag; the kernel
    /// observes it between chunks and returns `Cancelled`, leaving any existing
    /// install untouched (the result surfaces as a toast via drain).
    pub(crate) fn cancel_tool_install(&mut self, tool: &'static str) {
        self.tool_installs.cancel_active(tool);
    }

    /// "Save proxy settings": in `custom` mode run the advisory URL validation
    /// first (clear reason for the user), then the download kernel's
    /// authoritative `build_client` check (offline); only when both pass is the
    /// policy (mode + URL) written and persisted. Mode is staged by the Select
    /// as a draft, so nothing half-applies before this button. The URL field is
    /// only rendered in `custom` mode, so the mode is the single source of
    /// truth here: a stored URL never re-promotes `env`/`off`, and saving those
    /// modes clears the URL so a later launch's
    /// `duthub_core::store::migrate_download_proxy` cannot flip the explicit
    /// choice back to `custom`. On failure the drafts survive (pit #41: drafts
    /// are removed only after a successful save).
    pub(crate) fn save_tool_proxy(&mut self) {
        use duthub_core::model::DownloadProxyMode;
        let mode = self
            .tool_proxy_mode_draft
            .unwrap_or(self.settings.tools.download.proxy.mode);
        let url = if mode == DownloadProxyMode::Custom {
            self.input_value(In::ProxyUrl).trim().to_string()
        } else {
            String::new()
        };
        if mode == DownloadProxyMode::Custom {
            if let Err(problem) = duthub_desktop::tool_ui::validate_custom_proxy_url(&url) {
                self.notify_toast(i18n::message(
                    "proxyUrlInvalid",
                    &[("reason", problem.reason().as_str())],
                ));
                return;
            }
            if let Err(error) = duthub_tooldown::ProxyConfig::Custom(url.clone()).build_client() {
                // Kernel build error goes through the same localized template
                // (label() would leave the {reason} placeholder unsubstituted).
                self.notify_toast(i18n::message(
                    "proxyUrlInvalid",
                    &[("reason", error.to_string().as_str())],
                ));
                return;
            }
        }
        self.settings.tools.download.proxy.mode = mode;
        self.settings.tools.download.proxy.url.clone_from(&url);
        self.tool_proxy_mode_draft = None;
        self.drafts.remove(&In::ProxyUrl);
        self.persist_settings();
        self.refresh_tool_rows();
        // Say which policy is now in effect: a custom URL that was just
        // promoted from `env` must be visible as applied, not implied.
        let effective = match mode {
            DownloadProxyMode::Off => i18n::label("proxyModeOff").to_string(),
            DownloadProxyMode::Env => i18n::label("proxyModeEnv").to_string(),
            DownloadProxyMode::Custom => {
                format!("{} · {url}", i18n::label("proxyModeCustom"))
            }
        };
        self.notify_toast(format!("{} — {effective}", i18n::label("proxySaved")));
    }

    /// Refresh the settings-page "Tools download" row snapshots (on-disk
    /// facts: manifests / effective paths).
    pub(super) fn refresh_tool_rows(&mut self) {
        let tools_root = duthub_desktop::tool_resolver::config_tools_root();
        let adb = self.settings.tools.adb_executable.clone();
        let ffmpeg = self.settings.tools.capture.ffmpeg.clone();
        // goose has no direct settings key (resolved via the agent registry):
        // pass None.
        let configured = |spec_id: &str| -> Option<String> {
            match spec_id {
                "adb" => Some(adb.clone()),
                "ffmpeg" => Some(ffmpeg.clone()),
                _ => None,
            }
        };
        self.tool_rows = duthub_tooldown::builtin_specs()
            .iter()
            .map(|spec| {
                duthub_desktop::tool_ui::tool_row_snapshot(
                    spec,
                    configured(spec.id).as_deref(),
                    tools_root.as_deref(),
                )
            })
            .collect();
    }

    /// Drain finished tool installs and sync the progress display; report
    /// whether anything changed so the tick can request a redraw.
    pub(super) fn drain_tool_installs(&mut self) -> bool {
        let mut changed = false;
        let mut finished_any = false;
        for finished in self.tool_installs.drain() {
            let purpose = finished.purpose;
            let message = match &finished.outcome {
                Ok(InstallFinishedOutcome::Installed { .. }) => {
                    let key = if purpose == ToolPurpose::Update {
                        "toolUpdateDone"
                    } else {
                        "toolInstallDone"
                    };
                    format!("{} {}", i18n::label(key), finished.tool)
                }
                Ok(InstallFinishedOutcome::AlreadyInstalled { .. }) => {
                    format!("{} {}", i18n::label("toolAlreadyInstalled"), finished.tool)
                }
                Ok(InstallFinishedOutcome::Cancelled) => {
                    let key = if purpose == ToolPurpose::Update {
                        "toolUpdateCancelled"
                    } else {
                        "toolDownloadCancelled"
                    };
                    format!("{} {}", i18n::label(key), finished.tool)
                }
                Err(failure) => {
                    let key = if purpose == ToolPurpose::Update {
                        "toolUpdateFailed"
                    } else {
                        "toolInstallFailed"
                    };
                    format!(
                        "{} {} — {}{}",
                        i18n::label(key),
                        finished.tool,
                        i18n::label(tool_failure_label(failure.kind)),
                        if failure.detail.is_empty() {
                            String::new()
                        } else {
                            i18n::message(
                                "toolFailureDetail",
                                &[("detail", failure.detail.as_str())],
                            )
                        }
                    )
                }
            };
            if finished.tool == "goose" {
                self.agent.goose_install = None;
            }
            self.tool_last_results.insert(finished.tool, finished);
            self.notify_toast(message);
            changed = true;
            finished_any = true;
        }
        // Progress is sampled per tick: the worker threads write the map on
        // every chunk, the UI redraws only when a displayed value changes.
        let live = self.tool_installs.progress_snapshot();
        if live != self.tool_progress_view {
            self.tool_progress_view = live;
            changed = true;
        }
        // Only rescan disk manifests/PATH when an install actually finishes —
        // never at 20 Hz while download progress ticks.
        if finished_any {
            self.refresh_tool_rows();
        }
        changed
    }

    // ------------------------------------------------------------------
    // Device event absorption
    // ------------------------------------------------------------------

    pub(super) fn ensure_session(&mut self, session_id: &str, dut_id: &str) -> usize {
        if let Some(index) = self
            .sessions
            .iter()
            .position(|s| s.session_id == session_id)
        {
            return index;
        }
        let source = self.device.session_source(dut_id).unwrap_or("device");
        self.sessions.push(SessionView::new(
            dut_id.to_string(),
            session_id.to_string(),
            format!("{dut_id} · {source}"),
            source,
            0,
        ));
        // A new session requires a termResize (grid size is reset per session).
        self.term_grid = None;
        self.sessions.len() - 1
    }

    pub(super) fn absorb_envelope(&mut self, env: duthub_core::ipc::EventEnvelope) {
        let Some(session_id) = env.session_id.clone() else {
            return;
        };
        let Some(dut_id) = env.dut_id.clone() else {
            return;
        };
        if !self
            .sessions
            .iter()
            .any(|view| view.session_id == session_id)
            && self.device.session_source(&dut_id).is_none()
        {
            return;
        }
        let index = self.ensure_session(&session_id, &dut_id);
        let view = &mut self.sessions[index];
        // Failed to enqueue at the beginning. Payload (Requirement ⑤ Access Point ②): Record it
        // during the borrowing period, and then enqueue it later.
        let mut serial_open_failure: Option<(String, String)> = None;
        let mut serial_reconnected = false;
        // Route a screen snapshot separately and move its `Arc` into `SessionView` without cloning
        // the contents. The drain and first-open failure handlers have no work for this event.
        if let UiEvent::SerialScreen { screen } = env.event {
            // Alert on the edge into `Garbled` only: the snapshot arrives every
            // frame, so reporting on every garbled frame would rely on the 30s
            // dedup alone and the tab dot must not queue anything.
            let previous = view.last_garbled_level;
            let current = screen.garbled.level;
            let permille = screen.garbled.control_permille;
            view.absorb_serial_screen(screen, env.seq);
            if crate::garble_ui::alert_on_transition(previous, current) {
                self.alerts.lock().unwrap().report_for(
                    &view.dut_id,
                    AlertSource::SerialGarbled,
                    crate::i18n::label("serialGarbledAlertTitle"),
                    crate::i18n::message(
                        "serialGarbledAlertDetail",
                        &[("percent", &crate::garble_ui::control_percent_text(permille))],
                    ),
                );
            }
            return;
        }
        match &env.event {
            UiEvent::SerialChunk { text, style } => {
                let kind = match style {
                    ChunkStyle::Normal => LineKind::Normal,
                    ChunkStyle::Dim => LineKind::Dim,
                    ChunkStyle::Good => LineKind::Good,
                    ChunkStyle::Prompt => LineKind::Prompt,
                    ChunkStyle::Error => LineKind::Error,
                    ChunkStyle::Mark => LineKind::Mark,
                };
                if self.rec_on {
                    self.rec_buffer.push(text.clone().clone());
                }
                let opened_baud = if view.source == "device" && *style == ChunkStyle::Dim {
                    parse_serial_open_baud(text)
                } else {
                    None
                };
                // Link status derivation (only applicable to device sessions): The driver for the
                // "[serial]" row contains the status information.
                if view.source == "device" {
                    if *style == ChunkStyle::Normal {
                        view.crash_line_tail.push_str(text);
                        if let Some(end) = view.crash_line_tail.rfind('\n') {
                            let remainder = view.crash_line_tail.split_off(end + 1);
                            if let Some(line) = agent::serial_crash_evidence(&view.crash_line_tail)
                            {
                                self.alerts.lock().unwrap().report_for(
                                    &view.dut_id,
                                    AlertSource::Agent,
                                    crate::i18n::label("serialCrash"),
                                    format!("{}\n{line}", view.title),
                                );
                            }
                            view.crash_line_tail = remainder;
                        }
                        if view.crash_line_tail.len() > 8192 {
                            view.crash_line_tail.clear();
                        }
                    }
                    if let Some(baud) = opened_baud {
                        view.link = LinkState::Connected;
                        view.reconnect_attempts = 0;
                        view.baud = baud;
                        serial_reconnected = true;
                    } else if text.starts_with("[serial] ") && text.contains("重连") {
                        view.link = LinkState::Reconnecting;
                        view.reconnect_attempts += 1;
                    } else if let Some(target) = crate::alerts::parse_serial_open_failure(text) {
                        // Failure to enqueue: The driver only reported an error once and ended the
                        // session task (pit #7). In other words, the "task died" – it should be
                        // redirected to a pop-up reminder (requirement ⑤, access point ②). The view
                        // is still borrowed; save the payload first, and enqueue it after a match
                        // occurs.
                        serial_open_failure = Some((target, text.clone()));
                    }
                }
                // Annotation: Device sessions (PuTTY mode): Device output is rendered via the
                // emulator's grid (SerialScreen). SerialChunks only contain annotation lines
                // ([serial] signaling / [mark] / errors); Demo sessions maintain the original
                // full-line rendering.
                if view.should_queue_chunk_line(*style, opened_baud.is_some()) {
                    view.lines.push(LogLine {
                        text: SharedString::from(text.clone()),
                        kind,
                    });
                    view.output_seq = view.output_seq.wrapping_add(1);
                }
                view.last_seq = env.seq;
                self.core_lines.fetch_add(1, Ordering::Relaxed);
            }
            UiEvent::SerialDisconnected { reason } => {
                // After a session disconnects, the device automatically enters backoff mode for
                // reconnection → The status bar shows "Reconnecting".
                if view.source == "device" {
                    view.crash_line_tail.clear();
                    view.link = LinkState::Reconnecting;
                }
                view.lines.push(LogLine {
                    text: SharedString::from(i18n::message(
                        "sessionDisconnectedNote",
                        &[("reason", reason)],
                    )),
                    kind: LineKind::Dim,
                });
                view.output_seq = view.output_seq.wrapping_add(1);
                view.last_seq = env.seq;
            }
            _ => {}
        }
        if self.sessions[index].lines.len() > 5000 {
            let drain = self.sessions[index].lines.len() - 4000;
            self.sessions[index].lines.drain(..drain);
        }
        // Initial connection failed → Pop-up reminder in the center of the interface (attempting to
        // reconnect via a separate window).
        if serial_reconnected {
            self.resolve_alert_for(&dut_id, AlertSource::Serial);
        }
        if let Some((target, line)) = serial_open_failure {
            let dut_label = self
                .duts
                .file
                .duts
                .iter()
                .find(|d| d.id == dut_id)
                .map_or_else(|| dut_id.clone(), |d| d.name.clone());
            self.report_serial_session_failure(
                &dut_id,
                &dut_label,
                &crate::i18n::message("alertSerialOpenFailed", &[("target", &target)]),
                &line,
            );
        }
    }

    /// Access Point ② (Serial): Session initialization failed / Task terminated → Pop-up alert
    /// appears in the center.
    /// The "detail" field contains the original driver settings (port number, baud rate, reason for
    /// failure), which helps in manual troubleshooting.
    pub(super) fn report_serial_session_failure(
        &mut self,
        dut_id: &str,
        dut_label: &str,
        title: &str,
        detail: &str,
    ) -> bool {
        self.alerts.lock().unwrap().report_for(
            dut_id,
            AlertSource::Serial,
            title.to_string(),
            crate::i18n::message(
                "alertSerialDetail",
                &[("dut", dut_label), ("detail", detail)],
            ),
        )
    }

    pub(super) fn absorb_device_events(
        &mut self,
        first: Result<duthub_core::ipc::EventEnvelope, broadcast::error::TryRecvError>,
        rx: &mut broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
    ) -> usize {
        let _span = tracing::info_span!("serial.ui_apply").entered();
        let mut handled = 0usize;
        let events = std::iter::once(first).chain(std::iter::from_fn(|| Some(rx.try_recv())));
        for event in events.take(64) {
            match event {
                Ok(env) => {
                    self.absorb_envelope(env);
                    handled += 1;
                }
                Err(broadcast::error::TryRecvError::Lagged(lagged)) => {
                    let views: Vec<(String, u64)> = self
                        .sessions
                        .iter()
                        .map(|s| (s.session_id.clone(), s.last_seq))
                        .collect();
                    for (session_id, since) in views {
                        for env in self.device.read_log(
                            &session_id,
                            since,
                            usize::try_from(lagged).unwrap_or(usize::MAX),
                        ) {
                            self.absorb_envelope(env);
                            handled += 1;
                            if handled >= 256 {
                                break;
                            }
                        }
                    }
                    break;
                }
                Err(_) => break,
            }
        }
        handled
    }

    // ------------------------------------------------------------------
    // Session Heartbeat (Requirement ⑦): Timed Driver + Alarm Drain
    // ------------------------------------------------------------------

    /// Heartbeat engine drives once (50ms tick calls): issuing commands when deadlines approach,
    /// responding within the window, and generating alerts when the connection threshold is reached
    /// (toast notifications + redirecting to pop-up alerts, requirement ⑤ connects to point ①). It
    /// returns whether there are any changes in status (used to determine repainting during
    /// rendering loops).
    pub fn heartbeat_tick(&mut self) -> bool {
        let changed = self.heartbeat.tick();
        for alert in self.heartbeat.drain_alerts() {
            self.notify_toast(format!("⚠ {}", alert.message));
            let label = self
                .duts
                .file
                .duts
                .iter()
                .find(|d| d.id == alert.dut_id)
                .map_or_else(|| alert.dut_id.clone(), |d| d.name.clone());
            let (title, detail) = crate::alerts::heartbeat_alert_texts(&label, &alert.message);
            self.alerts.lock().unwrap().report_for(
                &alert.dut_id,
                AlertSource::Heartbeat,
                title,
                detail,
            );
        }
        changed
    }
}
