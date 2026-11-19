use super::*;

// ---------------------------------------------------------------------------
// 连接管理
// ---------------------------------------------------------------------------

pub fn render_connection(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
    scoped: bool,
) -> impl IntoElement {
    let mut page = div()
        .flex()
        .flex_col()
        .min_h_0()
        .max_w(px(980.0))
        .mx_auto()
        .w_full()
        .px(px(12.))
        .pb(px(16.));
    if !scoped {
        let mut tabs = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(3.))
            .min_h(px(40.))
            .mt(px(14.))
            .border_b_1()
            .border_color(p.border);
        for index in 0..w.duts.file.duts.len() {
            let (name, id, active) = (
                w.duts.file.duts[index].name.clone(),
                w.duts.file.duts[index].id.clone(),
                !w.duts.editing_new_connection && index == w.duts.selected,
            );
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("dut-tab-{index}")))
                    .on_click(cx.listener(move |this, _ev, _win, cx| {
                        this.select_connection(index);
                        cx.notify();
                    }))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_w(px(120.))
                    .max_w(px(230.))
                    .px(px(12.))
                    .h(px(32.))
                    .rounded_t(px(6.))
                    .text_size(px(12.))
                    .text_color(if active { p.text } else { p.muted })
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .when(active, |el| {
                        el.bg(p.surface)
                            .border_1()
                            .border_b_0()
                            .border_color(p.border)
                    })
                    .child(div().size(px(6.)).rounded_full().bg(if active {
                        p.accent
                    } else {
                        p.faint
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(name)),
                    )
                    .child(
                        icon_btn(format!("delete-dut-tab-{index}"), "x", p)
                            .w(px(21.))
                            .h(px(21.))
                            .flex_none()
                            .tooltip(crate::i18n::label("deleteConnection"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Err(error) = this.remove_dut(&id, cx) {
                                    this.notify_toast(format!(
                                        "{} {error}",
                                        crate::i18n::label("saveFailed")
                                    ));
                                }
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(draft) = &w.duts.new_connection {
            let title = draft.dut.name.trim();
            tabs = tabs.child(
                div()
                    .id("new-connection-tab")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(32.))
                    .px(px(8.))
                    .max_w(px(230.))
                    .rounded_t(px(6.))
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .hover(|el| el.bg(p.hover).text_color(p.text))
                    .when(w.duts.editing_new_connection, |el| {
                        el.bg(p.raised).text_color(p.text)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_new_connection();
                        cx.notify();
                    }))
                    .child(icon("plus").size(px(14.)).text_color(p.accent))
                    .child(div().min_w_0().truncate().child(SharedString::from(
                        if title.is_empty() {
                            crate::i18n::label("createConnection")
                        } else {
                            title
                        },
                    )))
                    .child(
                        icon_btn("cancel-new-connection-tab", "x", p)
                            .flex_none()
                            .tooltip(crate::i18n::label("cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.cancel_new_connection();
                                cx.notify();
                            })),
                    ),
            );
        }
        page = page.child(
            tabs.child(
                div()
                    .id("add-dut")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.begin_new_connection(false, cx);
                        cx.notify();
                    }))
                    .ml(px(5.))
                    .mt(px(4.))
                    .size(px(30.))
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(p.muted)
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .child(icon("plus").size(px(16.)).text_color(p.muted)),
            ),
        );
    }
    if w.duts.selected().is_none() {
        return page;
    }
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(
            div()
                .id(if w.duts.editing_new_connection {
                    "create-connection-scroll"
                } else {
                    "connection-scroll"
                })
                .min_h_0()
                .flex_1()
                .overflow_y_scroll()
                .child(
                    page.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(16.))
                            .pt(px(16.))
                            .child(serial_panel(w, p, window, cx))
                            .child(mcp_panel(w, p, window, cx))
                            .child(adb_panel(w, p, window, cx))
                            .child(relay_panel(w, p, cx))
                            .child(flash_panel(w, p, cx))
                            .when(!w.duts.editing_new_connection, |form| {
                                form.child(connection_agent_panel(w, p, window, cx))
                            })
                            .child(heartbeat_panel(w, p, window, cx))
                            .child(serial_logs_panel(w, p, window, cx))
                            .child(alerts_panel(w, p, cx))
                            .child(crate::media_page::render_bindings(w, p, window, cx))
                            .child(a2a_dut_panel(w, p, window, cx)),
                    ),
                ),
        )
        .child(
            div()
                .border_t_1()
                .border_color(p.line)
                .child(actions_row(w, p, cx)),
        )
}

