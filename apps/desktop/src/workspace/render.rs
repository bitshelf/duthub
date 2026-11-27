use super::{Workbench, Window, Context, IntoElement, AppContext, Ordering, WindowExt, FluentBuilder, usb_debug_enabled, InteractiveElement, Styled, div, px, ParentElement, ViewMode, PaneDivider, StatefulInteractiveElement, icon_btn, theme, MouseMoveEvent, MouseButton, MouseDownEvent, ConfigPage, Notification, AlertItem, Palette, AlertSource, SharedString, icon, img, LinkState, Disableable, btn, i18n, BtnVariant, WindowControlArea, CommandState, Command, CommandItem, AnyElement, GarbleLevel, Div, kit_icon, In, DEFAULT_TERMINAL_FONT_SIZE, terminal_grid_for, terminal_selection_bg, KeyDownEvent, arrow_escape_prefix, typed_char, LogLine, Pixels, LineKind, Arc, SessionView, ModalKind, HashMap, store, archive_safe_name, boot_reference_path, demo_dut_template, render_connection, render_flash_tools, render_usb_debug, render_settings, ModalState, QuickCommand};

impl Workbench {
    // ------------------------------------------------------------------
    // 渲染
    // ------------------------------------------------------------------

    pub fn render_root(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _frame = tracing::info_span!(
            "ui.frame",
            frames = self.frames.load(std::sync::atomic::Ordering::Relaxed)
        )
        .entered();
        self.frames.fetch_add(1, Ordering::Relaxed);
        // Main window handle + close gate (§6.4): the float window must be
        // closed before the main window is allowed to go away.
        if self.main_window.is_none() {
            self.main_window = Some(window.window_handle());
        }
        if !self.main_close_hook_installed {
            self.main_close_hook_installed = true;
            let weak = cx.weak_entity();
            window.on_window_should_close(cx, move |_window, cx| {
                weak.update(cx, super::Workbench::prepare_main_window_close)
                    .unwrap_or(true)
            });
        }
        // Tray icon host: installed on the first render because this is the
        // only place a `&mut Window` exists. `--self-test` never installs one,
        // and a failure latches `tray_failed` so close quits for real.
        if !self.agent_self_test && self.tray.is_none() && !self.tray_failed {
            match duthub_desktop::tray::install(
                window,
                duthub_desktop::tray::TrayOptions {
                    tooltip: crate::i18n::label("trayTooltip").to_string(),
                    show_label: crate::i18n::label("trayShowMain").to_string(),
                    quit_label: crate::i18n::label("trayQuit").to_string(),
                },
            ) {
                Ok(tray) => {
                    self.tray_tooltip = crate::i18n::label("trayTooltip").to_string();
                    self.tray = Some(tray);
                }
                Err(error) => {
                    self.tray_failed = true;
                    eprintln!("[tray] install failed: {error}");
                    self.notify_toast(crate::i18n::label("trayUnavailable"));
                }
            }
        }
        // Follow a language switch with the icon tooltip. The context-menu
        // labels are captured in `TrayOptions` at install time and have no
        // update entry point (see the caveats in the delivery report).
        let tooltip = crate::i18n::label("trayTooltip");
        if self.tray_tooltip != tooltip {
            self.tray_tooltip = tooltip.to_string();
            if let Some(tray) = self.tray.as_ref() {
                tray.set_tooltip(tooltip);
            }
        }
        // Read the dialog state here (the only place with `&mut Window`) so
        // the tick can consult it one frame later.
        self.dialog_open = window.has_active_dialog(cx);
        if let Some(dut_id) = self.connection_dut_scope.as_deref()
            && self.active_serial_dut() != Some(dut_id)
        {
            self.connection_dut_scope = None;
            self.connection_open = false;
            self.open_dd = None;
        }
        let usb_debug_view = if self.connection_open {
            self.duts
                .selected()
                .filter(|dut| usb_debug_enabled(&dut.adb))
                .map(|dut| dut.adb.tool == "hdc")
        } else {
            None
        };
        if self.usb_debug_view != usb_debug_view {
            self.usb_debug_view = usb_debug_view;
            if let Some(hdc) = usb_debug_view {
                self.scan_usb_devices(hdc);
            }
        }
        if self.relay_press.as_ref().is_some_and(|press| {
            !self.connection_open
                || self.overlay.is_some()
                || !window.is_window_active()
                || self
                    .duts
                    .selected()
                    .is_none_or(|dut| dut.id != press.dut_id)
        }) {
            self.release_relay(cx);
        }
        self.sync_monitor_panel(window, cx);
        let p = self.palette();
        self.p = p;

        let panel_open = self.side_panel.visible.is_some();
        let creating = self.connection_open && self.duts.editing_new_connection;
        let (connection_width, side_width) = self.pane_widths(window.viewport_size().width);
        let mut split = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .on_drag_move(cx.listener(|this, event, _, cx| this.drag_pane(event, cx)))
            .when(creating, |el| {
                let width = (window.viewport_size().width - px(36.)).clamp(px(240.), px(690.));
                el.child(
                    div()
                        .flex_1()
                        .flex()
                        .justify_center()
                        .py(px(14.))
                        .child(self.render_connection_dock(width, p, window, cx)),
                )
            })
            .when(self.connection_open && !creating, |el| {
                el.child(self.render_connection_dock(connection_width, p, window, cx))
            })
            .when(self.view == ViewMode::Home && !creating, |el| {
                el.child(self.render_home(p, cx))
            })
            .when(self.view == ViewMode::Serial, |el| {
                el.child(self.render_serial_page(window, cx))
            });
        if panel_open && self.view == ViewMode::Serial {
            split = split.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(side_width)
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .child(self.render_side_content(side_width, p, window, cx))
                    .child(PaneDivider::Tools.handle()),
            );
        } else if self.side_panel.visible.is_some() {
            self.close_side_panel(window, cx);
        }
        let mut bottom = div()
            .flex()
            .items_center()
            .h(px(40.))
            .flex_none()
            .px(px(8.))
            .gap(px(8.))
            .bg(p.rail)
            .border_t_1()
            .border_color(p.line)
            .child(
                icon_btn("bottom-connection", "link", p)
                    .flex_none()
                    .when(self.connection_open, |button| button.bg(p.raised))
                    .tooltip(crate::i18n::label("connectionTitle"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_connection_dock();
                        cx.notify();
                    })),
            );
        bottom = if self.show_command_dock() {
            bottom.child(self.render_command_dock(p, cx))
        } else {
            bottom.child(div().flex_1())
        };
        bottom = bottom.child(self.render_side_strip(p, cx));

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .font_family(theme::UI_FONT)
            .text_color(p.text)
            .bg(p.bg)
            .relative()
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                let left_pressed = ev.pressed_button == Some(MouseButton::Left);
                this.update_terminal_selection(ev.position, left_pressed, window, cx);
                this.update_shell_selection(ev.position, left_pressed, window, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.release_relay(cx);
                    this.finish_terminal_selection(cx);
                    this.finish_shell_selection(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.release_relay(cx);
                    this.finish_terminal_selection(cx);
                    this.finish_shell_selection(cx);
                }),
            )
            // 下拉打开时：底层透明捕获层命中“空白处”点击 → 关闭下拉
            // （下拉列表自身 occlude 在上层，内部交互不受影响）
            .when(self.open_dd.is_some(), |el| {
                el.child(
                    div()
                        .id("dd-backdrop")
                        .absolute()
                        .inset_0()
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            this.open_dd = None;
                            cx.notify();
                        })),
                )
            })
            .child(self.render_titlebar(p, cx))
            .child(split)
            .child(bottom);

        // 浮层：菜单 / 配置壳 / 弹窗 / Toast / 下拉关闭背景
        if self.menu_open {
            // 空白捕获层：点菜单以外任意区域关闭（菜单面板自身 occlude 挡住本层）
            root = root.child(
                div()
                    .id("menu-backdrop")
                    .absolute()
                    .inset_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _win, cx| {
                            this.menu_open = false;
                            cx.notify();
                        }),
                    ),
            );
            root = root.child(self.render_workspace_menu(p, cx));
        }
        if let Some((ctx_x, ctx_y)) = self.terminal_ctx {
            root = root.child(self.render_terminal_ctx_menu(ctx_x, ctx_y, p, cx));
        }
        if self.overlay != Some(ConfigPage::AgentModel) {
            self.agent_token_input = None;
            self.provider_models.close_editor();
        }
        if let Some(page) = self.overlay {
            root = root.child(self.render_config_overlay(page, window, cx));
        }
        // 异常弹窗提醒调度（需求⑤）：仅在没有活动 Dialog 时弹出（不与表单弹窗
        // 叠加，避免 close_dialog 关错层）；「需要人工干预」弹窗禁用了 Esc/遮罩/
        // 右上角 X 关闭，只能经按钮收尾，alert_dialog_open 若因外部关闭残留
        //（如其他代码 close_all_dialogs）由本帧自愈复位。
        // While hidden to the tray or minimized to the taskbar, the dialog stays
        // queued: the tick flashes the taskbar/tray icon instead, and the queue
        // is drained as soon as the window is visible on the desktop again.
        let minimized = self
            .tray
            .as_ref()
            .is_some_and(duthub_desktop::tray::TrayHandle::main_minimized);
        if !self.hidden && !minimized && self.alert_dialog_open && !window.has_active_dialog(cx) {
            self.alert_dialog_open = false;
        }
        if !self.hidden && !minimized && !self.alert_dialog_open && !window.has_active_dialog(cx) {
            let next = self.alerts.lock().unwrap().pop_next();
            if let Some(item) = next {
                self.alert_dialog_open = true;
                self.open_alert_dialog(item, window, cx);
                // Flash the taskbar icon if the window is open on the desktop
                // but currently behind another application (no-op when foreground).
                if let Some(tray) = self.tray.as_ref() {
                    tray.flash_taskbar();
                }
            }
        }
        // kit Notification：渲染帧统一推送（Root 层管理生命周期/动画）
        if self.toasts_omitted > 0 {
            let omitted = std::mem::take(&mut self.toasts_omitted);
            let message = crate::i18n::message("toastOmitted", &[("count", &omitted.to_string())]);
            window.push_notification(Notification::info(message), cx);
        }
        for message in self.pending_toasts.drain(..) {
            window.push_notification(Notification::info(message), cx);
        }
        root
    }

    /// 打开「需要人工干预」弹窗（kit Dialog，需求⑤）：
    /// 标题固定，detail 展示来源与上下文；按钮 知道了 / 忽略此类（按 source
    /// 抑制并落盘）/ 打开面板（仅 Agent 来源）。浮层内 kit Button 渲染异常
    /// （坑 #2），按钮一律手绘 div；禁用 Esc/遮罩/X 关闭，关闭动作全部经按钮，
    /// 保证 `alert_dialog_open` 生命周期可控。
    pub(super) fn open_alert_dialog(
        &mut self,
        item: AlertItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity();
        let p = self.p;
        window.open_dialog(cx, move |dialog, _window, cx| {
            let (title, body) = view.update(cx, |w, cx| {
                (
                    crate::i18n::label("alertsTitle"),
                    w.render_alert_dialog_body(&item, p, cx),
                )
            });
            dialog
                .w(px(440.))
                .title(title)
                .overlay_closable(false)
                .keyboard(false)
                .close_button(false)
                .child(body)
        });
        cx.notify();
    }

    /// Modern alert dialog body: semantic icon badge + title/meta + accented
    /// message card + hairline-separated footer actions.
    pub(super) fn render_alert_dialog_body(
        &mut self,
        item: &AlertItem,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let source = item.source;
        let dut_id = item.dut_id.clone();
        let at = crate::alerts::format_alert_time(item.at, crate::alerts::now_ms());
        let tone = match item.level {
            crate::alerts::AlertLevel::Error => p.red,
            crate::alerts::AlertLevel::Warning => p.warn,
            crate::alerts::AlertLevel::Info => p.accent,
        };
        let tint = |alpha: f32| gpui_kit::Rgba { a: alpha, ..tone };
        let glyph = match source {
            AlertSource::Agent | AlertSource::AgentNotice => "spark",
            AlertSource::Media => "eye",
            AlertSource::UsbDebug => "usb",
            AlertSource::Relay | AlertSource::Control => "bolt",
            _ => "alert",
        };

        let mut meta = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(999.))
                    .bg(tint(0.14))
                    .border_1()
                    .border_color(tint(0.28))
                    .text_size(px(11.))
                    .font_weight(gpui_kit::FontWeight(550.0))
                    .text_color(tone)
                    .child(source.label()),
            )
            .child(div().text_size(px(11.)).text_color(p.faint).child("·"))
            .child(
                div()
                    .text_size(px(11.))
                    .font_family(theme::MONO_FONT)
                    .text_color(p.faint)
                    .child(SharedString::from(at)),
            );
        if item.repeat > 0 {
            meta = meta.child(
                div()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(999.))
                    .bg(p.raised)
                    .border_1()
                    .border_color(p.line)
                    .text_size(px(10.5))
                    .font_family(theme::MONO_FONT)
                    .text_color(p.muted)
                    .child(SharedString::from(format!("+{}", item.repeat))),
            );
        }

        let header = div()
            .flex()
            .items_start()
            .gap(px(14.))
            .w_full()
            .child(
                div()
                    .w(px(42.))
                    .h(px(42.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(12.))
                    .bg(tint(0.14))
                    .border_1()
                    .border_color(tint(0.28))
                    .child(icon(glyph).size(px(20.)).text_color(tone)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(gpui_kit::FontWeight(650.0))
                            .line_height(px(21.))
                            .text_color(p.text)
                            .whitespace_normal()
                            .child(SharedString::from(item.title.clone())),
                    )
                    .child(meta),
            );

        let mut body = div().flex().flex_col().gap(px(14.)).w_full().child(header);
        if !item.detail.trim().is_empty() {
            body = body.child(
                div()
                    .flex()
                    .w_full()
                    .min_w_0()
                    .rounded(px(10.))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.line)
                    .overflow_hidden()
                    .child(div().w(px(3.)).flex_shrink_0().bg(tone))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px(px(14.))
                            .py(px(12.))
                            .text_size(px(13.))
                            .line_height(px(20.))
                            .text_color(p.text)
                            .whitespace_normal()
                            .child(SharedString::from(item.detail.clone())),
                    ),
            );
        }

        // Agent source carries an "Open pane" action to jump straight to the Agent panel.
        let can_open_agent =
            source == AlertSource::Agent && self.sessions.iter().any(|view| view.dut_id == dut_id);
        let open_panel_btn = can_open_agent.then(|| {
            let target = dut_id.clone();
            div()
                .id("alert-open-panel")
                .flex()
                .items_center()
                .justify_center()
                .h(px(34.))
                .px(px(14.))
                .rounded(px(8.))
                .bg(p.raised)
                .border_1()
                .border_color(p.border)
                .text_size(px(13.))
                .text_color(p.text)
                .cursor_pointer()
                .hover(|s| s.bg(p.hover))
                .child(crate::i18n::label("openPane"))
                .on_click(cx.listener(move |this, _ev, window, cx| {
                    if let Some(index) = this.sessions.iter().position(|view| view.dut_id == target)
                    {
                        this.active_session = index;
                        this.term_grid = None;
                        this.open_side_panel(crate::side_panel::Panel::Agent, window, cx);
                    }
                    this.alert_dialog_open = false;
                    window.close_dialog(cx);
                    cx.notify();
                }))
        });

        let mute_btn = div()
            .id("alert-suppress")
            .flex()
            .items_center()
            .justify_center()
            .h(px(32.))
            .px(px(10.))
            .rounded(px(8.))
            .text_size(px(12.5))
            .text_color(p.muted)
            .cursor_pointer()
            .hover(|s| s.bg(p.hover).text_color(p.text))
            .child(crate::i18n::label("muteAlertType"))
            .on_click(cx.listener(move |this, _ev, window, cx| {
                // A subject that is not a user-editable DUT (relay board id,
                // empty id from a generic-listener notice) has no per-DUT
                // settings row; route it to the global suppression list.
                if this.is_saved_dut_id(&dut_id) {
                    this.edit_dut_alerts(&dut_id, |cfg| {
                        if !cfg.suppressed_sources.iter().any(|key| key == source.key()) {
                            cfg.suppressed_sources.push(source.key().into());
                        }
                    });
                } else {
                    this.suppress_alert_source_globally(source);
                }
                this.alert_dialog_open = false;
                window.close_dialog(cx);
                cx.notify();
            }));

        let ack_btn = div()
            .id("alert-ack")
            .flex()
            .items_center()
            .justify_center()
            .h(px(34.))
            .px(px(18.))
            .rounded(px(8.))
            .bg(p.accent)
            .font_weight(gpui_kit::FontWeight(650.0))
            .text_size(px(13.))
            .text_color(p.accent_contrast)
            .cursor_pointer()
            .hover(|s| s.opacity(0.92))
            .child(crate::i18n::label("acknowledgeAlert"))
            .on_click(cx.listener(move |this, _ev, window, cx| {
                this.alert_dialog_open = false;
                window.close_dialog(cx);
                cx.notify();
            }));

        let mut right_actions = div().flex().items_center().gap(px(8.));
        if let Some(btn) = open_panel_btn {
            right_actions = right_actions.child(btn);
        }
        right_actions = right_actions.child(ack_btn);

        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.))
            .pt(px(12.))
            .border_t_1()
            .border_color(p.line)
            .child(mute_btn)
            .child(right_actions);

        body.child(footer).into_any_element()
    }

    /// 首页：居中品牌 + 创建连接 / 设备工具 + 历史连接磁贴（两行五列，最多 10 个）。
    pub(super) fn render_home(
        &mut self,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut content = div()
            .flex()
            .flex_col()
            .items_center()
            .mx_auto()
            .mt(px(56.0))
            .max_w(px(1240.0))
            .w_full()
            .px(px(24.0));

        // 品牌大图
        content = content.child(
            img("brand/app-256.png")
                .size(px(96.0))
                .rounded(px(22.0))
                .shadow_md(),
        );

        // 首页与标签栏共用连接表单，避免枚举顺序决定连接目标。
        content = content.child(
            div()
                .flex()
                .flex_row()
                .justify_center()
                .gap(px(18.0))
                .mt(px(44.0))
                .w_full()
                .child(
                    div()
                        .id("home-create-session")
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.begin_new_connection(true, cx);
                        }))
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(10.0))
                        .h(px(52.0))
                        .min_w(px(240.0))
                        .rounded(px(10.0))
                        .bg(gpui_kit::Rgba {
                            r: 17.0 / 255.0,
                            g: 61.0 / 255.0,
                            b: 60.0 / 255.0,
                            a: 1.0,
                        })
                        .border_1()
                        .border_color(p.accent)
                        .cursor_pointer()
                        .hover(|s| {
                            s.bg(gpui_kit::Rgba {
                                r: 23.0 / 255.0,
                                g: 79.0 / 255.0,
                                b: 77.0 / 255.0,
                                a: 1.0,
                            })
                        })
                        .child(icon("plus").size(px(16.0)).text_color(p.accent))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(gpui_kit::FontWeight(600.0))
                                .text_color(p.text)
                                .child(crate::i18n::label("createConnection")),
                        ),
                )
                .child(
                    div()
                        .id("home-devtools")
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            this.set_config_page(Some(ConfigPage::Flash));
                            cx.notify();
                        }))
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(10.0))
                        .h(px(52.0))
                        .min_w(px(240.0))
                        .rounded(px(10.0))
                        .bg(p.surface)
                        .border_1()
                        .border_color(p.border)
                        .cursor_pointer()
                        .hover(|s| s.border_color(p.accent).bg(p.hover))
                        .child(icon("maximize").size(px(16.0)).text_color(p.muted))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(gpui_kit::FontWeight(600.0))
                                .text_color(p.text)
                                .child(crate::i18n::label("deviceTools")),
                        ),
                ),
        );

        // 历史连接磁贴（两行五列；不足 10 个按实际数量显示，0 个不显示）
        let tiles: Vec<(usize, &duthub_core::model::DutConfig)> =
            self.duts.file.duts.iter().enumerate().take(10).collect();
        if !tiles.is_empty() {
            // 从中间向两边：flex wrap 居中，磁贴固定宽（每行最多 5 个）
            let mut grid = div()
                .flex()
                .flex_wrap()
                .justify_center()
                .gap(px(14.0))
                .mt(px(40.0))
                .max_w(px(1124.0))
                .w_full();
            for (index, dut) in tiles {
                let online = self
                    .sessions
                    .iter()
                    .any(|s| s.dut_id == dut.id && s.link == LinkState::Connected);
                let tool = usb_debug_enabled(&dut.adb).then_some(dut.adb.tool.as_str());
                let name = dut.name.clone();
                let sub = format!("{} · {}", dut.serial.port, dut.serial.baud);
                let edit_id = dut.id.clone();
                grid = grid.child(
                    div()
                        .id(SharedString::from(format!("home-tile-{index}")))
                        .on_click(cx.listener(move |this, _ev, _win, cx| {
                            this.connect_dut(index, cx);
                        }))
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .min_h(px(116.0))
                        .px(px(14.0))
                        .py(px(12.0))
                        .rounded(px(10.0))
                        .bg(p.surface)
                        .border_1()
                        .border_color(p.border)
                        .cursor_pointer()
                        .hover(|s| s.border_color(p.accent).bg(p.hover))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(28.0))
                                        .rounded(px(7.0))
                                        .bg(p.raised)
                                        .child(
                                            icon("terminal").size(px(14.0)).text_color(p.accent),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_size(px(13.0))
                                        .font_weight(gpui_kit::FontWeight(650.0))
                                        .text_color(p.text)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("home-tile-edit-{index}")))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this, _ev: &MouseDownEvent, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.show_dut_connections(&edit_id);
                                                    cx.notify();
                                                },
                                            ),
                                        )
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(18.0))
                                        .rounded(px(5.0))
                                        .text_size(px(14.0))
                                        .text_color(p.faint)
                                        .hover(|s| s.text_color(p.text).bg(p.hover))
                                        .child("⋯"),
                                )
                                .child(
                                    crate::components::icon_btn(
                                        SharedString::from(format!("home-tile-delete-{index}")),
                                        "trash",
                                        p,
                                    )
                                    .w(px(24.0))
                                    .h(px(24.0))
                                    .tooltip(crate::i18n::label("deleteConnection"))
                                    .on_click(cx.listener(
                                        move |this, _ev, window, cx| {
                                            cx.stop_propagation();
                                            this.open_remove_dut(index, window, cx);
                                        },
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_family(theme::MONO_FONT)
                                .text_color(p.muted)
                                .overflow_hidden()
                                .child(SharedString::from(sub)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(4.0))
                                        .px(px(7.0))
                                        .py(px(2.0))
                                        .rounded(px(5.0))
                                        .bg(if online { p.accent_weak } else { p.raised })
                                        .child(div().size(px(5.0)).rounded_full().bg(if online {
                                            p.accent
                                        } else {
                                            p.faint
                                        }))
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .text_color(if online { p.accent } else { p.muted })
                                                .child(if online {
                                                    crate::i18n::label("online")
                                                } else {
                                                    crate::i18n::label("offline")
                                                }),
                                        ),
                                )
                                .when(tool == Some("adb"), |el| {
                                    el.child(
                                        div()
                                            .px(px(7.0))
                                            .py(px(2.0))
                                            .rounded(px(5.0))
                                            .bg(p.raised)
                                            .text_size(px(10.5))
                                            .text_color(p.blue)
                                            .child("ADB"),
                                    )
                                })
                                .when(tool == Some("hdc"), |el| {
                                    el.child(
                                        div()
                                            .px(px(7.0))
                                            .py(px(2.0))
                                            .rounded(px(5.0))
                                            .bg(p.raised)
                                            .text_size(px(10.5))
                                            .text_color(p.blue)
                                            .child("HDC"),
                                    )
                                }),
                        ),
                );
            }
            content = content.child(grid);
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(content)
    }

    pub(super) fn render_serial_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.p;
        let active = self.active_session;
        if self.terminal_focus_pending && self.overlay.is_none() && !window.has_active_dialog(cx) {
            self.terminal_focus_pending = false;
            window.focus(&self.input_focus, cx);
        }
        let mut page = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(260.))
            .min_h_0()
            .bg(p.term);
        if self.find_open {
            page = page.child(self.render_find_bar(window, p, cx));
        }
        if let Some(session) = self.sessions.get(active)
            && let Some(history) = &session.history
        {
            let older = history.skip + history.screen.rows.len();
            let newer = history
                .skip
                .saturating_sub(duthub_device::term::HISTORY_PAGE_ROWS);
            page = page.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .p(px(8.))
                    .bg(p.surface)
                    .child(
                        btn(
                            "terminal-history-older",
                            i18n::label("terminalHistoryOlder"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .disabled(history.earlier == 0 || session.history_pending.is_some())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.request_terminal_history(older, true, cx);
                        })),
                    )
                    .child(
                        btn(
                            "terminal-history-newer",
                            i18n::label("terminalHistoryNewer"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .disabled(newer == 0 || session.history_pending.is_some())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.request_terminal_history(newer, true, cx);
                        })),
                    )
                    .child(
                        btn(
                            "terminal-history-live",
                            i18n::label("terminalHistoryLive"),
                            &BtnVariant::TinyPrimary,
                            p,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(session) = this.sessions.get_mut(this.active_session) {
                                session.history = None;
                                session.history_pending = None;
                                session.selection = None;
                            }
                            this.terminal_scroll.scroll_to_bottom();
                            window.focus(&this.input_focus, cx);
                            cx.notify();
                        })),
                    ),
            );
        }
        page = page.child(self.render_terminal(active, p, window, cx));
        page
    }

    pub(super) fn request_terminal_history(
        &mut self,
        skip: usize,
        show_empty_notice: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get_mut(self.active_session) else {
            return;
        };
        if session.source != "device" || session.history_pending.is_some() {
            return;
        }
        let request = match self.device.read_terminal_history(&session.dut_id, skip) {
            Ok(request) => request,
            Err(error) => {
                self.notify_toast(i18n::message(
                    "terminalHistoryFailed",
                    &[("error", &error.to_string())],
                ));
                cx.notify();
                return;
            }
        };
        let id = session.session_id.clone();
        let request_id = uuid::Uuid::new_v4();
        session.history_pending = Some(request_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |this, cx| {
                let Some(session) = this
                    .sessions
                    .iter_mut()
                    .find(|session| session.session_id == id)
                else {
                    return;
                };
                if session.history_pending != Some(request_id) {
                    return;
                }
                session.history_pending = None;
                match result {
                    Ok(history) if !history.screen.rows.is_empty() => {
                        session.history = Some(history);
                        session.selection = None;
                        if this
                            .sessions
                            .get(this.active_session)
                            .is_some_and(|session| session.session_id == id)
                        {
                            this.terminal_scroll
                                .scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
                        }
                    }
                    Ok(_) if show_empty_notice => {
                        this.notify_toast(i18n::label("terminalHistoryEmpty"));
                    }
                    Ok(_) => {}
                    Err(error) => this.notify_toast(i18n::message(
                        "terminalHistoryFailed",
                        &[("error", &error.to_string())],
                    )),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_titlebar(
        &mut self,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 单行集成工具行：整行 WCA::Drag（系统拖动 + 双击最大化），
        // tabs/按钮子元素命中优先于拖动；右端为 46px 宽的 Windows 风格控制钮。
        div()
            .flex()
            .items_center()
            .h(px(38.0))
            .min_h(px(38.0))
            .flex_shrink_0()
            .px(px(9.0))
            .gap(px(2.0))
            .bg(p.rail)
            .window_control_area(WindowControlArea::Drag)
            .children(self.render_titlebar_content(p, cx))
    }

    pub(super) fn activate_terminal_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_session = index;
        if self.view != ViewMode::Serial {
            self.connection_open = false;
            self.connection_dut_scope = None;
        }
        self.view = ViewMode::Serial;
        self.open_dd = None;
        self.menu_open = false;
        window.focus(&self.input_focus, cx);
    }

    pub(super) fn open_tab_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) || self.sessions.is_empty() {
            return;
        }
        self.menu_open = false;
        self.open_dd = None;
        let state = cx.new(|cx| CommandState::new(window, cx));
        let focus = state.clone();
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, window, cx| {
            use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
            use nucleo_matcher::{Matcher, Utf32Str};

            let query = state.read(cx).query(cx);
            let mut choices = view
                .upgrade()
                .map_or_default(|view| {
                    let this = view.read(cx);
                    this.sessions
                        .iter()
                        .enumerate()
                        .map(|(index, session)| {
                            (
                                session.session_id.clone(),
                                session.display_title(&this.duts.file.duts),
                                this.view == ViewMode::Serial && index == this.active_session,
                            )
                        })
                        .collect::<Vec<_>>()
                });

            if !query.trim().is_empty() {
                let pattern = Pattern::new(
                    &query,
                    CaseMatching::Ignore,
                    Normalization::Smart,
                    AtomKind::Fuzzy,
                );
                let mut matcher = Matcher::default();
                let mut buffer = Vec::new();
                let mut ranked: Vec<_> = choices
                    .into_iter()
                    .filter_map(|choice| {
                        let text = Utf32Str::new(&choice.1, &mut buffer);
                        pattern
                            .score(text, &mut matcher)
                            .map(|score| (score, choice))
                    })
                    .collect();
                ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
                choices = ranked.into_iter().map(|(_, choice)| choice).collect();
            }
            let ids: Vec<_> = choices.iter().map(|(id, _, _)| id.clone()).collect();
            let view = view.clone();
            let palette = Command::new(&state)
                .bordered(false)
                .placeholder("")
                .filterable(false)
                .items(
                    choices
                        .into_iter()
                        .map(|(_, title, active)| CommandItem::new().label(title).checked(active)),
                )
                .on_query(|_, window, _| window.refresh())
                .on_confirm(move |choice, window, cx| {
                    let Some(id) = ids.get(choice.row) else {
                        return;
                    };
                    window.close_dialog(cx);
                    let _ = view.update(cx, |this, cx| {
                        if let Some(index) = this
                            .sessions
                            .iter()
                            .position(|session| &session.session_id == id)
                        {
                            this.activate_terminal_tab(index, window, cx);
                        }
                        cx.notify();
                    });
                });
            dialog
                .margin_top((window.viewport_size().height / 6.).clamp(px(64.), px(96.)))
                .width(px(540.))
                .close_button(false)
                .p(px(8.))
                .child(palette)
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |state, cx| state.focus(window, cx));
        });
        cx.notify();
    }

    pub(super) fn render_titlebar_content(
        &mut self,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = Vec::new();

        // menu
        out.push(
            div()
                .id("ws-menu")
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    this.menu_open = !this.menu_open;
                    this.open_dd = None;
                    cx.notify();
                }))
                .h(px(26.0))
                .w(px(30.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .occlude()
                .hover(|s| s.bg(p.hover))
                .child(icon("menu").size(px(15.0)).text_color(p.muted))
                .into_any_element(),
        );
        out.push(
            div()
                .flex_none()
                .occlude()
                .child(
                    icon_btn("tab-picker", "tab-search", p)
                        .tooltip(i18n::label("allTabs"))
                        .disabled(self.sessions.is_empty())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_tab_picker(window, cx)),
                        ),
                )
                .into_any_element(),
        );
        // 分隔线
        out.push(
            div()
                .w(px(1.0))
                .h(px(18.0))
                .mx(px(6.0))
                .bg(p.line)
                .into_any_element(),
        );

        // session tabs（h_full 顶接 titlebar 底边，激活 bg(term) 与终端连通）
        let count = self.sessions.len();
        for index in 0..count {
            let is_active = self.view == ViewMode::Serial && index == self.active_session;
            let session = &self.sessions[index];
            let title = session.display_title(&self.duts.file.duts);
            // 重连中的会话 tab 圆点标红（H1.4 重连状态显示）
            let link_red = self.sessions[index].link == LinkState::Reconnecting;
            // Garbled serial bytes keep an amber dot (link loss stays red).
            let garbled = session.last_garbled_level == GarbleLevel::Garbled;
            let mut tab = div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .occlude()
                .h_full()
                .px(px(8.0))
                .min_w(px(105.0))
                .max_w(px(200.0))
                .rounded_t(px(6.0))
                .text_size(px(12.0))
                .text_color(if is_active { p.text } else { p.muted });
            tab = if is_active {
                tab.bg(p.term)
                    .border_t_1()
                    .border_l_1()
                    .border_r_1()
                    .border_color(p.border)
            } else {
                tab.hover(|s| s.bg(p.hover))
            };
            let dot = div()
                .size(px(11.0))
                .rounded_full()
                .bg(p.accent_ring)
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(6.0)).rounded_full().bg(if link_red {
                    p.red
                } else if garbled {
                    p.warn
                } else if is_active {
                    p.accent
                } else {
                    p.faint
                }));
            out.push(
                tab.id(SharedString::from(format!("session-tab-{index}")))
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.activate_terminal_tab(index, window, cx);
                        cx.notify();
                    }))
                    .child(dot)
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .font_weight(gpui_kit::FontWeight(550.0))
                            .child(title),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("session-tab-close-{index}")))
                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                // 阻断冒泡：否则父 tab 的 on_click 也会触发，
                                // 把 active_session 设成删除前的旧索引（可能越界）
                                cx.stop_propagation();
                                this.close_session_view(index, cx);
                            }))
                            .size(px(16.0))
                            .rounded(px(5.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(14.0))
                            .text_color(p.faint)
                            .hover(|s| s.bg(p.hover).text_color(p.text))
                            .child("×"),
                    )
                    .into_any_element(),
            );
        }
        // ＋ 新建会话
        out.push(
            div()
                .id("new-session")
                .on_click(cx.listener(|this, _ev, _window, cx| {
                    this.menu_open = false;
                    this.begin_new_connection(true, cx);
                }))
                .w(px(24.0))
                .h(px(24.0))
                .ml(px(2.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .occlude()
                .hover(|s| s.bg(p.hover))
                .child(icon("plus").size(px(14.0)).text_color(p.muted))
                .into_any_element(),
        );

        // 空档：占满 tab 与工具栏之间（TitleBar 拖动区）
        out.push(div().flex_1().h_full().into_any_element());

        if self.rec_on {
            out.push(
                div()
                    .px(px(4.0))
                    .text_size(px(10.0))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.red)
                    .child("● REC")
                    .into_any_element(),
            );
        }

        // Windows 风格窗口控制钮（46px 宽，16px 字形；系统命中区 Min/Max/Close）
        out.push(
            div()
                .flex()
                .items_center()
                .h_full()
                .flex_shrink_0()
                .ml(px(6.0))
                .occlude()
                .child(Self::win_btn(
                    "win-min",
                    "window-minimize",
                    WindowControlArea::Min,
                    p,
                    false,
                ))
                .child(Self::win_btn(
                    "win-max",
                    "window-maximize",
                    WindowControlArea::Max,
                    p,
                    false,
                ))
                .child(Self::win_btn(
                    "win-close",
                    "window-close",
                    WindowControlArea::Close,
                    p,
                    true,
                ))
                .into_any_element(),
        );
        out
    }

    /// 单个窗口控制钮：宽 46 与 Windows 原生一致；close 悬停红底。
    pub(super) fn win_btn(
        id: &'static str,
        icon_name: &'static str,
        area: WindowControlArea,
        p: &'static Palette,
        close: bool,
    ) -> gpui_kit::Stateful<Div> {
        let hover_fg = if close { p.bg } else { p.text };
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(46.0))
            .h_full()
            .text_color(p.muted)
            .window_control_area(area)
            .hover(move |s| {
                s.bg(if close { p.red } else { p.hover })
                    .text_color(hover_fg)
            })
            .child(kit_icon(icon_name).size(px(16.0)))
    }

    pub(super) fn render_find_bar(
        &mut self,
        window: &mut Window,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let query = self.find_query.clone();
        let folded_query = query.to_lowercase();
        let total = if folded_query.is_empty() {
            0
        } else {
            self.sessions
                .get(self.active_session)
                .map_or(0, |v| {
                    let annotations = v.history.is_none().then_some(v.lines.as_slice());
                    let terminal_rows = v
                        .displayed_screen()
                        .into_iter()
                        .flat_map(|screen| &screen.rows);
                    annotations
                        .into_iter()
                        .flatten()
                        .map(|line| line.text.as_ref())
                        .chain(terminal_rows.map(|row| row.text.as_str()))
                        .filter(|line| line.to_lowercase().contains(&folded_query))
                        .count()
                })
        };
        let mut bar = div()
            .flex()
            .items_center()
            .gap(px(9.))
            .h(px(47.))
            .px(px(20.))
            .bg(p.surface)
            .border_b_1()
            .border_color(p.border)
            .child(icon("search").size(px(15.)).text_color(p.muted));
        let input = div().max_w(px(360.)).flex_1().child(self.text_field(
            In::Find,
            &query,
            crate::i18n::label("findTerminal"),
            false,
            p,
            window,
            cx,
        ));
        bar = bar
            .child(input)
            .child(
                div()
                    .min_w(px(56.))
                    .text_center()
                    .text_size(px(11.))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.muted)
                    .child(SharedString::from(format!("0 / {total}"))),
            )
            .child(
                div()
                    .id("find-close")
                    .on_click(cx.listener(|this, _ev, _win, cx| {
                        this.find_open = false;
                        cx.notify();
                    }))
                    .size(px(29.))
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(p.muted)
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .child("×"),
            );
        bar
    }

    pub(super) fn render_terminal(
        &mut self,
        active: usize,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let query = self.find_query.to_lowercase();
        // 跟随模式（原型 nearBottom）：上一帧视口贴近底部（或内容不足一屏）时，
        // 新输出自动滚到底部；用户上翻阅读时不打断。
        let near_bottom = {
            let state = self.terminal_scroll.0.borrow();
            let offset = state.base_handle.offset().y;
            let max = state.base_handle.max_offset().y;
            // offset 向下滚动为负，到底时 offset.y ≈ -max；距底部距离 = max + offset
            max <= px(0.) || (max + offset) < px(65.)
        };
        // real 会话（PuTTY 模式）：视口尺寸变化 → 下发 term.resize(cols, rows)
        let is_device = self
            .sessions
            .get(active)
            .is_some_and(|s| s.source == "device");
        let (font_family, font_size, cell_width) = self.terminal_font(window);
        let row_height = font_size * (20.6 / DEFAULT_TERMINAL_FONT_SIZE);
        if is_device {
            let (left, right) = self.pane_widths(window.viewport_size().width);
            let width = left + right;
            let (cols, rows) = terminal_grid_for(window, width, cell_width, row_height);
            if self.term_grid != Some((cols, rows))
                && let Some(view) = self.sessions.get(active)
            {
                let dut_id = view.dut_id.clone();
                if self.device.resize_terminal(&dut_id, cols, rows).is_ok() {
                    self.term_grid = Some((cols, rows));
                }
            }
        }
        // 视口容器只承载焦点/按键/内边距；滚动交给内部的 uniform_list
        // （虚拟化：仅可见行构建元素，此前每帧全量重建 4000+400 个 div）。
        let selection_bg = terminal_selection_bg(p, self.theme_mode);
        let mut viewport = div()
            .id("terminal-viewport")
            .flex()
            .flex_1()
            .flex_col()
            .min_h_0()
            .px(px(19.))
            .pt(px(16.))
            .pb(px(40.))
            .text_size(font_size)
            .line_height(row_height)
            .font_family(font_family)
            .cursor_text()
            .track_focus(&self.input_focus)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.input_focus, cx);
                    if this.settings.serial.paste_on_right_click {
                        this.terminal_ctx = None;
                        this.clear_terminal_selection(this.active_session);
                        this.paste_terminal(cx);
                    } else {
                        this.terminal_ctx =
                            Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                    }
                    cx.notify();
                }),
            )
            .on_scroll(
                cx.listener(move |this, event: &gpui_kit::ScrollEvent, _, cx| {
                    if this.overlay.is_some() || event.delta.pixel_delta(row_height).y <= px(0.) {
                        return;
                    }
                    if this.terminal_scroll.0.borrow().base_handle.offset().y < -px(1.) {
                        return;
                    }
                    let Some(screen) = this.sessions.get(active).and_then(|view| {
                        (view.source == "device" && view.history.is_none())
                            .then_some(view.screen.as_ref())
                            .flatten()
                    }) else {
                        return;
                    };
                    let skip = screen.rows.len();
                    if skip >= this.device.terminal_buffer().1 {
                        this.request_terminal_history(skip, false, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    if this.overlay.is_some() || window.has_active_dialog(cx) {
                        return;
                    }
                    this.terminal_ctx = None;
                    window.focus(&this.input_focus, cx);
                    this.begin_terminal_selection(active, ev, window);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                    if this.overlay.is_some() || window.has_active_dialog(cx) {
                        return;
                    }
                    this.terminal_ctx = None;
                    window.focus(&this.input_focus, cx);
                    this.clear_terminal_selection(active);
                    this.paste_terminal(cx);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.terminal_ctx.take().is_some() {
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if this.overlay.is_some() || window.has_active_dialog(cx) {
                    return;
                }
                if this.terminal_zoom_key(event, cx) {
                    return;
                }
                let k = &event.keystroke;
                // Copy works across live, history, device, and demo views.
                let copy_shortcut = (k.modifiers.control && k.modifiers.shift && k.key == "c")
                    || (k.modifiers.platform
                        && !k.modifiers.control
                        && !k.modifiers.alt
                        && k.key == "c")
                    || (k.modifiers.control
                        && !k.modifiers.shift
                        && !k.modifiers.alt
                        && k.key == "insert");
                let smart_ctrl_c = k.modifiers.control
                    && !k.modifiers.shift
                    && !k.modifiers.alt
                    && !k.modifiers.platform
                    && k.key == "c"
                    && this.has_terminal_selection(active);
                if copy_shortcut || smart_ctrl_c {
                    if this.copy_terminal_selection(active, smart_ctrl_c, cx) {
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                // Reading terminal history must not send keys to the live device.
                if this
                    .sessions
                    .get(active)
                    .is_some_and(|session| session.history.is_some())
                {
                    return;
                }
                let paste_shortcut = (k.modifiers.control && k.modifiers.shift && k.key == "v")
                    || (k.modifiers.platform
                        && !k.modifiers.control
                        && !k.modifiers.alt
                        && k.key == "v")
                    || (k.modifiers.shift
                        && !k.modifiers.control
                        && !k.modifiers.alt
                        && k.key == "insert");
                if paste_shortcut {
                    this.clear_terminal_selection(active);
                    this.paste_terminal(cx);
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if is_device {
                    if k.modifiers.alt || k.modifiers.platform {
                        return;
                    }
                    if k.modifiers.control {
                        if let [key @ b'a'..=b'z'] = k.key.as_bytes() {
                            this.clear_terminal_selection(active);
                            this.send_bytes(&[key - b'a' + 1], cx);
                            cx.stop_propagation();
                        }
                        return;
                    }
                    match k.key.as_str() {
                        // Enter/Backspace bytes follow settings.serial (CR / DEL by default).
                        "enter" => {
                            this.clear_terminal_selection(active);
                            this.send_bytes(this.enter_bytes(), cx);
                        }
                        "backspace" => {
                            this.clear_terminal_selection(active);
                            this.send_bytes(&[this.backspace_byte()], cx);
                        }
                        "tab" => {
                            this.clear_terminal_selection(active);
                            this.send_bytes(b"\t", cx);
                        }
                        "escape" => {
                            this.clear_terminal_selection(active);
                            this.send_bytes(b"\x1b", cx);
                        }
                        "up" | "down" | "right" | "left" | "home" | "end" => {
                            let prefix = arrow_escape_prefix(
                                this.sessions
                                    .get(active)
                                    .and_then(|session| session.screen.as_ref())
                                    .is_some_and(|screen| screen.application_cursor),
                            );
                            let code = match k.key.as_str() {
                                "up" => b'A',
                                "down" => b'B',
                                "right" => b'C',
                                "left" => b'D',
                                "home" => b'H',
                                _ => b'F',
                            };
                            this.clear_terminal_selection(active);
                            this.send_bytes(&[0x1b, prefix, code], cx);
                            cx.stop_propagation();
                        }
                        "delete" => {
                            this.clear_terminal_selection(active);
                            this.send_bytes(b"\x1b[3~", cx);
                            cx.stop_propagation();
                        }
                        _ => {
                            if let Some(ch) = typed_char(event) {
                                this.clear_terminal_selection(active);
                                this.send_bytes(ch.as_bytes(), cx);
                            }
                        }
                    }
                    return;
                }
                if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
                    return;
                }
                if k.key == "enter" {
                    this.clear_terminal_selection(active);
                    this.send_input(cx);
                    return;
                }
                if k.key == "backspace" {
                    this.clear_terminal_selection(active);
                    this.input.pop();
                    cx.notify();
                    return;
                }
                if let Some(ch) = typed_char(event).filter(|_| this.input.len() < 4096) {
                    this.clear_terminal_selection(active);
                    this.input.push_str(&ch);
                    cx.notify();
                }
            }));
        let empty = self
            .sessions
            .get(active)
            .is_none_or(|s| {
                s.lines.is_empty()
                    && s.screen
                        .as_ref()
                        .is_none_or(|sc| sc.rows.iter().all(|r| r.text.is_empty()))
            });
        if empty && !is_device {
            viewport = viewport.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .mt(px(90.))
                    .gap(px(14.))
                    .text_color(p.muted)
                    .child(icon("terminal").size(px(34.)).text_color(p.faint))
                    .child("Duthub Console")
                    .child(
                        div()
                            .text_size(px(12.))
                            .child(SharedString::from(crate::i18n::label("waitingForSerial"))),
                    ),
            );
        } else if let Some(session) = self.sessions.get(active) {
            // 虚拟化列表：注记行 + 网格行 + demo 输入行统一为固定行高的列表
            // 项，只为可见范围构建元素。滚动/贴底跟随全部走 UniformList 句柄。
            let rows_len = session.displayed_screen().map_or(0, |sc| sc.rows.len());
            let annotations = if session.history.is_some() {
                0
            } else {
                session.lines.len()
            };
            let item_count = annotations + rows_len + usize::from(!is_device);
            let weak = cx.entity().downgrade();
            let query = query.clone();
            let highlight_rules = self.compiled_highlight_rules();
            viewport = viewport.child(
                gpui_kit::uniform_list("terminal-list", item_count, move |range, _window, cx| {
                    let Some(workbench) = weak.upgrade() else {
                        return Vec::new();
                    };
                    let this = workbench.read(cx);
                    let Some(session) = this.sessions.get(active) else {
                        return Vec::new();
                    };
                    let rows_len = session.displayed_screen().map_or(0, |sc| sc.rows.len());
                    let selection = session.selection;
                    let mut items = Vec::with_capacity(range.end - range.start);
                    for idx in range {
                        if idx < annotations {
                            let line = &session.lines[idx];
                            let sel_range = selection
                                .and_then(|sel| sel.row_range(idx, line.text.chars().count()));
                            items.push(
                                this.terminal_line_item(
                                    line,
                                    &query,
                                    sel_range,
                                    selection_bg,
                                    row_height,
                                    font_size,
                                    p,
                                )
                                .into_any_element(),
                            );
                        } else if idx < annotations + rows_len {
                            let row_idx = idx - annotations;
                            let sel_range = selection.and_then(|sel| {
                                let len = session
                                    .displayed_screen()
                                    .and_then(|sc| sc.rows.get(row_idx))
                                    .map_or(0, |row| row.text.chars().count());
                                sel.row_range(idx, len)
                            });
                            items.push(
                                Self::terminal_row_item(
                                    session,
                                    row_idx,
                                    &query,
                                    &highlight_rules,
                                    sel_range,
                                    selection_bg,
                                    row_height,
                                    font_size,
                                    p,
                                )
                                .into_any_element(),
                            );
                        } else {
                            let demo_len = "root:~# ".chars().count() + this.input.chars().count();
                            let sel_range = selection.and_then(|sel| sel.row_range(idx, demo_len));
                            items.push(
                                this.terminal_demo_input_item(
                                    sel_range,
                                    selection_bg,
                                    row_height,
                                    font_size,
                                    p,
                                )
                                .into_any_element(),
                            );
                        }
                    }
                    items
                })
                .track_scroll(&self.terminal_scroll)
                .flex_1()
                .w_full(),
            );
        }
        let selecting = self
            .sessions
            .get(active)
            .and_then(|session| session.selection)
            .is_some_and(|selection| selection.dragging);
        if near_bottom
            && !selecting
            && self
                .sessions
                .get(active)
                .is_none_or(|session| session.history.is_none())
        {
            // Keep following live output at the bottom unless user is dragging a selection.
            self.terminal_scroll.scroll_to_bottom();
        }
        viewport
    }

    /// Render an annotation at the same fixed height as a terminal row.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn terminal_line_item(
        &self,
        line: &LogLine,
        query: &str,
        selection: Option<(usize, usize)>,
        selection_bg: gpui_kit::Rgba,
        row_height: Pixels,
        font_size: Pixels,
        p: &'static Palette,
    ) -> AnyElement {
        let hit = !query.is_empty() && line.text.to_lowercase().contains(query);
        let mut el = div()
            .h(row_height)
            .line_height(row_height)
            .text_size(font_size)
            .whitespace_nowrap()
            .text_color(line.color(p))
            .when(hit, |el| el.bg(p.match_));
        if line.kind == LineKind::Mark {
            // Segment marker divider flanked by thin rules.
            let mut label = div().flex_none().text_size(font_size * 0.84);
            if selection.is_some() {
                label = label.bg(selection_bg);
            }
            el = el
                .flex()
                .items_center()
                .gap(px(9.))
                .child(div().flex_1().h(px(1.)).bg(p.line))
                .child(label.child(line.text.clone()))
                .child(div().flex_1().h(px(1.)).bg(p.line));
        } else if selection.is_some() {
            let runs = crate::terminal_selection::split_plain_selection(&line.text, selection);
            el = el.flex().children(runs.into_iter().map(|(text, selected)| {
                let mut cell = div().flex_shrink_0().whitespace_nowrap();
                if selected {
                    cell = cell.bg(selection_bg);
                }
                cell.child(SharedString::from(text.to_owned()))
            }));
        } else {
            el = el.child(line.text.clone());
        }
        el.into_any_element()
    }

    /// Return cached compiled prompt highlight rules as an `Arc` so unchanged
    /// rule sets perform zero regex/string clones during terminal rendering.
    pub(super) fn compiled_highlight_rules(&self) -> Arc<Vec<crate::highlight::CompiledRule>> {
        {
            let cached = self.highlight_rules_cache.borrow();
            if let Some((source_rules, compiled)) = cached.as_ref()
                && source_rules == &self.settings.serial.highlight_rules
            {
                return Arc::clone(compiled);
            }
        }
        let mut cache = self.highlight_cache.borrow_mut();
        if cache.len() > 128 {
            cache.clear();
        }
        let rules: Vec<crate::highlight::CompiledRule> = self
            .settings
            .serial
            .highlight_rules
            .iter()
            .filter(|rule| rule.enabled && !rule.regex.trim().is_empty())
            .filter_map(|rule| {
                let compiled = if let Some(compiled) = cache.get(&rule.regex) { compiled.clone() } else {
                    let compiled = regex::Regex::new(&rule.regex).ok();
                    cache.insert(rule.regex.clone(), compiled.clone());
                    compiled
                };
                compiled.map(|regex| crate::highlight::CompiledRule {
                    regex,
                    color: rule.color.clone(),
                })
            })
            .collect();
        let arc = Arc::new(rules);
        *self.highlight_rules_cache.borrow_mut() = Some((
            self.settings.serial.highlight_rules.clone(),
            Arc::clone(&arc),
        ));
        arc
    }

    /// Virtual list item: emulator grid row with ANSI colors, prompt highlight
    /// overlay, cursor inversion, and mouse selection background.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn terminal_row_item(
        session: &SessionView,
        idx: usize,
        query: &str,
        highlight_rules: &[crate::highlight::CompiledRule],
        selection: Option<(usize, usize)>,
        selection_bg: gpui_kit::Rgba,
        row_height: Pixels,
        font_size: Pixels,
        p: &'static Palette,
    ) -> AnyElement {
        let Some(screen) = session.displayed_screen() else {
            return div().h(row_height).into_any_element();
        };
        let Some(row) = screen.rows.get(idx) else {
            return div().h(row_height).into_any_element();
        };
        let hit = !query.is_empty() && row.text.to_lowercase().contains(query);
        let mut el = div()
            .h(row_height)
            .line_height(row_height)
            .text_size(font_size)
            .whitespace_nowrap()
            .text_color(row.fg.map_or(p.term_fg, theme::rgba_hex))
            .when(hit, |el| el.bg(p.match_));
        if let Some(bg) = row.bg {
            el = el.bg(theme::rgba_hex(bg));
        }
        let cursor_char = if screen.cursor_row == Some(idx as u32) {
            screen.cursor_char
        } else {
            None
        };
        let segments = crate::highlight::row_segments_with_selection(
            &row.text,
            highlight_rules,
            cursor_char,
            selection,
        );
        if segments.len() == 1
            && !segments[0].is_cursor
            && !segments[0].selected
            && segments[0].color.is_none()
        {
            el = el.child(SharedString::from(segments[0].text.clone()));
        } else {
            el = el.flex().children(segments.into_iter().map(|segment| {
                if segment.is_cursor {
                    div()
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .text_color(p.term)
                        .bg(p.term_fg)
                        .child(SharedString::from(segment.text))
                } else {
                    let mut cell = div().flex_shrink_0().whitespace_nowrap();
                    if let Some(color) = segment.color {
                        cell = cell.text_color(crate::pages::hex_to_rgba(&color, p));
                    }
                    if segment.selected {
                        cell = cell.bg(selection_bg);
                    }
                    cell.child(SharedString::from(segment.text))
                }
            }));
        }
        el.into_any_element()
    }

    /// Virtual list item: demo session local input line (real sessions rely on
    /// remote device echo instead).
    pub(super) fn terminal_demo_input_item(
        &self,
        selection: Option<(usize, usize)>,
        selection_bg: gpui_kit::Rgba,
        row_height: Pixels,
        font_size: Pixels,
        p: &'static Palette,
    ) -> AnyElement {
        const PROMPT: &str = "root:~# ";
        let prompt_len = PROMPT.chars().count();
        let prompt_range = selection.and_then(|(s, e)| {
            let s = s.min(prompt_len);
            let e = e.min(prompt_len);
            (s < e).then_some((s, e))
        });
        let input_range = selection.and_then(|(s, e)| {
            let s = s.saturating_sub(prompt_len);
            let e = e.saturating_sub(prompt_len);
            (s < e).then_some((s, e))
        });
        let prompt_el = if prompt_range.is_some() {
            let runs = crate::terminal_selection::split_plain_selection(PROMPT, prompt_range);
            div()
                .flex()
                .text_color(p.prompt)
                .font_weight(gpui_kit::FontWeight(600.0))
                .children(runs.into_iter().map(|(text, selected)| {
                    let mut cell = div().flex_shrink_0().whitespace_nowrap();
                    if selected {
                        cell = cell.bg(selection_bg);
                    }
                    cell.child(SharedString::from(text.to_owned()))
                }))
        } else {
            div()
                .text_color(p.prompt)
                .font_weight(gpui_kit::FontWeight(600.0))
                .whitespace_nowrap()
                .child(PROMPT)
        };
        let input = if self.input.is_empty() {
            div()
                .text_color(p.faint)
                .child(crate::i18n::label("terminalInputHint"))
        } else if input_range.is_some() {
            let runs = crate::terminal_selection::split_plain_selection(&self.input, input_range);
            div()
                .flex()
                .children(runs.into_iter().map(|(text, selected)| {
                    let mut cell = div().flex_shrink_0().whitespace_nowrap();
                    if selected {
                        cell = cell.bg(selection_bg);
                    }
                    cell.child(SharedString::from(text.to_owned()))
                }))
        } else {
            div()
                .whitespace_nowrap()
                .child(SharedString::from(self.input.clone()))
        };
        div()
            .h(row_height)
            .line_height(row_height)
            .text_size(font_size)
            .whitespace_nowrap()
            .flex()
            .items_start()
            .text_color(p.term_fg)
            .child(prompt_el)
            .child(input)
            .into_any_element()
    }

    pub(super) fn show_command_dock(&self) -> bool {
        self.settings.terminal.show_command_bar && !self.settings.commands.is_empty()
    }

    pub(super) fn render_command_dock(
        &mut self,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut dock = div()
            .flex()
            .items_center()
            .flex_1()
            .min_w_0()
            .gap(px(3.))
            .id("command-dock")
            .overflow_x_scroll();
        let can_run =
            self.view == ViewMode::Serial && self.sessions.get(self.active_session).is_some();
        for (index, command) in self.settings.commands.iter().enumerate() {
            let value = command.value.clone();
            dock = dock.child(
                div()
                    .id(SharedString::from(format!("dock-cmd-{index}")))
                    .when(can_run, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.input = value.clone();
                            this.send_input(cx);
                        }))
                    })
                    .when(!can_run, |button| button.opacity(0.6))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .h(px(29.))
                    .flex_none()
                    .min_w_0()
                    .whitespace_nowrap()
                    .px(px(6.))
                    .rounded(px(5.))
                    .text_size(px(11.))
                    .line_height(px(19.))
                    .font_family(theme::MONO_FONT)
                    .text_color(p.muted)
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .child(
                        icon(if command.icon.is_empty() {
                            "bolt"
                        } else {
                            command.icon.as_str()
                        })
                        .size(px(14.))
                        .flex_none()
                        .text_color(p.accent),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(command.name.clone())),
                    ),
            );
        }
        dock = dock.child(
            div()
                .id("dock-add")
                .on_click(cx.listener(|this, _ev, window, cx| {
                    this.open_modal(ModalKind::NewCommand, window, cx);
                    cx.notify();
                }))
                .size(px(29.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .text_color(p.muted)
                .hover(|s| s.bg(p.hover).text_color(p.text))
                .child(icon("plus").size(px(15.)).text_color(p.muted)),
        );
        dock
    }

    pub(crate) fn open_remove_dut(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !window.has_active_dialog(cx)
            && let Some(dut) = self.duts.file.duts.get(index)
        {
            self.open_modal(ModalKind::RemoveDut(dut.id.clone()), window, cx);
        }
    }

    /// The saved control device (relay board) currently selected, if any.
    pub(crate) fn selected_relay_board(&self) -> Option<&duthub_core::model::RelayBoard> {
        self.duts.file.relay_boards.get(self.relay_board_selected)
    }

    /// Edit value for the selected board: the draft wins, the saved board is
    /// the fallback. Rendering and input echo both go through this.
    pub(crate) fn relay_board_edit(&self) -> Option<duthub_core::model::RelayBoard> {
        let saved = self.selected_relay_board()?;
        match &self.relay_board_draft {
            Some(draft) if draft.id == saved.id => Some(draft.clone()),
            _ => Some(saved.clone()),
        }
    }

    /// Whether the selected board has unsaved edits.
    pub(crate) fn relay_board_dirty(&self) -> bool {
        match (&self.relay_board_draft, self.selected_relay_board()) {
            (Some(draft), Some(saved)) => draft.id == saved.id && draft != saved,
            _ => false,
        }
    }

    /// Route one text/port edit into the draft (never into the board list).
    pub(crate) fn set_relay_board_field(&mut self, id: In, value: String) {
        self.input_set(id, value);
    }

    /// Channel-count edit, draft-only.
    pub(crate) fn set_relay_board_channel_count(&mut self, count: u8) {
        if let Some(saved) = self.selected_relay_board().cloned() {
            let draft = self.relay_board_draft.get_or_insert(saved);
            draft.channel_count = count;
        }
    }

    /// Commit the draft: name and serial port are required; only then is the
    /// board list updated and persisted (atomic `save_duts`).
    pub(crate) fn save_relay_board(&mut self) {
        let Some(draft) = self.relay_board_draft.clone() else {
            return;
        };
        if draft.name.trim().is_empty() || draft.node.trim().is_empty() {
            self.notify_toast(i18n::label("relayBoardIncomplete").to_string());
            return;
        }
        if let Some(slot) = self
            .duts
            .file
            .relay_boards
            .iter_mut()
            .find(|board| board.id == draft.id)
        {
            *slot = draft;
        }
        self.duts.dirty = true;
        self.save_duts();
        // Rebuild the name input from the saved value on the next frame.
        self.relay_board_draft = None;
        self.relay_board_name = None;
    }

    /// Discard the draft and fall back to the saved value.
    pub(crate) fn revert_relay_board(&mut self) {
        self.relay_board_draft = None;
        self.relay_board_name = None;
    }

    pub(crate) fn remove_control_device(&mut self, id: &str) -> Result<(), String> {
        let mut draft = self.duts.file.clone();
        crate::data::remove_control_device(&mut draft, id)?;
        self.heartbeat.commit_configs(&HashMap::new(), |_| {
            if let Some(mut file) =
                store::load_duts(&self.data_dir).map_err(|err| err.to_string())?
            {
                file.migrate_usb_relays();
                if crate::data::remove_control_device(&mut file, id)? {
                    store::save_duts_atomic(&self.data_dir, &file)
                        .map_err(|err| err.to_string())?;
                }
            }
            Ok(())
        })?;
        self.duts.file = draft;
        self.relay_board_selected = self
            .relay_board_selected
            .min(self.duts.file.relay_boards.len().saturating_sub(1));
        self.relay_board_name = None;
        self.relay_board_draft = None;
        Ok(())
    }

    pub fn remove_dut(&mut self, dut_id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(index) = self.duts.file.duts.iter().position(|dut| dut.id == dut_id) else {
            return Err(crate::i18n::label("connectionMissing").into());
        };
        let removed_name = self.duts.file.duts[index].name.clone();
        // W4a 显式清理序：strip MCP → 落盘删除 → 停 Agent/串口/A2A → 删
        // Token → 删 Agent 目录。strip/persist 失败原样 Err 中止（DUT 保持
        // 原状）；Token/目录失败记入 pending-cleanup 并告警，绝不谎称清理
        // 完成——warnings 随成功路径带回给 toast。
        let outcome = duthub_desktop::agent_ui::run_remove_dut(self, dut_id)?;
        crate::data::remove_connection(&mut self.duts.file, dut_id);
        self.a2a_tokens.remove(dut_id);
        self.duts.forget_saved_bindings(dut_id);
        self.media.reconcile_bindings(&self.duts);
        self.alerts
            .lock()
            .unwrap()
            .set_dut_settings(&self.duts.file.alerts_by_dut);
        // The detection map is keyed by DUT id: drop the switch explicitly so a
        // stale session id can never keep observing bytes after deletion.
        self.device.set_garbled_detection_for(dut_id, false);
        self.sync_garbled_runtime();
        if index < self.duts.selected {
            self.duts.selected -= 1;
        }
        self.duts.selected = self
            .duts
            .selected
            .min(self.duts.file.duts.len().saturating_sub(1));
        self.clear_dut_endpoint_drafts();
        self.heartbeat_drafts.remove(dut_id);
        if let Some(index) = self.sessions.iter().position(|view| view.dut_id == dut_id) {
            self.close_session_view(index, cx);
        }
        self.agent_prompts.remove(dut_id);
        self.agent_transcript_scroll.remove(dut_id);
        self.agent_transcript_seen.remove(dut_id);
        self.agent_prompt_clear.remove(dut_id);
        // DUT 级 Agent 运行状态：停止请求已在 stop_and_join 发出，清掉
        // UI 侧草稿/滚动状态；宿主的会话记录留在 HostManager（已进入终止态，
        // 重启同 id 会话时被回收）。
        self.control.unregister_dut(dut_id);
        for warning in &outcome.warnings {
            self.notify_toast(i18n::message(
                "dutDeletedWithPendingWarning",
                &[("warning", warning)],
            ));
        }
        self.notify_toast(crate::i18n::message(
            "connectionDeleted",
            &[("name", &removed_name)],
        ));
        Ok(())
    }

    pub(super) fn close_session_view(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(view) = self.sessions.get(index) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        // 关闭串口标签 → 该 DUT 的 Agent 会话只发停止请求（非阻塞；等待/杀树在宿主线程）。
        self.stop_agent_session(&dut_id);
        // demo 会话同样注册在 DeviceService（SessionKind::Demo），tab 关闭必须级联
        // 关闭底层会话——否则 demo 任务/注册表句柄/端口锁泄漏，同一 DUT 重开恒 Busy；
        // 注册表无此 dut 时返回 NotFound，忽略即可（NotFound 之外的错误同样不影响 tab 关闭）。
        let _ = self.device.close_session(&dut_id);
        self.sessions.remove(index);
        // 关闭位于活动会话左侧的 tab 时，活动索引随列表收缩前移
        //（否则活动视图静默切到相邻会话——输入行会把命令写进另一个串口）
        if index < self.active_session {
            self.active_session -= 1;
        }
        if self.active_session >= self.sessions.len() {
            self.active_session = self.sessions.len().saturating_sub(1);
        }
        // 关闭全部会话 → 回首页
        if self.sessions.is_empty() {
            self.view = ViewMode::Home;
        }
        cx.notify();
    }

    /// 从 DUT 绑定打开会话（NewSession 弹窗回调）。
    /// 直连真实串口（首页卡片 / ＋ 新建会话共用）：成功建 tab，失败只 toast 一次。
    pub fn open_real_port(&mut self, port: &str, baud: u32, cx: &mut Context<Self>) {
        let dut_id = Self::dut_id_for_port(&self.duts, port);
        let dut = self.duts.file.duts.iter().find(|dut| dut.id == dut_id);
        let name = dut.map_or_else(
            || archive_safe_name(port, &dut_id),
            |dut| archive_safe_name(&dut.name, &dut_id),
        );
        let reference = dut.and_then(|dut| boot_reference_path(&self.artifact_root, dut));
        match self.device.open_named_session_with_log_options(
            &dut_id,
            &name,
            port,
            baud,
            duthub_device::SessionKind::Real,
            duthub_device::SerialLogOptions {
                split: dut.is_some_and(|dut| dut.serial.split_logs),
                reference: reference.as_deref(),
            },
        ) {
            Ok(session_id) => {
                let index = self.ensure_session(&session_id, &dut_id);
                self.sessions[index].title = format!("{port} · device");
                self.active_session = index;
                self.view = ViewMode::Serial;
                self.terminal_focus_pending = true;
                self.notify_toast(i18n::message(
                    "serialConnectionCreated",
                    &[("port", port), ("baud", &baud.to_string())],
                ));
                // 保存为主页磁贴（Xshell 式：下次直连/修改复用）；磁贴名走
                // 归档守卫（COM 口名直存会让下次 matched 命中后再炸一次）
                if !self.duts.file.duts.iter().any(|d| d.id == dut_id) {
                    let mut dut = demo_dut_template(&name, port);
                    dut.id = dut_id;
                    dut.serial.baud = baud;
                    self.duts.file.duts.push(dut);
                    self.duts.selected = self.duts.file.duts.len() - 1;
                    self.clear_dut_endpoint_drafts();
                    if let Err(err) = self.duts.save(&self.data_dir) {
                        self.notify_toast(i18n::message(
                            "saveSessionFailed",
                            &[("error", &err.clone())],
                        ));
                    }
                }
            }
            Err(err) => {
                let hint = if matches!(err, duthub_core::DeviceError::Unsupported) {
                    i18n::label("realHardwareDisabled").to_string()
                } else {
                    i18n::message("connectFailed", &[("error", &err.to_string())])
                };
                // 首开失败（端口被占/不支持）：toast + 弹窗提醒（需求⑤接入点②）
                self.report_serial_session_failure(
                    &dut_id,
                    port,
                    i18n::label("serialConnectFailedTitle"),
                    &hint,
                );
                self.notify_toast(hint);
            }
        }
        cx.notify();
    }

    /// 终端右键菜单：立即重连 / 重置终端 / 查找 / 复制 / 清屏。
    pub(super) fn render_terminal_ctx_menu(
        &mut self,
        x: f32,
        y: f32,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut menu = div()
            .id("terminal-ctx")
            .min_w(px(104.0))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .rounded(px(9.0))
            .shadow_lg()
            .p(px(6.0))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.terminal_ctx = None;
                cx.notify();
            }));

        let item = |this_id: &'static str, label: &'static str| {
            div()
                .id(this_id)
                .flex()
                .items_center()
                .justify_between()
                .gap(px(10.0))
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(6.0))
                .text_size(px(12.0))
                .whitespace_nowrap()
                .text_color(p.text)
                .cursor_pointer()
                .hover(|s| s.bg(p.hover))
                .child(SharedString::from(label))
        };

        menu = menu.child(
            item("tctx-reconnect", crate::i18n::label("reconnectNow")).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _win, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    this.request_manual_reconnect(cx);
                    cx.notify();
                }),
            ),
        );

        menu = menu.child(
            item("tctx-reset", crate::i18n::label("resetTerminal")).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    this.reset_serial_terminal(window, cx);
                }),
            ),
        );

        menu = menu.child(
            item("tctx-find", crate::i18n::label("findShort")).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _win, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    this.find_open = !this.find_open;
                    cx.notify();
                }),
            ),
        );
        if self
            .sessions
            .get(self.active_session)
            .is_some_and(|view| view.history.is_none())
        {
            menu = menu.child(
                item("tctx-paste", crate::i18n::label("paste")).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, window, cx| {
                        cx.stop_propagation();
                        this.terminal_ctx = None;
                        window.focus(&this.input_focus, cx);
                        this.clear_terminal_selection(this.active_session);
                        this.paste_terminal(cx);
                        cx.notify();
                    }),
                ),
            );
        }
        menu = menu.child(
            item("tctx-copy", crate::i18n::label("copyOutput")).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    window.focus(&this.input_focus, cx);
                    if !this.copy_terminal_selection(this.active_session, true, cx)
                        && let Some(view) = this.sessions.get(this.active_session)
                    {
                        // Device session: annotations + emulator grid; demo session: all lines.
                        let mut parts: Vec<String> = if view.history.is_some() {
                            Vec::new()
                        } else {
                            view.lines.iter().map(|l| l.text.to_string()).collect()
                        };
                        if let Some(screen) = view.displayed_screen() {
                            parts.extend(screen.rows.iter().map(|r| r.text.clone()));
                        }
                        let text = parts.join("\n");
                        crate::components::copy_text(cx, &text);
                    }
                    cx.notify();
                }),
            ),
        );
        menu = menu.child(
            item("tctx-clear", crate::i18n::label("clearTerminal")).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _win, cx| {
                    cx.stop_propagation();
                    this.terminal_ctx = None;
                    if let Some(view) = this.sessions.get_mut(this.active_session) {
                        view.lines.clear();
                        view.history = None;
                        view.history_pending = None;
                        view.selection = None;
                    }
                    cx.notify();
                }),
            ),
        );

        gpui_kit::anchored()
            .position(gpui_kit::point(px(x), px(y)))
            .snap_to_window_with_margin(px(4.))
            .child(menu)
    }

    pub(super) fn render_workspace_menu(
        &mut self,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let item = |id: &'static str, icon_name: &str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(11.))
                .h(px(35.))
                .w_full()
                .px(px(10.))
                .rounded(px(6.))
                .text_size(px(13.))
                .text_color(p.text)
                .hover(|s| s.bg(p.hover))
                .child(icon(icon_name).size(px(16.)).text_color(p.muted))
                .child(label)
        };
        div()
            .id("ws-menu-root")
            .absolute()
            .left(px(6.))
            .top(px(48.))
            .w(px(206.))
            .p(px(6.))
            .flex()
            .flex_col()
            .gap(px(3.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .rounded(px(9.))
            .shadow_lg()
            .occlude()
            .on_click(cx.listener(|this, _ev, _win, cx| {
                this.menu_open = false;
                cx.notify();
            }))
            .child(
                item("menu-agent-model", "spark", crate::i18n::label("agent")).on_click(
                    cx.listener(|this, _ev, _win, cx| {
                        this.set_config_page(Some(ConfigPage::AgentModel));
                        this.menu_open = false;
                        cx.notify();
                    }),
                ),
            )
            .child(
                item("menu-devtools", "cpu", crate::i18n::label("deviceTools")).on_click(
                    cx.listener(|this, _ev, _win, cx| {
                        this.set_config_page(Some(ConfigPage::Flash));
                        this.menu_open = false;
                        cx.notify();
                    }),
                ),
            )
            .child(
                item("menu-settings", "settings", crate::i18n::label("settings")).on_click(
                    cx.listener(|this, _ev, _win, cx| {
                        this.set_config_page(Some(ConfigPage::Settings));
                        this.menu_open = false;
                        cx.notify();
                    }),
                ),
            )
    }

    pub(super) fn render_connection_dock(
        &mut self,
        width: Pixels,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let creating = self.duts.editing_new_connection;
        let scoped = self.connection_dut_scope.is_some() || creating;
        div()
            .flex()
            .flex_col()
            .flex_none()
            .min_h_0()
            .w(width)
            .relative()
            .overflow_hidden()
            .bg(p.surface)
            .when(creating, |panel| {
                panel.rounded(px(12.)).border_1().border_color(p.border)
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(14.))
                    .h(px(44.))
                    .flex_none()
                    .border_b_1()
                    .border_color(p.line)
                    .child(crate::i18n::label(if self.duts.editing_new_connection {
                        "createConnection"
                    } else {
                        "connectionTitle"
                    }))
                    .child(
                        icon_btn("close-connection-dock", "x", p).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.connection_open = false;
                                this.connection_dut_scope = None;
                                this.open_dd = None;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(render_connection(self, p, window, cx, scoped))
            .when(!creating, |panel| {
                panel.child(PaneDivider::Connection.handle())
            })
    }

    pub(super) fn render_config_overlay(
        &mut self,
        page: ConfigPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.p;
        let title = match page {
            ConfigPage::AgentModel => crate::i18n::label("agent").to_string(),
            ConfigPage::Flash => crate::i18n::label("flashTitle").to_string(),
            ConfigPage::Adb => self.lang.usb_debug_title().to_string(),
            ConfigPage::Relay => crate::i18n::label("relayTitle").to_string(),
            ConfigPage::Serial => crate::i18n::label("serialTitle").to_string(),
            ConfigPage::Media => crate::i18n::label("deviceMonitor").to_string(),
            ConfigPage::ToolDownloads => crate::i18n::label("toolsDownload").to_string(),
            ConfigPage::Settings => i18n::t(self.lang, "settings").to_string(),
            ConfigPage::About => crate::i18n::label("aboutTitle").to_string(),
        };
        let search = self.cfg_search.clone();
        let query = search.trim().to_lowercase();
        let nav = |w: &mut Self,
                   id: &'static str,
                   icon_name: &str,
                   label: &'static str,
                   target: ConfigPage,
                   cx: &mut Context<Self>| {
            if !query.is_empty() && !label.to_lowercase().contains(&query) {
                return div().id(id);
            }
            let active = w.overlay == Some(target);
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(10.))
                .w_full()
                .py(px(9.))
                .px(px(10.))
                .rounded(px(8.))
                .text_size(px(14.))
                .text_color(if active { p.text } else { p.muted })
                .hover(|s| s.bg(p.hover).text_color(p.text))
                .when(active, |el| {
                    el.bg(p.hover).font_weight(gpui_kit::FontWeight(600.0))
                })
                .child(icon(icon_name).size(px(16.)).text_color(if active {
                    p.accent
                } else {
                    p.muted
                }))
                .child(label)
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.set_config_page(Some(target));
                    cx.notify();
                }))
        };
        let search_input = self
            .text_field(
                In::CfgSearch,
                &search,
                crate::i18n::label("searchSettings"),
                false,
                p,
                window,
                cx,
            )
            .prefix(icon("search").size(px(15.)).text_color(p.muted));
        let content = match page {
            ConfigPage::AgentModel => {
                crate::provider_models::render(self, p, window, cx).into_any_element()
            }
            ConfigPage::Flash => render_flash_tools(self, p, cx).into_any_element(),
            ConfigPage::Adb => render_usb_debug(self, p, window, cx).into_any_element(),
            ConfigPage::Relay => {
                crate::pages::render_relay_boards(self, p, window, cx).into_any_element()
            }
            ConfigPage::Serial => {
                crate::pages::render_serial_settings(self, p, window, cx).into_any_element()
            }
            ConfigPage::Media => crate::media_page::render(self, p, window, cx).into_any_element(),
            ConfigPage::ToolDownloads => {
                crate::pages::render_tool_downloads(self, p, window, cx).into_any_element()
            }
            ConfigPage::Settings => render_settings(self, p, window, cx).into_any_element(),
            ConfigPage::About => crate::pages::render_about(p).into_any_element(),
        };
        div()
            .absolute()
            .inset_0()
            .bg(p.backdrop)
            .flex()
            .items_center()
            .justify_center()
            .p(px(26.))
            .occlude()
            .child(
                div()
                    .w(px(1120.))
                    .max_w_full()
                    .h(px(760.))
                    .max_h_full()
                    .bg(p.surface)
                    .border_1()
                    .border_color(p.border)
                    .rounded(px(14.))
                    .shadow_lg()
                    .flex()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(260.))
                            .flex_none()
                            .bg(p.sidebar)
                            .border_r_1()
                            .border_color(p.line)
                            .p(px(12.))
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child(search_input)
                            .child(nav(
                                self,
                                "cfg-nav-agent-model",
                                "spark",
                                crate::i18n::label("agent"),
                                ConfigPage::AgentModel,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-flash",
                                "hard-drive",
                                crate::i18n::label("flashTitle"),
                                ConfigPage::Flash,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-adb",
                                "usb",
                                self.lang.usb_debug_title(),
                                ConfigPage::Adb,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-relays",
                                "bolt",
                                crate::i18n::label("relayTitle"),
                                ConfigPage::Relay,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-serial",
                                "terminal",
                                crate::i18n::label("serialTitle"),
                                ConfigPage::Serial,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-media",
                                "eye",
                                crate::i18n::label("deviceMonitor"),
                                ConfigPage::Media,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-tool-downloads",
                                "download",
                                crate::i18n::label("toolsDownload"),
                                ConfigPage::ToolDownloads,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-settings",
                                "settings",
                                crate::i18n::label("settings"),
                                ConfigPage::Settings,
                                cx,
                            ))
                            .child(nav(
                                self,
                                "cfg-nav-about",
                                "info",
                                crate::i18n::label("aboutTitle"),
                                ConfigPage::About,
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .min_h_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .h(px(52.))
                                    .px(px(16.))
                                    .border_b_1()
                                    .border_color(p.line)
                                    .child(
                                        div()
                                            .text_size(px(15.))
                                            .font_weight(gpui_kit::FontWeight(650.0))
                                            .text_color(p.text)
                                            .child(SharedString::from(title)),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .id("config-close")
                                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                                this.set_config_page(None);
                                                cx.notify();
                                            }))
                                            .size(px(30.))
                                            .rounded(px(7.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_color(p.muted)
                                            .hover(|s| s.bg(p.hover).text_color(p.text))
                                            .child(icon("x").size(px(16.)).text_color(p.muted)),
                                    ),
                            )
                            .child(
                                div()
                                    .id("config-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .pt(px(2.))
                                    .child(content),
                            ),
                    ),
            )
    }

    /// 主页磁贴单击：按已保存 DUT 配置直连。
    pub fn connect_dut(&mut self, dut_index: usize, cx: &mut Context<Self>) {
        let Some(dut) = self.duts.file.duts.get(dut_index).cloned() else {
            return;
        };
        let kind = if dut.serial.port == "DEMO" {
            duthub_device::SessionKind::Demo
        } else {
            duthub_device::SessionKind::Real
        };
        // Linux 设备路径在 Windows 上不可能打开：直接明确报错，不产生"重连中"僵尸会话
        #[cfg(target_os = "windows")]
        if kind == duthub_device::SessionKind::Real && dut.serial.port.starts_with("/dev/") {
            self.notify_toast(i18n::message(
                "linuxPortOnWindowsHint",
                &[("name", &dut.name), ("port", &dut.serial.port)],
            ));
            cx.notify();
            return;
        }
        let reference = boot_reference_path(&self.artifact_root, &dut);
        match self.device.open_named_session_with_log_options(
            &dut.id,
            self.duts.saved_name(&dut.id).unwrap_or(&dut.name),
            &dut.serial.port,
            dut.serial.baud,
            kind,
            duthub_device::SerialLogOptions {
                split: dut.serial.split_logs,
                reference: reference.as_deref(),
            },
        ) {
            Ok(session_id) => {
                let index = self.ensure_session(&session_id, &dut.id);
                self.sessions[index].title = format!("{} · {}", dut.name, dut.serial.port);
                self.active_session = index;
                self.view = ViewMode::Serial;
                self.terminal_focus_pending = true;
                self.notify_toast(i18n::message(
                    "connectingDutToast",
                    &[("name", &dut.name), ("port", &dut.serial.port)],
                ));
            }
            Err(err) => {
                let hint = if matches!(err, duthub_core::DeviceError::Unsupported) {
                    i18n::label("realHardwareDisabled").to_string()
                } else {
                    i18n::message("connectFailed", &[("error", &err.to_string())])
                };
                // 首开失败（DUT 磁贴直连）：toast + 弹窗提醒（需求⑤接入点②）
                self.report_serial_session_failure(
                    &dut.id,
                    &dut.name,
                    i18n::label("serialConnectFailedTitle"),
                    &hint,
                );
                self.notify_toast(hint);
            }
        }
        cx.notify();
    }

    /// 打开 kit Dialog：外壳（遮罩/动画/标题栏）由 kit 管理，
    /// 表单主体仍由 `render_modal_parts` 渲染（草稿状态留在 Workbench）。
    pub fn open_modal(&mut self, kind: ModalKind, window: &mut Window, cx: &mut Context<Self>) {
        for field in [
            In::CmdName,
            In::CmdValue,
            In::CmdIcon,
            In::NewPresetName,
            In::NewPresetExe,
            In::NewPresetTestArgs,
            In::PeerName,
            In::PeerUrl,
            In::RenameName,
        ] {
            self.text_editors.remove(&field);
        }
        let (draft, draft2, draft3) = match &kind {
            ModalKind::NewPreset => (
                "upgrade_tool".to_owned(),
                "upgrade_tool".to_owned(),
                "--help".to_owned(),
            ),
            ModalKind::EditPreset(id) => self
                .settings
                .flash_tools
                .iter()
                .find(|preset| preset.id == *id)
                .map_or_default(|preset| {
                    (
                        preset.name.clone(),
                        preset.executable.clone(),
                        preset.test_args.clone(),
                    )
                }),
            _ => Default::default(),
        };
        self.modal = Some(ModalState {
            kind,
            draft,
            draft2,
            draft3,
        });
        let view = cx.entity();
        let p = self.p;
        window.open_dialog(cx, move |dialog, window, cx| {
            let modal = view.update(cx, |w, _cx| {
                w.modal.clone().unwrap_or(ModalState {
                    kind: ModalKind::NewSession,
                    draft: String::new(),
                    draft2: String::new(),
                    draft3: String::new(),
                })
            });
            let (title, body) = view.update(cx, |w, cx| w.render_modal_parts(modal, p, window, cx));
            dialog.title(title).child(body)
        });
        cx.notify();
    }

    pub(super) fn render_modal_parts(
        &mut self,
        modal: ModalState,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (SharedString, gpui_kit::AnyElement) {
        let (title, body): (&str, gpui_kit::AnyElement) = match &modal.kind {
            ModalKind::RemoveDut(id) => (
                crate::i18n::label("deleteConnection"),
                div()
                    .text_size(px(12.5))
                    .text_color(p.muted)
                    .child(SharedString::from(i18n::message(
                        "deleteConnectionConfirm",
                        &[(
                            "name",
                            self.duts
                                .file
                                .duts
                                .iter()
                                .find(|dut| dut.id == *id)
                                .map_or(id.as_str(), |dut| dut.name.as_str()),
                        )],
                    )))
                    .into_any_element(),
            ),
            ModalKind::RemoveControlDevice(id) => (
                crate::i18n::label("deleteControlDevice"),
                div()
                    .text_size(px(12.5))
                    .text_color(p.muted)
                    .child(SharedString::from(crate::i18n::message(
                        "deleteControlDeviceBody",
                        &[(
                            "name",
                            self.duts
                                .file
                                .relay_boards
                                .iter()
                                .find(|board| board.id == *id)
                                .map_or(id.as_str(), |board| board.name.as_str()),
                        )],
                    )))
                    .into_any_element(),
            ),
            ModalKind::NewSession => {
                // 列出本机真实串口（打开弹窗时已刷新），点击直连 115200
                let mut body = div().flex().flex_col();
                if self.serial_ports.is_empty() {
                    body = body.child(
                        div()
                            .text_size(px(12.5))
                            .text_color(p.muted)
                            .child(crate::i18n::label("noSerialPorts")),
                    );
                }
                for (index, port_info) in self.serial_ports.iter().enumerate() {
                    let port = port_info.port.clone();
                    body = body.child(
                        div()
                            .id(SharedString::from(format!("modal-port-{index}")))
                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                this.modal = None;
                                this.open_real_port(&port, 115_200, cx);
                            }))
                            .flex()
                            .items_center()
                            .justify_between()
                            .w_full()
                            .px(px(11.))
                            .py(px(10.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.bg)
                            .mb(px(8.))
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.accent))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_family(theme::MONO_FONT)
                                    .text_color(p.text)
                                    .child(SharedString::from(port_info.port.clone())),
                            )
                            .child(div().text_size(px(11.)).text_color(p.faint).child(
                                SharedString::from(
                                    port_info.description.clone().unwrap_or_else(|| {
                                        crate::i18n::label("serialDevices").into()
                                    }),
                                ),
                            )),
                    );
                }
                (crate::i18n::label("newSerialTab"), body.into_any_element())
            }
            ModalKind::NewCommand => {
                let current_icon = self
                    .modal
                    .as_ref().map_or_else(|| "bolt".to_string(), |m| {
                        if m.draft3.is_empty() {
                            "bolt".to_string()
                        } else {
                            m.draft3.clone()
                        }
                    });
                let mut picker = div().flex().flex_wrap().gap(px(6.)).mb(px(12.));
                for name in [
                    "bolt", "terminal", "play", "power", "refresh", "check", "settings", "code",
                    "cpu", "network", "usb", "folder", "download", "upload", "alert", "bug",
                ] {
                    let selected = name == current_icon;
                    let value = name.to_string();
                    picker = picker.child(
                        div()
                            .id(SharedString::from(format!("cmd-icon-{name}")))
                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                if let Some(modal) = this.modal.as_mut() {
                                    modal.draft3 = value.clone();
                                }
                                cx.notify();
                            }))
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(30.))
                            .h(px(30.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(if selected { p.accent } else { p.border })
                            .bg(if selected { p.hover } else { p.bg })
                            .cursor_pointer()
                            .child(icon(name).size(px(15.)).text_color(if selected {
                                p.accent
                            } else {
                                p.muted
                            })),
                    );
                }
                let body = div()
                    .child(self.modal_text_input(
                        In::CmdName,
                        i18n::label("nameFieldLabel"),
                        "uname",
                        p,
                        window,
                        cx,
                    ))
                    .child(self.modal_text_input(
                        In::CmdValue,
                        crate::i18n::label("command"),
                        "uname -a",
                        p,
                        window,
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(12.))
                            .mb(px(6.))
                            .text_color(p.muted)
                            .child(crate::i18n::label("commandIcon")),
                    )
                    .child(picker);
                (crate::i18n::label("customCommand"), body.into_any_element())
            }
            ModalKind::NewPreset | ModalKind::EditPreset(_) => {
                let body = div()
                    .child(self.modal_text_input(
                        In::NewPresetName,
                        crate::i18n::label("presetName"),
                        "upgrade_tool",
                        p,
                        window,
                        cx,
                    ))
                    .child(self.modal_text_input(
                        In::NewPresetExe,
                        crate::i18n::label("executable"),
                        "upgrade_tool",
                        p,
                        window,
                        cx,
                    ))
                    .child(self.modal_text_input(
                        In::NewPresetTestArgs,
                        crate::i18n::label("flashTestArgs"),
                        "--help",
                        p,
                        window,
                        cx,
                    ));
                (
                    i18n::label(if matches!(modal.kind, ModalKind::EditPreset(_)) {
                        "editFlashTool"
                    } else {
                        "addFlashTool"
                    }),
                    body.into_any_element(),
                )
            }
            ModalKind::NewPeer => {
                let body = div()
                    .child(self.modal_text_input(
                        In::PeerName,
                        i18n::label("nameFieldLabel"),
                        "Lab Peer Agent",
                        p,
                        window,
                        cx,
                    ))
                    .child(self.modal_text_input(
                        In::PeerUrl,
                        "Agent Card URL",
                        "https://…/.well-known/agent-card.json",
                        p,
                        window,
                        cx,
                    ));
                (crate::i18n::label("addPeerAgent"), body.into_any_element())
            }
        };
        type ConfirmFn = Box<dyn Fn(&mut Workbench, &mut Context<Workbench>) + 'static>;
        let (confirm_label, on_confirm): (&str, ConfirmFn) = match &modal.kind {
            ModalKind::RemoveDut(id) => (
                crate::i18n::label("delete"),
                Box::new({
                    let id = id.clone();
                    move |w: &mut Workbench, cx: &mut Context<Workbench>| {
                        match w.remove_dut(&id, cx) {
                            Ok(()) => w.modal = None,
                            Err(err) => w.notify_toast(i18n::message(
                                "deleteConnectionFailed",
                                &[("error", &err)],
                            )),
                        }
                        cx.notify();
                    }
                }),
            ),
            ModalKind::RemoveControlDevice(id) => (
                crate::i18n::label("delete"),
                Box::new({
                    let id = id.clone();
                    move |w: &mut Workbench, cx: &mut Context<Workbench>| {
                        match w.remove_control_device(&id) {
                            Ok(()) => w.modal = None,
                            Err(err) => w.notify_toast(err),
                        }
                        cx.notify();
                    }
                }),
            ),
            ModalKind::NewCommand => (
                crate::i18n::label("add"),
                Box::new(|w: &mut Workbench, cx: &mut Context<Workbench>| {
                    let (name, value) = w
                        .modal
                        .as_ref()
                        .map_or_default(|m| (m.draft.clone(), m.draft2.clone()));
                    if name.trim().is_empty() || value.trim().is_empty() {
                        w.notify_toast(crate::i18n::label("nameAndCommandRequired"));
                        return;
                    }
                    let icon = w.input_value(In::CmdIcon);
                    let icon = if icon.trim().is_empty() {
                        "bolt".to_string()
                    } else {
                        icon.trim().to_string()
                    };
                    w.settings.commands.push(QuickCommand {
                        name: name.trim().to_string(),
                        value: value.trim().to_string(),
                        icon,
                    });
                    w.persist_settings();
                    w.modal = None;
                    cx.notify();
                }),
            ),
            ModalKind::NewPreset | ModalKind::EditPreset(_) => {
                let id = match &modal.kind {
                    ModalKind::EditPreset(id) => Some(id.clone()),
                    _ => None,
                };
                (
                    i18n::label(if id.is_some() { "save" } else { "add" }),
                    Box::new(move |w, cx| {
                        let Some(draft) = w.modal.as_ref() else {
                            return;
                        };
                        let name = draft.draft.trim();
                        let executable = draft.draft2.trim();
                        if name.is_empty() || executable.is_empty() {
                            w.notify_toast(i18n::label("nameAndExecutableRequired"));
                            return;
                        }
                        let mut candidate = w.settings.clone();
                        let preset = duthub_core::model::FlashToolPreset {
                            id: id.clone().unwrap_or_default(),
                            name: name.to_owned(),
                            executable: executable.to_owned(),
                            test_args: draft.draft3.trim().to_owned(),
                        };
                        if let Some(id) = &id {
                            let Some(saved) = candidate
                                .flash_tools
                                .iter_mut()
                                .find(|saved| saved.id == *id)
                            else {
                                return;
                            };
                            *saved = preset;
                        } else {
                            candidate.flash_tools.push(preset);
                            store::normalize_preset_ids(&mut candidate.flash_tools);
                        }
                        if let Err(err) = store::save_settings_secured(&w.data_dir, &candidate) {
                            w.notify_toast(format!("{} {err}", i18n::label("saveFailed")));
                            return;
                        }
                        w.settings = candidate;
                        if let Some(id) = &id {
                            w.flash_preset_tests.remove(id);
                        }
                        w.modal = None;
                        cx.notify();
                    }),
                )
            }
            ModalKind::NewPeer => (
                crate::i18n::label("add"),
                Box::new(|w: &mut Workbench, cx: &mut Context<Workbench>| {
                    let (name, url) = w
                        .modal
                        .as_ref()
                        .map_or_default(|m| (m.draft.clone(), m.draft2.clone()));
                    if name.trim().is_empty() || url.trim().is_empty() {
                        w.notify_toast(crate::i18n::label("nameAndAddressRequired"));
                        return;
                    }
                    let a2a = w
                        .settings
                        .a2a
                        .get_or_insert_with(duthub_core::A2aConfig::default);
                    a2a.peers.push(duthub_core::A2aPeer {
                        id: format!("peer-{}", a2a.peers.len() + 1),
                        name: name.trim().to_string(),
                        card_url: url.trim().to_string(),
                    });
                    w.persist_settings();
                    w.modal = None;
                    cx.notify();
                }),
            ),
            ModalKind::NewSession => ("", Box::new(|_w, _cx| {})),
        };
        let mut col = div().flex().flex_col().child(body);
        if !confirm_label.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(10.))
                    .mt(px(17.))
                    .child(
                        div()
                            .id("modal-cancel")
                            .flex()
                            .items_center()
                            .min_h(px(32.0))
                            .px(px(12.0))
                            .rounded(px(8.0))
                            .bg(p.raised)
                            .border_1()
                            .border_color(p.border)
                            .text_size(px(13.0))
                            .text_color(p.text)
                            .cursor_pointer()
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.modal = None;
                                window.close_dialog(cx);
                                cx.notify();
                            }))
                            .child(crate::i18n::label("cancel")),
                    )
                    .child(
                        div()
                            .id("modal-confirm")
                            .flex()
                            .items_center()
                            .min_h(px(32.0))
                            .px(px(14.0))
                            .rounded(px(8.0))
                            .bg(
                                if matches!(
                                    &modal.kind,
                                    ModalKind::RemoveDut(_) | ModalKind::RemoveControlDevice(_)
                                ) {
                                    p.red
                                } else {
                                    p.accent
                                },
                            )
                            .text_size(px(13.0))
                            .font_weight(gpui_kit::FontWeight(700.0))
                            .text_color(p.accent_contrast)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                on_confirm(this, cx);
                                // 确认逻辑成功后置 modal=None（校验失败保留输入）
                                if this.modal.is_none() {
                                    window.close_dialog(cx);
                                }
                            }))
                            .child(SharedString::from(confirm_label.to_string())),
                    ),
            );
        }
        (
            SharedString::from(title.to_string()),
            col.into_any_element(),
        )
    }

    pub(super) fn modal_text_input(
        &mut self,
        id: In,
        label: &str,
        placeholder: &str,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input = if id == In::NewPresetExe {
            self.path_input(id, placeholder, p, window, cx)
                .into_any_element()
        } else {
            self.text_field(id, &self.input_value(id), placeholder, false, p, window, cx)
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .mb(px(14.))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .child(label.to_owned()),
            )
            .child(input)
            .into_any_element()
    }
}