/// 心跳面板（需求⑦）：每 DUT 心跳配置行（启用/间隔/自定义命令）+ 最近一次状态。
/// 配置写 `dut.heartbeat`（保存落盘）并同步到心跳引擎运行时；
/// Agent 可经 heartbeat.set 改写命令（引擎是运行时单一归属，保存前回填）。
pub(super) fn serial_logs_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    use crate::i18n::label;

    let Some(dut) = w.duts.selected() else {
        return div();
    };
    let enabled = dut.serial.split_logs;
    let dut_id = dut.id.clone();
    let header = panel_head("file-text", label("splitSerialLogs"), p).child(
        toggle("opt-split-serial-logs", enabled, p).on_click(cx.listener(
            move |this, enabled: &bool, _, cx| {
                if let Some(dut) = this.duts.connection_mut(&dut_id) {
                    dut.serial.split_logs = *enabled;
                    this.duts.mark_connection_dirty();
                }
                cx.notify();
            },
        )),
    );
    let body = enabled.then(|| {
        let minimum = w.input_value(In::BootSimilarity);
        let default = duthub_core::model::DEFAULT_BOOT_SIMILARITY.to_string();
        let mut body = field(
            label("bootReferencePath"),
            w.path_input(In::BootReference, "", p, window, cx),
            None,
            p,
        )
        .child(setting_row(
            label("bootMinimumSimilarity"),
            "",
            div().w(px(82.)).child(w.text_field(
                In::BootSimilarity,
                &minimum,
                &default,
                true,
                p,
                window,
                cx,
            )),
            p,
        ));
        if let Some(dut_id) = w.duts.selected().map(|dut| dut.id.clone()) {
            let title = if w.boot_learning_dut.as_deref() == Some(dut_id.as_str()) {
                label("bootLearningRunning")
            } else {
                label("bootLearningStart")
            };
            body = body.child(
                div().flex().justify_end().child(
                    btn("learn-boot-reference", title, &BtnVariant::Tiny, p)
                        .disabled(!w.can_learn_boot_reference(&dut_id))
                        .on_click(cx.listener(move |_, _, window, cx| {
                            if window.has_active_dialog(cx) {
                                return;
                            }
                            let view = cx.entity();
                            let target = dut_id.clone();
                            window.open_dialog(cx, move |dialog, _, _| {
                                let view = view.clone();
                                let target = target.clone();
                                dialog
                                    .title(label("bootLearningStart"))
                                    .child(label("bootLearningConfirm"))
                                    .footer(
                                        // Hand-drawn button inside Dialog overlay (gpui-kit Button loses text in dialogs).
                                        div().flex().justify_end().child(
                                            div()
                                                .id("confirm-boot-learning")
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .min_h(px(32.0))
                                                .px(px(14.0))
                                                .rounded(px(8.0))
                                                .bg(p.accent)
                                                .text_size(px(13.0))
                                                .font_weight(gpui_kit::FontWeight(700.0))
                                                .text_color(p.accent_contrast)
                                                .cursor_pointer()
                                                .hover(|s| s.opacity(0.9))
                                                .child(label("bootLearningStart"))
                                                .on_click(move |_, window, cx| {
                                                    window.close_dialog(cx);
                                                    view.update(cx, |w, cx| {
                                                        w.start_boot_learning(&target, cx)
                                                    });
                                                }),
                                        ),
                                    )
                            });
                        })),
                ),
            );
        }
        body
    });
    collapsible_panel(header, body, p)
}

pub(super) fn heartbeat_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let state = w.heartbeat.state_or_default(&dut.id);
    let config = w.heartbeat_config(&dut.id);
    let header = switch_panel_header(
        "activity",
        crate::i18n::label("sessionHeartbeat"),
        toggle("hb-enable", config.enabled, p).on_click(cx.listener(|this, _, _, cx| {
            this.toggle_heartbeat();
            cx.notify();
        })),
        p,
    );
    if !config.enabled {
        return collapsible_panel(header, None, p);
    }
    let mut body = div();
    // 多行命令独占一行表单宽度，便于编辑和检查顺序。
    let interval_value = w.input_value(In::HeartbeatInterval);
    body = body
        .child(field(
            crate::i18n::label("heartbeatCommand"),
            w.multiline_field(
                In::HeartbeatCommand,
                &config.command,
                "uname -a\nuptime",
                p,
                window,
                cx,
            ),
            Some(crate::i18n::label("heartbeatCommandHint")),
            p,
        ))
        .child(
            div()
                .grid()
                .grid_cols(2)
                .gap(px(11.))
                .child(field(
                    crate::i18n::label("heartbeatInterval"),
                    w.text_field(
                        In::HeartbeatInterval,
                        &interval_value,
                        "30",
                        false,
                        p,
                        window,
                        cx,
                    ),
                    None,
                    p,
                ))
                .child(field(
                    crate::i18n::label("expectedResponse"),
                    w.text_field(
                        In::HeartbeatExpectedResponse,
                        config.expected_response.as_deref().unwrap_or_default(),
                        crate::i18n::label("responseExample"),
                        true,
                        p,
                        window,
                        cx,
                    ),
                    Some(crate::i18n::label("heartbeatResponseHint")),
                    p,
                )),
        );
    body = body.child(subtle(
        &crate::i18n::message(
            "heartbeatLossHint",
            &[("count", &config.loss_threshold.to_string())],
        ),
        p,
    ));
    let now_ms = duthub_device::heartbeat::now_unix_ms();
    let (status_label, status_color) =
        match (config.enabled, config.paused_until_unix_ms, state.last_ok) {
            (false, _, _) => (crate::i18n::label("heartbeatDisabled").to_string(), p.faint),
            (true, Some(until), _) if until > now_ms => (
                crate::i18n::message(
                    "heartbeatPausedSummary",
                    &[("secs", &(until - now_ms).div_ceil(1000).to_string())],
                ),
                p.muted,
            ),
            (true, _, Some(true)) => (crate::i18n::label("heartbeatOk").to_string(), p.prompt),
            (true, _, Some(false)) => (
                crate::i18n::message(
                    "heartbeatLostCount",
                    &[("count", &state.loss_count.to_string())],
                ),
                p.red,
            ),
            (true, _, None) => (crate::i18n::label("noProbeYet").to_string(), p.faint),
        };
    let mut summary = status_label;
    if let Some(h) = &state.sysinfo.hostname {
        summary.push_str(&format!(" · {h}"));
    }
    if let Some(u) = &state.sysinfo.uptime {
        summary.push_str(&format!(" · up {u}"));
    }
    if let Some(c) = &state.sysinfo.cpu {
        summary.push_str(&format!(" · {c}"));
    }
    if let Some(m) = &state.sysinfo.mem {
        summary.push_str(&format!(" · {m}"));
    }
    body = body.child(
        div()
            .pt(px(10.))
            .border_t_1()
            .border_color(p.line)
            .text_size(px(11.))
            .font_family(crate::theme::MONO_FONT)
            .text_color(status_color)
            .child(SharedString::from(summary)),
    );
    collapsible_panel(header, Some(body), p)
}

pub(crate) fn panel_shell(
    title_icon: &str,
    title: &str,
    body: Div,
    p: &'static Palette,
) -> gpui_kit::Div {
    panel_shell_with_header(panel_head(title_icon, title, p), body, p)
}

pub(super) fn panel_shell_with_header(header: Div, body: Div, p: &'static Palette) -> Div {
    collapsible_panel(header, Some(body), p)
}

pub(crate) fn collapsible_panel(header: Div, body: Option<Div>, p: &'static Palette) -> Div {
    div()
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
        .rounded(px(12.))
        .min_w_0()
        .border_b_1()
        .border_color(p.line)
        .child(header.when(body.is_none(), |header| header.border_b_0()))
        .children(body.map(|body| div().px(px(18.)).py(px(18.)).max_w(px(780.)).child(body)))
}

pub(crate) fn switch_panel_header(
    icon: &str,
    title: &str,
    switch: Switch,
    p: &'static Palette,
) -> Div {
    panel_head(icon, title, p).child(div().flex_none().child(switch))
}

pub(super) fn serial_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let mut body = div();
    let name = w.text_field(In::DutName, &dut.name, "", false, p, window, cx);
    body = body.child(field(crate::i18n::label("dutName"), name, None, p));
    let port_options: Vec<(String, String)> = w
        .serial_ports
        .iter()
        .map(|pi| {
            (
                pi.port.clone(),
                format!(
                    "{} · {}",
                    pi.port,
                    pi.description.clone().unwrap_or_default()
                ),
            )
        })
        .collect();
    body = body.child(field(
        crate::i18n::t(w.lang, "portManual"),
        w.combobox(
            In::Port,
            crate::workspace::Dd::Port,
            &dut.serial.port,
            if cfg!(windows) {
                "COM3"
            } else {
                "/dev/ttyUSB0"
            },
            true,
            &port_options,
            window,
            cx,
        ),
        None,
        p,
    ));
    // 波特率 + 数据格式
    let baud_options: Vec<(String, String)> = BAUD_OPTIONS
        .iter()
        .map(|b| (b.to_string(), b.to_string()))
        .collect();
    let two = div()
        .grid()
        .grid_cols(2)
        .gap(px(11.))
        .child(field(
            crate::i18n::t(w.lang, "baud"),
            w.combobox(
                In::Baud,
                crate::workspace::Dd::Baud,
                &dut.serial.baud.to_string(),
                "115200",
                true,
                &baud_options,
                window,
                cx,
            ),
            None,
            p,
        ))
        .child(field(
            crate::i18n::t(w.lang, "serialFormat"),
            w.select_field(
                "cfg-format",
                dut.serial.format.clone(),
                vec![
                    ("8N1".to_string(), 0),
                    ("8E1".to_string(), 1),
                    ("8O1".to_string(), 2),
                    ("7E1".to_string(), 3),
                ],
                Some(format_index(&dut.serial.format)),
                std::sync::Arc::new(
                    move |this: &mut Workbench,
                          value: usize,
                          _win: &mut Window,
                          cx: &mut Context<Workbench>| {
                        if let Some(dut) = this.duts.selected_mut() {
                            dut.serial.format = ["8N1", "8E1", "8O1", "7E1"][value].to_string();
                            this.duts.mark_connection_dirty();
                        }
                        cx.notify();
                    },
                ),
                p,
                cx,
            ),
            None,
            p,
        ));
    body = body.child(two);
    panel_shell("terminal", crate::i18n::t(w.lang, "serialBinding"), body, p)
}

pub(super) fn format_index(format: &str) -> usize {
    ["8N1", "8E1", "8O1", "7E1"]
        .iter()
        .position(|f| *f == format)
        .unwrap_or(0)
}

pub(super) fn usb_debug_display(
    discovery: Option<&Result<Vec<duthub_core::AdbDevice>, String>>,
    bound: &str,
    hdc: bool,
    lang: crate::i18n::Lang,
) -> (Vec<(String, String)>, String) {
    let options = discovery
        .and_then(|result| result.as_ref().ok())
        .into_iter()
        .flatten()
        .map(|d| {
            let label = d.model.as_ref().map_or_else(
                || format!("{} ({})", d.serial, d.state),
                |model| format!("{} · {model} ({})", d.serial, d.state),
            );
            (d.serial.clone(), label)
        })
        .collect::<Vec<_>>();
    let status = match discovery {
        None => crate::i18n::t(lang, "detectingDevices").to_owned(),
        Some(Err(err)) => crate::i18n::message("usbDiscoveryFailed", &[("error", err)]),
        Some(Ok(devices)) if devices.is_empty() => crate::i18n::t(
            lang,
            if hdc {
                "noHdcDetected"
            } else {
                "noAdbDetected"
            },
        )
        .into(),
        Some(Ok(_)) if bound.is_empty() => crate::i18n::t(lang, "noUsbBinding").into(),
        Some(Ok(devices)) => match devices.iter().find(|device| device.serial == bound) {
            None => crate::i18n::t(lang, "usbBindingDisconnected").into(),
            Some(device) if !hdc && device.state != "device" => {
                crate::i18n::message("usbBindingNotReady", &[("state", &device.state)])
            }
            Some(_) => crate::i18n::t(lang, "usbBindingDetected").into(),
        },
    };
    (options, status)
}

pub(super) fn adb_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let lang = w.lang;
    let hdc = dut.adb.tool == "hdc";
    let enabled = crate::workspace::usb_debug_enabled(&dut.adb);
    let tool = if hdc { "hdc" } else { "adb" };
    let header = switch_panel_header(
        "usb",
        w.lang.usb_debug_title(),
        toggle("dut-usb-debug-toggle", enabled, p).on_click(cx.listener(
            move |this, checked: &bool, _, cx| {
                this.select_usb_debug_tool(checked.then_some(tool));
                this.open_dd = None;
                cx.notify();
            },
        )),
        p,
    );
    if !enabled {
        return collapsible_panel(header, None, p);
    }
    let selected = usize::from(hdc);
    let body = div().child(field(
        crate::i18n::t(lang, "debugTool"),
        w.select_field(
            "usb-debug-tool",
            ["ADB", "HDC"][selected].into(),
            vec![("ADB".into(), 0), ("HDC".into(), 1)],
            Some(selected),
            std::sync::Arc::new(|w, index, _, cx| {
                if let Some(tool) = ["adb", "hdc"].get(index).copied() {
                    w.select_usb_debug_tool(Some(tool));
                }
                cx.notify();
            }),
            p,
            cx,
        ),
        None,
        p,
    ));
    let discovery = w.usb_discovery(hdc).result.as_ref();
    let (options, status) = usb_debug_display(discovery, dut.adb.serial.trim(), hdc, lang);
    let body = body.child(field(
        if hdc {
            crate::i18n::t(lang, "boundHdcConnectKey")
        } else {
            crate::i18n::t(lang, "boundAdbSerial")
        },
        w.combobox(
            In::AdbSerial,
            crate::workspace::Dd::AdbSerial,
            &dut.adb.serial,
            crate::i18n::t(lang, "selectOrEnterADeviceId"),
            true,
            &options,
            window,
            cx,
        ),
        Some(&status),
        p,
    ));
    collapsible_panel(header, Some(body), p)
}

/// 只读地址行 + 复制按钮。
pub(super) fn endpoint_copy_row(
    label: &str,
    value: &str,
    id: &str,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let copy = value.to_owned();
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .py(px(3.))
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .child(subtle(label, p)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(11.))
                .font_family(crate::theme::MONO_FONT)
                .text_color(p.accent)
                .overflow_hidden()
                .child(SharedString::from(copy.clone())),
        )
        .child(
            icon_btn(id.to_owned(), "copy", p)
                .w(px(28.))
                .h(px(28.))
                .accessibility_label(crate::i18n::label("copySelection"))
                .tooltip(crate::i18n::label("copySelection"))
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    copy_text(cx, &copy);
                    this.notify_toast(crate::i18n::label("addressCopied"));
                    cx.notify();
                })),
        )
}

/// Read-only multi-line config snippet (`.mcp.json`, A2A discovery) with a copy
/// button. Each line renders as its own row so the text keeps its shape.
pub(super) fn config_block(
    id: &'static str,
    label: Option<&str>,
    text: &str,
    copy_label: &str,
    toast: &str,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let copy = text.to_owned();
    let toast = toast.to_owned();
    let mut lines = div().flex().flex_col().line_height(px(17.));
    for (index, line) in text.lines().enumerate() {
        lines = lines.child(
            div()
                .when(index < 2, |line| line.pr(px(30.)))
                .font_family(crate::theme::MONO_FONT)
                .text_size(px(11.))
                .text_color(p.text)
                .child(SharedString::from(line.to_owned())),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .py(px(4.))
        .children(label.map(|label| subtle(label, p)))
        .child(
            div()
                .relative()
                .rounded(px(6.))
                .bg(p.term)
                .p(px(10.))
                .overflow_hidden()
                .child(lines)
                .child(
                    div().absolute().top(px(5.)).right(px(5.)).child(
                        icon_btn(id, "copy", p)
                            .w(px(28.))
                            .h(px(28.))
                            .accessibility_label(copy_label.to_owned())
                            .tooltip(copy_label.to_owned())
                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                copy_text(cx, &copy);
                                this.notify_toast(toast.clone());
                                cx.notify();
                            })),
                    ),
                ),
        )
}

pub(super) fn mcp_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let saved = !w.duts.editing_new_connection;
    let lan_ip = detected_lan_ip(w);
    let endpoint = w.dut_mcp_endpoint(&dut.id);
    let port_value = w.input_value(In::DutMcpPort);
    let mut body = div();

    if saved && let Some((port, token)) = endpoint {
        if !w.settings.agent_access.enabled {
            body = body.child(subtle(crate::i18n::label("mcpEnableAuthHint"), p));
        }
        let port_field = w.text_field(
            In::DutMcpPort,
            &port_value,
            &port.to_string(),
            true,
            p,
            window,
            cx,
        );
        let local_url = format!("http://127.0.0.1:{port}/mcp");
        body = body.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .py(px(4.))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.))
                        .child(subtle(crate::i18n::label("mcpPort"), p))
                        .child(div().w(px(61.)).child(port_field))
                        .child(
                            btn(
                                "dut-mcp-save-port",
                                crate::i18n::label("save"),
                                &BtnVariant::Tiny,
                                p,
                            )
                            .tooltip(crate::i18n::label("mcpDutPortHint"))
                            .on_click(cx.listener({
                                let dut_id = dut.id.clone();
                                move |this, _ev, _win, cx| {
                                    this.save_dut_mcp_port(&dut_id);
                                    cx.notify();
                                }
                            })),
                        ),
                )
                .child(div().flex_1().min_w(px(220.)).child(endpoint_copy_row(
                    crate::i18n::label("mcpLocalAddress"),
                    &local_url,
                    "dut-mcp-copy-local",
                    p,
                    cx,
                ))),
        );
        // Surface per-port bind failures directly inside the panel.
        if let Some(reason) = w.mcp_runtime.failure_for_port(port) {
            body = body.child(
                subtle(
                    &crate::i18n::message(
                        "mcpBindFailure",
                        &[("port", &port.to_string()), ("reason", reason)],
                    ),
                    p,
                )
                .text_color(p.red),
            );
        }
        if let Some(ip) = lan_ip {
            let lan_url = format!("http://{ip}:{port}/mcp");
            body = body.child(endpoint_copy_row(
                crate::i18n::label("mcpLanAddress"),
                &lan_url,
                "dut-mcp-copy-lan",
                p,
                cx,
            ));
        }
        let regenerate = btn(
            "dut-mcp-regen-token",
            crate::i18n::label("mcpRegenerateToken"),
            &BtnVariant::Tiny,
            p,
        )
        .on_click(cx.listener({
            let dut_id = dut.id.clone();
            move |this, _, _, cx| {
                this.regenerate_dut_mcp_token(&dut_id);
                cx.notify();
            }
        }));
        if let Some(token) = token {
            let token_for_copy = token.clone();
            let expanded = w.expanded_mcp_config.as_deref() == Some(dut.id.as_str());
            let toggle_id = dut.id.clone();
            body = body
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .py(px(4.))
                        .child(subtle("Token", p))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .font_family(crate::theme::MONO_FONT)
                                .text_color(p.text)
                                .overflow_hidden()
                                .child(SharedString::from(token.clone())),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            btn(
                                "dut-mcp-copy-token",
                                crate::i18n::label("mcpCopyToken"),
                                &BtnVariant::Tiny,
                                p,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    copy_text(cx, &token_for_copy);
                                    this.notify_toast(crate::i18n::label("tokenCopied"));
                                    cx.notify();
                                },
                            )),
                        )
                        .child(regenerate)
                        .child(
                            btn("dut-mcp-config-toggle", ".mcp.json", &BtnVariant::Tiny, p)
                                .ml_auto()
                                .child(
                                    crate::icons::kit_icon(if expanded {
                                        "chevron-down"
                                    } else {
                                        "chevron-right"
                                    })
                                    .size(px(14.))
                                    .text_color(p.muted),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.expanded_mcp_config =
                                        (!expanded).then(|| toggle_id.clone());
                                    cx.notify();
                                })),
                        ),
                );
            if expanded {
                let server = duthub_desktop::agent_ui::mcp_server_name(&dut.id);
                let config_text =
                    duthub_desktop::agent_ui::mcp_client_config(&server, &local_url, &token);
                body = body.child(config_block(
                    "dut-mcp-config",
                    None,
                    &config_text,
                    crate::i18n::label("mcpCopyConfig"),
                    crate::i18n::label("mcpConfigCopied"),
                    p,
                    cx,
                ));
            }
        } else {
            body = body
                .child(subtle(crate::i18n::label("mcpCredentialMissing"), p).text_color(p.warn))
                .child(div().flex().child(regenerate));
        }
    } else {
        let status = if !saved {
            crate::i18n::label("mcpSaveConnectionForPort")
        } else if !w.settings.agent_access.enabled {
            crate::i18n::label("mcpEnableAuthHint")
        } else {
            crate::i18n::label("mcpNoDedicatedPort")
        };
        body = body.child(subtle(status, p));
    }
    panel_shell("link", crate::i18n::label("mcpServiceTitle"), body, p)
}

pub(crate) fn render_about(p: &'static Palette) -> Div {
    div()
        .p(px(24.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(div().text_size(px(24.)).text_color(p.text).child("duthub"))
        .child(
            div()
                .text_size(px(13.))
                .text_color(p.muted)
                .font_family(crate::theme::MONO_FONT)
                .child(concat!("v", env!("CARGO_PKG_VERSION"))),
        )
}
pub(super) fn a2a_dut_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let saved = !w.duts.editing_new_connection;
    let config = dut.a2a.clone();
    let port_value = w.input_value(In::DutA2aPort);
    let name_value = w.input_value(In::DutA2aName);
    let token_state = w.cached_a2a_token(&dut.id).cloned();
    // Token for the copyable client config; the token row below consumes the
    // full state (including read errors) separately.
    let a2a_token = token_state
        .as_ref()
        .and_then(|state| state.as_ref().ok().and_then(|token| token.clone()));
    let mut body = div();
    // W4a：运行时状态行（Running/Starting/失败原因），替代旧的
    // 「运行时尚未接入」静态提示。W4c：self-test 下运行时为 None → Disabled。
    let a2a_status = w
        .a2a
        .as_ref()
        .map_or(A2aStatus::Disabled, |a2a| a2a.status(&dut.id));
    let a2a_view = duthub_desktop::agent_ui::a2a_endpoint_view(&config, &a2a_status);
    if saved && config.enabled {
        let port_field = w.text_field(
            In::DutA2aPort,
            &port_value,
            crate::i18n::label("autoAssign"),
            true,
            p,
            window,
            cx,
        );
        let name_field = w.text_field(In::DutA2aName, &name_value, &dut.name, false, p, window, cx);
        body = body.child(field(crate::i18n::label("a2aPort"), port_field, None, p));
        body = body.child(field(
            crate::i18n::label("a2aAgentNameLabel"),
            name_field,
            None,
            p,
        ));
        body = body.child(
            div().flex().items_center().justify_end().gap(px(8.)).child(
                btn(
                    "dut-a2a-save",
                    crate::i18n::label("a2aSaveConfig"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener({
                    let dut_id = dut.id.clone();
                    move |this, _ev, _win, cx| {
                        this.save_dut_a2a(&dut_id);
                        cx.notify();
                    }
                })),
            ),
        );

        body = body.child(
            div()
                .text_size(px(13.))
                .text_color(if a2a_view.failed { p.red } else { p.text })
                .child(SharedString::from(a2a_view.status_text)),
        );
        if let Some(card_url) = a2a_view.card_url.clone() {
            body = body.child(endpoint_copy_row(
                crate::i18n::label("a2aLocalCardRunning"),
                &card_url,
                "dut-a2a-copy-local",
                p,
                cx,
            ));
        }
        if let (Some(card_url), Some(token)) = (a2a_view.card_url.as_deref(), a2a_token.as_deref())
        {
            let config_text = duthub_desktop::agent_ui::a2a_client_config(card_url, token);
            body = body.child(config_block(
                "dut-a2a-config",
                Some(crate::i18n::label("a2aClientConfigLabel")),
                &config_text,
                crate::i18n::label("a2aCopyConfig"),
                crate::i18n::label("a2aConfigCopied"),
                p,
                cx,
            ));
        }
        match token_state {
            Some(Ok(Some(token))) => {
                let token_for_copy = token.clone();
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .py(px(4.))
                        .child(subtle("Token", p))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .font_family(crate::theme::MONO_FONT)
                                .text_color(p.text)
                                .overflow_hidden()
                                .child(SharedString::from(token)),
                        )
                        .child(
                            chip("dut-a2a-copy-token", crate::i18n::label("mcpCopyToken"), p)
                                .on_click(cx.listener(move |this, _ev, _win, cx| {
                                    copy_text(cx, &token_for_copy);
                                    this.notify_toast(crate::i18n::label("a2aTokenCopied"));
                                    cx.notify();
                                })),
                        ),
                );
            }
            Some(Ok(None)) => {
                body =
                    body.child(subtle(crate::i18n::label("a2aTokenMissing"), p).text_color(p.warn));
            }
            Some(Err(error)) => {
                body = body.child(
                    subtle(
                        &crate::i18n::message("a2aTokenReadFailed", &[("error", &error)]),
                        p,
                    )
                    .text_color(p.red),
                );
            }
            None => {
                body = body.child(subtle(crate::i18n::label("a2aTokenNotLoaded"), p));
            }
        }
        body = body.child(
            div().flex().items_center().gap(px(8.)).child(
                chip(
                    "dut-a2a-regen-token",
                    crate::i18n::label("mcpRegenerateToken"),
                    p,
                )
                .on_click(cx.listener({
                    let dut_id = dut.id.clone();
                    move |this, _ev, _win, cx| {
                        this.regenerate_dut_a2a_token(&dut_id);
                        cx.notify();
                    }
                })),
            ),
        );
    }
    let header = switch_panel_header(
        "network",
        "A2A",
        toggle("dut-a2a-toggle", config.enabled, p)
            .disabled(!saved)
            .on_click(cx.listener(move |this, enabled, _, cx| {
                if *enabled {
                    this.enable_dut_a2a(&dut.id);
                } else {
                    this.disable_dut_a2a(&dut.id);
                }
                cx.notify();
            })),
        p,
    );
    collapsible_panel(header, (saved && config.enabled).then_some(body), p)
}

pub(super) fn flash_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let Some(dut) = w.duts.selected() else {
        return div();
    };
    let id = dut.id.clone();
    let choices = crate::data::flash_preset_labels(&w.settings.flash_tools);
    let enabled = !dut.flash.executable.trim().is_empty() || dut.flash.preset_id.is_some();
    let initial_preset = choices.first().map(|(_, id)| id.clone());
    let toggle_dut = id.clone();
    let header = switch_panel_header(
        "hard-drive",
        crate::i18n::label("flashTitle"),
        toggle("dut-flash-toggle", enabled, p)
            .disabled(!enabled && initial_preset.is_none())
            .on_click(cx.listener(move |this, checked, _, cx| {
                if *checked {
                    if let Some(id) = &initial_preset {
                        this.set_dut_flash_preset(&toggle_dut, id);
                    }
                } else {
                    this.set_dut_flash_preset(&toggle_dut, "");
                }
                cx.notify();
            })),
        p,
    );
    if !enabled {
        return collapsible_panel(header, None, p);
    }
    let selected = crate::data::flash_preset_index(&dut.flash, &w.settings.flash_tools);
    let label = selected.map_or_else(
        || dut.flash.executable.clone(),
        |index| choices[index].0.clone(),
    );
    let options = choices
        .iter()
        .enumerate()
        .map(|(ix, (label, _))| (label.clone(), ix))
        .collect();
    let select = w.select_field(
        "dut-flash-preset",
        label,
        options,
        selected,
        std::sync::Arc::new(move |this, index, _, cx| {
            if let Some((_, preset_id)) = choices.get(index) {
                this.set_dut_flash_preset(&id, preset_id);
                cx.notify();
            }
        }),
        p,
        cx,
    );
    let body = field(crate::i18n::label("chooseFlashTool"), select, None, p);
    collapsible_panel(header, Some(body), p)
}

pub(super) fn actions_row(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let creating = w.duts.editing_new_connection;
    let save_label = if creating {
        if w.duts
            .new_connection
            .as_ref()
            .is_some_and(|draft| draft.connect_after_save)
        {
            "createConnect"
        } else {
            "create"
        }
    } else {
        "saveCreation"
    };
    div()
        .flex()
        .items_center()
        .flex_wrap()
        .gap(px(8.))
        .px(px(18.))
        .py(px(15.))
        .when(!creating, |row| {
            row.child(
                div()
                    .mr_auto()
                    .text_size(px(11.))
                    .text_color(p.muted)
                    .child(SharedString::from(
                        if w.duts.dirty {
                            crate::i18n::label("unsavedChanges")
                        } else {
                            crate::i18n::label("bindingSaveHint")
                        }
                        .to_string(),
                    )),
            )
        })
        .children(if creating {
            None
        } else {
            connection_lease_action(w, p, cx)
        })
        .when(creating, |row| {
            row.justify_end().child(
                btn(
                    "cancel-new-connection",
                    crate::i18n::label("cancel"),
                    &BtnVariant::Normal,
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cancel_new_connection();
                    cx.notify();
                })),
            )
        })
        .child(
            btn(
                "save-connection",
                crate::i18n::label(save_label),
                &BtnVariant::Primary,
                p,
            )
            .on_click(cx.listener(|this, _ev, _win, cx| {
                this.save_connection(cx);
            }))
            .child(
                icon("check")
                    .size(px(15.))
                    .text_color(p.accent_contrast)
                    .into_any_element(),
            ),
        )
}
