//! 页面：连接管理（`#page-connection`）与设置（`#page-settings`），1:1 移植。
//!
//! 数据绑定：连接页编辑 `duts.json`（选中 DUT 草稿，保存落盘）；
//! 设置页编辑 `settings.json`（core::store 原子写）。

use duthub_core::model::{McpConfig, RelayChannels, RelayConfig};
use duthub_core::{DutAction, LeasePhase};
use gpui_kit::component::Disableable;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AppContext, Context, Div, InteractiveElement, IntoElement, ParentElement, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::components::{
    BtnVariant, btn, chip, copy_text, field, icon_btn, panel_head, subtle, toggle,
};
use crate::icons::icon;
use crate::theme::Palette;
use crate::workspace::{FlashPresetTest, In, Workbench};

pub const BAUD_OPTIONS: [&str; 20] = [
    "110", "300", "600", "1200", "2400", "4800", "9600", "14400", "19200", "38640", "38400",
    "56000", "57600", "115200", "128000", "230400", "256000", "460800", "921600", "1500000",
];

// ---------------------------------------------------------------------------
// 连接管理
// ---------------------------------------------------------------------------

pub fn render_connection(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
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
    // DUT tabs + add
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
        tabs =
            tabs.child(
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
                            .child(adb_panel(w, p, window, cx))
                            .child(relay_panel(w, p, cx))
                            .child(flash_panel(w, p, cx))
                            .child(mcp_panel(w, p, window, cx))
                            .child(heartbeat_panel(w, p, window, cx))
                            .child(alerts_panel(w, p, cx))
                            .child(crate::media_page::render_bindings(w, p, window, cx)),
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
fn heartbeat_panel(
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
    let mut body = div();
    // 启用开关行（点击翻转，同步心跳引擎运行时）
    body = body.child(
        div()
            .id("hb-enable")
            .on_click(cx.listener(move |this, _ev, _win, cx| {
                this.toggle_heartbeat();
                cx.notify();
            }))
            .flex()
            .items_center()
            .gap(px(9.))
            .min_h(px(34.))
            .text_color(p.text)
            .cursor_pointer()
            .child(check_box(config.enabled, p))
            .child(crate::i18n::label("enableHeartbeat")),
    );
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
        &format!(
            "保存绑定后应用。连续 {} 次未收到预期响应时提醒。",
            config.loss_threshold
        ),
        p,
    ));
    // 最近一次状态（含系统信息启发式提取结果）
    let now_ms = duthub_device::heartbeat::now_unix_ms();
    let (status_label, status_color) =
        match (config.enabled, config.paused_until_unix_ms, state.last_ok) {
            (false, _, _) => (crate::i18n::label("heartbeatDisabled").to_string(), p.faint),
            (true, Some(until), _) if until > now_ms => (
                format!("已暂停 · 约 {} 秒后恢复", (until - now_ms).div_ceil(1000)),
                p.muted,
            ),
            (true, _, Some(true)) => (crate::i18n::label("heartbeatOk").to_string(), p.prompt),
            (true, _, Some(false)) => (format!("心跳丢联 ×{}", state.loss_count), p.red),
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
    panel_shell("activity", crate::i18n::label("sessionHeartbeat"), body, p)
}

fn panel_shell(title_icon: &str, title: &str, body: Div, p: &'static Palette) -> gpui_kit::Div {
    div()
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
        .rounded(px(12.))
        .min_w_0()
        .border_b_1()
        .border_color(p.line)
        .child(panel_head(title_icon, title, p))
        .child(div().px(px(18.)).py(px(18.)).max_w(px(780.)).child(body))
}

fn serial_panel(
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
        Some(crate::i18n::label("serialPortHint")),
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
            Some(crate::i18n::t(w.lang, "customBaud")),
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

fn format_index(format: &str) -> usize {
    ["8N1", "8E1", "8O1", "7E1"]
        .iter()
        .position(|f| *f == format)
        .unwrap_or(0)
}

fn usb_debug_display(
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

fn adb_panel(
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
    let selected = if enabled { if hdc { 2 } else { 1 } } else { 0 };
    let mut body = div().child(field(
        crate::i18n::t(lang, "debugTool"),
        w.select_field(
            "usb-debug-tool",
            ["None", "ADB", "HDC"][selected].into(),
            vec![("None".into(), 0), ("ADB".into(), 1), ("HDC".into(), 2)],
            Some(selected),
            std::sync::Arc::new(|w, index, _, cx| {
                w.select_usb_debug_tool([None, Some("adb"), Some("hdc")][index]);
                cx.notify();
            }),
            p,
            cx,
        ),
        None,
        p,
    ));
    if enabled {
        let discovery = w.usb_discovery(hdc).result.as_ref();
        let (options, status) = usb_debug_display(discovery, dut.adb.serial.trim(), hdc, lang);
        body = body.child(field(
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
    }
    panel_shell("usb", w.lang.usb_debug_title(), body, p)
}

pub(crate) fn render_relay_boards(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let lang = w.lang;
    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .w_full()
        .max_w(px(980.))
        .mx_auto()
        .p(px(24.));
    let options: Vec<_> = w
        .duts
        .file
        .relay_boards
        .iter()
        .enumerate()
        .map(|(i, board)| (format!("{} · {}", board.name, board.node), i))
        .collect();
    if let Some(board) = w
        .duts
        .file
        .relay_boards
        .get(w.relay_board_selected)
        .cloned()
    {
        body = body.child(field(
            crate::i18n::t(lang, "relayBoard"),
            w.select_field(
                "relay-board-select",
                format!("{} · {}", board.name, board.node),
                options,
                Some(w.relay_board_selected),
                std::sync::Arc::new(|this, index, _, cx| {
                    this.relay_board_selected = index;
                    cx.notify();
                }),
                p,
                cx,
            ),
            None,
            p,
        ));
        if w.relay_board_name
            .as_ref()
            .is_none_or(|(id, _)| *id != board.id)
        {
            let id = board.id.clone();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(board.name.clone()));
            let target = id.clone();
            cx.subscribe(&input, move |w, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(board) = w
                        .duts
                        .file
                        .relay_boards
                        .iter_mut()
                        .find(|board| board.id == target)
                {
                    board.name = input.read(cx).value().to_string();
                    w.duts.dirty = true;
                    cx.notify();
                }
            })
            .detach();
            w.relay_board_name = Some((id, input));
        }
        let name = &w.relay_board_name.as_ref().expect("relay name input").1;
        body = body.child(field(
            crate::i18n::t(lang, "name"),
            Input::new(name),
            None,
            p,
        ));
        let supported_driver = "CH340";
        let driver_label = crate::i18n::label("ch340RelayDriver");
        let driver_selected =
            board.driver.is_empty() || board.driver.eq_ignore_ascii_case(supported_driver);
        let id = board.id.clone();
        body = body.child(field(
            crate::i18n::label("controlDeviceDriver"),
            w.select_field(
                "control-device-driver",
                if driver_selected {
                    driver_label.to_owned()
                } else {
                    board.driver.clone()
                },
                vec![(driver_label.to_owned(), 0)],
                driver_selected.then_some(0),
                std::sync::Arc::new(move |this, _, _, cx| {
                    if let Some(board) = this
                        .duts
                        .file
                        .relay_boards
                        .iter_mut()
                        .find(|board| board.id == id)
                    {
                        board.driver = supported_driver.into();
                        this.duts.dirty = true;
                        cx.notify();
                    }
                }),
                p,
                cx,
            ),
            None,
            p,
        ));
        let ports = w
            .serial_ports
            .iter()
            .map(|port| {
                let name = port.port.clone();
                let label = port.description.as_ref().map_or_else(
                    || name.clone(),
                    |description| format!("{name} · {description}"),
                );
                (name, label)
            })
            .collect::<Vec<_>>();
        let node = w.combobox(
            In::RelayBoardNode,
            crate::workspace::Dd::RelayBoardPort,
            &board.node,
            "COM25",
            true,
            &ports,
            window,
            cx,
        );
        body = body.child(field(
            crate::i18n::t(lang, "relaySerialPort"),
            div()
                .flex()
                .items_start()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().child(node))
                .child(
                    icon_btn("relay-port-refresh", "refresh", p)
                        .tooltip(crate::i18n::t(lang, "refreshSerialPorts"))
                        .on_click(cx.listener(|w, _, _, cx| {
                            w.serial_ports = w.device.list_ports();
                            cx.notify();
                        })),
                ),
            Some(crate::i18n::t(lang, "selectAConnectedPortOrType")),
            p,
        ));
        body = body.child(field(
            crate::i18n::t(lang, "channelCount"),
            w.select_field(
                "relay-board-channel-count",
                board.channel_count.to_string(),
                (1..=usize::from(board.channel_count.max(8)))
                    .map(|count| (count.to_string(), count))
                    .collect(),
                Some(usize::from(board.channel_count)),
                std::sync::Arc::new(|this, count, _, cx| {
                    if let Some(board) = this
                        .duts
                        .file
                        .relay_boards
                        .get_mut(this.relay_board_selected)
                    {
                        board.channel_count = count as u8;
                        this.duts.dirty = true;
                    }
                    cx.notify();
                }),
                p,
                cx,
            ),
            None,
            p,
        ));
    } else {
        body = body.child(subtle(crate::i18n::t(lang, "addARelayBoardToBind"), p));
    }
    let selected = w
        .duts
        .file
        .relay_boards
        .get(w.relay_board_selected)
        .map(|board| board.id.clone());
    body.child(
        div()
            .flex()
            .gap(px(8.))
            .child(
                btn(
                    "relay-board-add",
                    crate::i18n::t(lang, "addRelayBoard"),
                    &BtnVariant::Normal,
                    p,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let boards = &mut this.duts.file.relay_boards;
                    let index = boards.len();
                    boards.push(duthub_core::model::RelayBoard {
                        id: format!("relay-{}", uuid::Uuid::new_v4()),
                        name: format!("{} {}", crate::i18n::t(lang, "controlDevice"), index + 1),
                        host: "local".into(),
                        node: String::new(),
                        driver: "CH340".into(),
                        channel_count: 8,
                    });
                    this.relay_board_selected = index;
                    this.duts.dirty = true;
                    cx.notify();
                })),
            )
            .when_some(selected, |row, id| {
                row.child(
                    btn(
                        "relay-board-save",
                        crate::i18n::t(lang, "saveRelayBoard"),
                        &BtnVariant::Primary,
                        p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_duts();
                        cx.notify();
                    })),
                )
                .child(
                    btn(
                        "control-device-delete",
                        crate::i18n::label("delete"),
                        &BtnVariant::Normal,
                        p,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Err(err) = this.remove_control_device(&id) {
                            this.notify_toast(err);
                        }
                        cx.notify();
                    })),
                )
            }),
    )
}

fn relay_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let board_ids: Vec<_> = w
        .duts
        .file
        .relay_boards
        .iter()
        .map(|board| board.id.clone())
        .collect();
    let mut options = vec![(crate::i18n::label("noControlDevice").to_string(), 0)];
    options.extend(
        w.duts
            .file
            .relay_boards
            .iter()
            .enumerate()
            .map(|(i, board)| (format!("{} · {}", board.name, board.node), i + 1)),
    );
    let selected = match &dut.relay {
        RelayConfig::Disabled => Some(0),
        RelayConfig::UsbBoard { board_id, .. } => board_ids
            .iter()
            .position(|id| id == board_id)
            .map(|i| i + 1),
        RelayConfig::SnmpPdu { .. } | RelayConfig::UsbSerial { .. } => None,
    };
    let value = selected
        .map(|i| options[i].0.clone())
        .unwrap_or_else(|| crate::i18n::label("controlDeviceUnavailable").into());
    let available_channels = if let RelayConfig::UsbBoard { board_id, .. } = &dut.relay {
        w.duts
            .file
            .relay_boards
            .iter()
            .find(|board| board.id == *board_id)
            .map_or(0, |board| board.channel_count)
    } else {
        0
    };
    let mut body = div().child(field(
        crate::i18n::label("controlDevice"),
        w.select_field(
            "cfg-relay-type",
            value,
            options,
            selected,
            std::sync::Arc::new(move |this, value, _, cx| {
                if Some(value) == selected {
                    return;
                }
                let relay = match value {
                    0 => RelayConfig::Disabled,
                    _ => {
                        let Some(board_id) = board_ids.get(value - 1) else {
                            return;
                        };
                        RelayConfig::UsbBoard {
                            board_id: board_id.clone(),
                            channels: RelayChannels::default(),
                        }
                    }
                };
                if let Some(dut) = this.duts.selected_mut() {
                    dut.relay = relay;
                    this.duts.mark_connection_dirty();
                }
                cx.notify();
            }),
            p,
            cx,
        ),
        None,
        p,
    ));
    if let RelayConfig::UsbBoard { channels, .. } = &dut.relay
        && available_channels > 0
    {
        for (key, label, channel) in [
            (
                "relay-power",
                crate::i18n::label("powerSupply"),
                channels.power,
            ),
            ("relay-reset", crate::i18n::label("reset"), channels.reset),
            (
                "relay-power-key",
                crate::i18n::label("powerKey"),
                channels.power_key,
            ),
            ("relay-maskrom", "Maskrom", channels.maskrom),
        ] {
            let options = (1..=usize::from(available_channels))
                .map(|number| (format!("CH {number}"), number))
                .collect();
            let current = channel.map_or_else(
                || crate::i18n::label("unbound").into(),
                |number| format!("CH {number}"),
            );
            let channel_select = w.select_field(
                key,
                current,
                options,
                channel.map(usize::from),
                std::sync::Arc::new(move |this, value, _, cx| {
                    set_relay_channel(this, key, Some(value as u8));
                    cx.notify();
                }),
                p,
                cx,
            );
            let mut row = div()
                .flex()
                .items_center()
                .gap(px(10.))
                .py(px(5.))
                .border_t_1()
                .border_color(p.line)
                .child(
                    div()
                        .w(px(86.))
                        .flex_none()
                        .text_sm()
                        .text_color(p.text)
                        .child(
                            Checkbox::new(format!("{key}-enabled"))
                                .label(label)
                                .checked(channel.is_some())
                                .on_change(cx.listener(move |this, checked, _, cx| {
                                    if *checked {
                                        this.open_dd = Some(key.into());
                                    } else {
                                        set_relay_channel(this, key, None);
                                        this.open_dd = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .w(px(120.))
                        .flex_shrink_1()
                        .min_w(px(76.))
                        .child(channel_select),
                );
            if let Some(channel) = channel
                .filter(|channel| *channel > 0 && *channel <= available_channels)
                .filter(|_| !w.duts.editing_new_connection)
            {
                let target = dut.id.clone();
                let held = w
                    .relay_press
                    .as_ref()
                    .is_some_and(|press| press.binding == key && press.is_held());
                let ready = w.relay_press.is_none();
                let label = crate::i18n::label("testAction");
                let press = chip(format!("{key}-hold"), label, p)
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(32.))
                    .min_w(px(93.))
                    .flex_none()
                    .when(held, |element| {
                        element.bg(p.accent_weak).text_color(p.accent)
                    })
                    .when(ready, |element| {
                        element.cursor_pointer().on_mouse_down(
                            gpui_kit::MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.press_relay(&target, key, channel, window, cx)
                            }),
                        )
                    })
                    .on_mouse_up(
                        gpui_kit::MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.release_relay(cx);
                        }),
                    )
                    .on_mouse_up_out(
                        gpui_kit::MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.release_relay(cx);
                        }),
                    );
                row = row.child(press);
            }
            body = body.child(row);
        }
    }
    panel_shell("bolt", crate::i18n::t(w.lang, "relay"), body, p)
}

fn set_relay_channel(w: &mut Workbench, key: &str, channel: Option<u8>) {
    if let Some(dut) = w.duts.selected_mut()
        && let RelayConfig::UsbBoard { channels, .. } = &mut dut.relay
    {
        let slot = match key {
            "relay-power" => &mut channels.power,
            "relay-reset" => &mut channels.reset,
            "relay-power-key" => &mut channels.power_key,
            "relay-maskrom" => &mut channels.maskrom,
            _ => unreachable!("unknown relay binding"),
        };
        *slot = channel;
        w.duts.mark_connection_dirty();
    }
}

fn mcp_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let (is_stdio, url, args, env) = match &dut.mcp {
        McpConfig::Stdio { args, env, .. } => (
            true,
            String::new(),
            args.join("\n"),
            env.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        McpConfig::StreamableHttp { url } => (false, url.clone(), String::new(), String::new()),
    };
    let mut body = div();
    body = body.child(field(
        crate::i18n::t(w.lang, "mcpTransport"),
        w.select_field(
            "cfg-mcp-transport",
            if is_stdio {
                "stdio".into()
            } else {
                "http".into()
            },
            vec![("http".to_string(), 0), ("stdio".to_string(), 1)],
            Some(if is_stdio { 1 } else { 0 }),
            std::sync::Arc::new(
                move |this: &mut Workbench,
                      value: usize,
                      _win: &mut Window,
                      cx: &mut Context<Workbench>| {
                    if let Some(dut) = this.duts.selected_mut() {
                        dut.mcp = if value == 1 {
                            McpConfig::Stdio {
                                command: String::new(),
                                args: vec![],
                                cwd: String::new(),
                                env: Default::default(),
                            }
                        } else {
                            McpConfig::StreamableHttp {
                                url: "http://127.0.0.1:8787/mcp".into(),
                            }
                        };
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
    if is_stdio {
        body = body.child(field(
            crate::i18n::t(w.lang, "mcpCommand"),
            w.path_input(
                In::McpCommand,
                crate::i18n::label("executableExample"),
                p,
                window,
                cx,
            ),
            None,
            p,
        ));
        body = body.child(field(
            crate::i18n::t(w.lang, "mcpArgs"),
            w.multiline_field(In::McpArgs, &args, "server.py\n--stdio", p, window, cx),
            None,
            p,
        ));
        body = body.child(field(
            crate::i18n::t(w.lang, "mcpCwd"),
            w.path_input(In::McpCwd, "/path/to/server", p, window, cx),
            None,
            p,
        ));
        body = body.child(field(
            crate::i18n::t(w.lang, "mcpEnv"),
            w.multiline_field(In::McpEnv, &env, "LOG_LEVEL=info", p, window, cx),
            Some(crate::i18n::t(w.lang, "mcpStdioHint")),
            p,
        ));
    } else {
        body = body.child(field(
            crate::i18n::t(w.lang, "mcpUrl"),
            w.text_field(
                In::McpUrl,
                &url,
                "http://127.0.0.1:8787/mcp",
                true,
                p,
                window,
                cx,
            ),
            Some(crate::i18n::t(w.lang, "mcpHttpHint")),
            p,
        ));
    }
    panel_shell("link", crate::i18n::t(w.lang, "mcpEndpoint"), body, p)
}

fn flash_panel(w: &mut Workbench, p: &'static Palette, cx: &mut Context<Workbench>) -> Div {
    let Some(dut) = w.duts.selected() else {
        return div();
    };
    let id = dut.id.clone();
    let choices = crate::data::flash_preset_labels(&w.settings.flash_tools);
    let selected = crate::data::flash_preset_index(&dut.flash, &w.settings.flash_tools)
        .map_or(0, |index| index + 1);
    let label = choices[selected].0.clone();
    let options = choices
        .iter()
        .enumerate()
        .map(|(ix, (label, _))| (label.clone(), ix))
        .collect();
    let select = w.select_field(
        "dut-flash-preset",
        label,
        options,
        Some(selected),
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
    panel_shell("hard-drive", crate::i18n::label("flashTitle"), body, p)
}

fn actions_row(
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
        "saveBinding"
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

fn check_box(checked: bool, p: &'static Palette) -> impl IntoElement {
    div()
        .size(px(16.))
        .rounded(px(4.))
        .border_1()
        .border_color(if checked { p.accent } else { p.border })
        .bg(if checked { p.accent } else { p.bg })
        .flex()
        .items_center()
        .justify_center()
        .text_color(p.accent_contrast)
        .text_size(px(11.))
        .when(checked, |el| el.child("✓"))
}

// ---------------------------------------------------------------------------
// 设置
// ---------------------------------------------------------------------------

pub fn render_settings(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let mut page = div()
        .flex()
        .flex_col()
        .min_h_0()
        .max_w(px(980.))
        .mx_auto()
        .w_full()
        .px(px(24.))
        .pb(px(22.));
    page = page.child(
        div()
            .pt(px(20.))
            .pb(px(8.))
            .child(
                div()
                    .text_size(px(21.))
                    .font_weight(gpui_kit::FontWeight(650.0))
                    .text_color(p.text)
                    .child(SharedString::from(
                        crate::i18n::t(w.lang, "settings").to_string(),
                    )),
            )
            .child(
                div()
                    .mt(px(2.))
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .child(SharedString::from(
                        crate::i18n::t(w.lang, "settingsSub").to_string(),
                    )),
            ),
    );
    let mut grid = div().flex().flex_col();
    grid = grid.child(appearance_panel(w, p, window, cx));
    grid = grid.child(terminal_font_panel(w, p, window, cx));
    grid = grid.child(setting_row(
        crate::i18n::label("saveSessionLogs"),
        crate::i18n::label("saveSessionLogsHint"),
        toggle(
            "opt-save-session-logs",
            w.settings.terminal.save_logs.unwrap_or(true),
            p,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            let enabled = !this.settings.terminal.save_logs.unwrap_or(true);
            this.settings.terminal.save_logs = (!enabled).then_some(false);
            this.device.set_log_saving(enabled);
            this.persist_settings();
            cx.notify();
        })),
        p,
    ));
    grid = grid.child(workspace_dir_panel(w, p, window, cx));
    grid = grid.child(regex_panel(w, p, window, cx));
    grid = grid.child(a2a_panel(w, p, window, cx));
    grid = grid.child(agent_access_panel(w, p, window, cx));
    grid = grid.child(commands_panel(w, p, cx));
    grid = grid.child(config_files_panel(w, p, cx));
    page.child(grid)
}

/// DUT-specific notification settings, shared with the alert dispatcher.
fn alerts_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    use crate::alerts::AlertSource;
    use crate::i18n::label;
    let Some(dut) = w.duts.selected() else {
        return div();
    };
    let id = dut.id.clone();
    let settings = w.dut_alert_settings(&id);
    let enabled = settings.enabled;
    let suppressed: Vec<AlertSource> = [
        AlertSource::Heartbeat,
        AlertSource::Flash,
        AlertSource::Serial,
        AlertSource::Control,
        AlertSource::Agent,
    ]
    .into_iter()
    .filter(|s| settings.suppressed_sources.iter().any(|key| key == s.key()))
    .collect();
    let has_suppressed = !suppressed.is_empty();

    let mut body = div();
    let toggle_id = id.clone();
    body = body.child(setting_row(
        label("alertsEnabled"),
        label("alertsHint"),
        toggle("opt-alerts-enabled", enabled, p).on_click(cx.listener(
            move |this, _ev, _win, cx| {
                this.edit_dut_alerts(&toggle_id, |cfg| cfg.enabled = !cfg.enabled);
                cx.notify();
            },
        )),
        p,
    ));
    // 抑制列表：来源标签行 + 重置按钮
    let suppressed_row = if has_suppressed {
        let mut chips = div().flex().flex_wrap().gap(px(6.));
        for source in &suppressed {
            chips = chips.child(
                div()
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(6.))
                    .bg(p.raised)
                    .border_1()
                    .border_color(p.border)
                    .text_size(px(10.5))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.muted)
                    .child(SharedString::from(source.label().to_string())),
            );
        }
        chips.child(
            btn(
                "opt-alerts-reset",
                label("restoreAlerts"),
                &BtnVariant::Normal,
                p,
            )
            .on_click(cx.listener(move |this, _ev, _win, cx| {
                this.edit_dut_alerts(&id, |cfg| cfg.suppressed_sources.clear());
                cx.notify();
            })),
        )
    } else {
        subtle(label("noMutedAlerts"), p)
    };
    body = body.child(setting_row(
        label("mutedAlerts"),
        label("mutedAlertsHint"),
        suppressed_row,
        p,
    ));
    panel_shell("alert", label("alertsTitle"), body, p)
}

fn settings_panel(
    title_icon: &str,
    title: &str,
    right: Option<&str>,
    body: Div,
    p: &'static Palette,
) -> gpui_kit::Div {
    let mut head = panel_head(title_icon, title, p);
    if let Some(right) = right {
        head = head.child(subtle(right, p));
    }
    div()
        .border_b_1()
        .border_color(p.line)
        .child(head)
        .child(div().pt(px(7.)).pb(px(20.)).child(body))
}

fn setting_row(title: &str, desc: &str, control: impl IntoElement, p: &'static Palette) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(18.))
        .py(px(14.))
        .border_b_1()
        .border_color(p.line)
        .child(
            div()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(gpui_kit::FontWeight(600.0))
                        .text_color(p.text)
                        .child(SharedString::from(title.to_string())),
                )
                .child(
                    div()
                        .mt(px(3.))
                        .text_size(px(11.))
                        .text_color(p.muted)
                        .child(SharedString::from(desc.to_string())),
                ),
        )
        .child(control)
}

/// 设置页「工作目录」面板。
fn workspace_dir_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let effective = w.artifact_root.display().to_string();
    let limit = w.input_value(In::DutStorageGib);
    let body = div()
        .child(field(
            crate::i18n::label("activeWorkDir"),
            div()
                .text_size(px(11.))
                .font_family(crate::theme::MONO_FONT)
                .text_color(p.muted)
                .child(SharedString::from(effective)),
            None,
            p,
        ))
        .child(field(
            crate::i18n::label("workDir"),
            w.path_input(
                In::WorkDir,
                crate::i18n::label("defaultWorkDirHint"),
                p,
                window,
                cx,
            ),
            None,
            p,
        ))
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .child(
                    btn(
                        "save-workdir",
                        crate::i18n::label("saveWorkDir"),
                        &BtnVariant::TinyPrimary,
                        p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        let cleaned = this
                            .settings
                            .workspace_dir
                            .take()
                            .map(|path| path.trim().to_owned())
                            .filter(|path| !path.is_empty());
                        this.settings.workspace_dir = cleaned;
                        this.persist_settings();
                        cx.notify();
                    })),
                )
                .child(subtle(crate::i18n::label("workDirPersistenceHint"), p)),
        )
        .child(field(
            crate::i18n::label("dutStorageLimit"),
            div()
                .flex()
                .gap(px(8.))
                .items_center()
                .child(div().w(px(90.)).child(w.text_field(
                    In::DutStorageGib,
                    &limit,
                    "10",
                    true,
                    p,
                    window,
                    cx,
                )))
                .child(
                    btn(
                        "save-dut-storage-limit",
                        crate::i18n::label("save"),
                        &BtnVariant::Tiny,
                        p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_dut_storage_limit();
                        cx.notify();
                    })),
                ),
            Some(crate::i18n::label("dutStorageRetention")),
            p,
        ))
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .mt(px(6.))
                .child(
                    btn(
                        "export-skill",
                        crate::i18n::label("exportSkill"),
                        &BtnVariant::Tiny,
                        p,
                    )
                    .flex_none()
                    .on_click(cx.listener(|this, _ev, _win, cx| {
                        this.export_skill_md();
                        cx.notify();
                    })),
                )
                .child(subtle(crate::i18n::label("exportSkillHint"), p)),
        );
    settings_panel(
        "folder",
        crate::i18n::label("workDir"),
        Some("settings.json"),
        body,
        p,
    )
}

/// 设置页「Agent 接入」面板（访问控制 M1 GUI，方案 §6 第 4 项；按需求只中文）：
/// auth 总开关与监听配置 → 凭据列表（Token 原文可见 + 复制 + 启停 + 撤销）→
/// 每凭据 DUT 授权编辑（读/写串口/ADB/继电器/电源/复位/烧录）→
/// DUT 控制权面板（状态/持有者/剩余 TTL/队列 + 释放/强制接管/安全恢复）→
/// 审计事件列表（最近 100 条）。
fn agent_access_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    // 面板内只读快照：渲染借用结束前收集全部数据，listener 闭包只捕获 clone。
    let access = w.settings.agent_access.clone();
    let auth_on = w.access_runtime.enabled();
    let port_value = w.input_value(In::AgentAccessPort);
    let ttl_value = w.input_value(In::AgentAccessTtl);
    let cred_name_value = w.input_value(In::AgentAccessCredName);
    let port_field = w.text_field(
        In::AgentAccessPort,
        &port_value,
        "8797",
        true,
        p,
        window,
        cx,
    );
    let ttl_field = w.text_field(In::AgentAccessTtl, &ttl_value, "120", true, p, window, cx);
    let extra_port = w.input_value(In::AgentAccessExtraPort);
    let extra_port_field = w.text_field(
        In::AgentAccessExtraPort,
        &extra_port,
        "8788",
        true,
        p,
        window,
        cx,
    );
    let endpoints: Vec<_> = std::iter::once((None, access.http_port))
        .chain(
            access
                .listeners
                .iter()
                .map(|listener| (Some(listener.id.clone()), listener.port)),
        )
        .collect();
    let selected = endpoints
        .iter()
        .position(|(id, _)| *id == w.agent_credential_listener)
        .unwrap_or(0);
    let listener_field = w.select_field(
        "aa-credential-listener",
        format!("127.0.0.1:{}", endpoints[selected].1),
        endpoints
            .iter()
            .enumerate()
            .map(|(index, (_, port))| (format!("127.0.0.1:{port}"), index))
            .collect(),
        Some(selected),
        std::sync::Arc::new(move |this, index, _, cx| {
            if let Some((id, _)) = endpoints.get(index) {
                this.agent_credential_listener = id.clone();
                cx.notify();
            }
        }),
        p,
        cx,
    );
    let name_field = w.text_field(
        In::AgentAccessCredName,
        &cred_name_value,
        "Agent 名称（仅展示，如 claude-code）",
        false,
        p,
        window,
        cx,
    );
    let duts_list: Vec<(String, String)> = w
        .duts
        .file
        .duts
        .iter()
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect();
    let lease = w.device.lease_manager();
    let audit = w.device.audit_sink();
    let audit_events = audit
        .as_ref()
        .map(|sink| (sink.recent(100), crate::alerts::now_ms()));

    let mut body = div().flex().flex_col();

    // —— 风险提示（Token 明文是用户明确要求）——
    body = body.child(
        subtle(
            "⚠ Token 明文保存、界面始终可见：泄露等同交出对应 DUT 控制权，请勿截图外传；发现泄露立即撤销该凭据。",
            p,
        )
        .text_color(p.warn),
    );

    // ① 认证总开关（离散变更：即时同步运行时 + 落盘）
    body = body.child(setting_row(
        "认证总开关（auth）",
        crate::i18n::label(if access.listeners.is_empty() {
            "mcpAuthOptional"
        } else {
            "mcpAuthRequired"
        }),
        toggle("aa-auth-enabled", auth_on, p).on_click(cx.listener(|this, _ev, _win, cx| {
            this.toggle_agent_auth();
            cx.notify();
        })),
        p,
    ));

    // 监听配置：端口 / TTL 文本输入（「保存」统一提交；端口重启生效）
    body = body.child(
        aa_section("监听与租约", p)
            .child(field(
                "MCP HTTP 监听端口",
                port_field,
                Some("固定绑定 127.0.0.1（loopback）；修改后重启生效"),
                p,
            ))
            .child(field(
                "租约 TTL（秒）",
                ttl_field,
                Some("dut_acquire_control 独占控制权的自动过期时间（续租可延长）"),
                p,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .py(px(6.))
                    .child(
                        btn("aa-save", "保存接入配置", &BtnVariant::TinyPrimary, p)
                            .flex_none()
                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                this.save_agent_access();
                                cx.notify();
                            })),
                    )
                    .child(subtle("TTL 经「保存」即时生效；端口重启生效", p)),
            )
            .child(field(
                crate::i18n::label("mcpExtraPort"),
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().child(extra_port_field))
                    .child(
                        btn(
                            "aa-add-listener",
                            crate::i18n::label("mcpAddListener"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.add_mcp_listener();
                            cx.notify();
                        })),
                    ),
                Some(crate::i18n::label("mcpListenerRestart")),
                p,
            ))
            .children(access.listeners.iter().map(|listener| {
                let id = listener.id.clone();
                setting_row(
                    &format!("127.0.0.1:{}", listener.port),
                    "MCP",
                    icon_btn(format!("aa-remove-listener-{id}"), "trash", p).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.remove_mcp_listener(&id);
                            cx.notify();
                        },
                    )),
                    p,
                )
            })),
    );

    // ② 凭据列表：添加表单 + 每凭据行（Token 原文/复制/启停/撤销）
    body = body.child(
        aa_section("凭据", p)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .py(px(6.))
                    .child(div().w(px(240.)).flex_none().child(name_field))
                    .child(div().min_w(px(140.)).child(listener_field))
                    .child(
                        btn("aa-add-cred", "生成并添加", &BtnVariant::TinyPrimary, p)
                            .flex_none()
                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                this.add_agent_credential();
                                cx.notify();
                            })),
                    )
                    .child(subtle(
                        "名称仅作展示，不参与认证；Token 由服务端随机生成",
                        p,
                    )),
            )
            .children(access.agents.iter().map(|cred| {
                let port = match cred.listener_id.as_deref() {
                    None => Some(access.http_port),
                    Some(id) => access
                        .listeners
                        .iter()
                        .find(|listener| listener.id == id)
                        .map(|listener| listener.port),
                };
                aa_credential_card(cred, port, &duts_list, p, cx)
            })),
    );

    // ③ DUT 控制权面板：每 DUT 状态行 + 释放/强制接管/安全恢复
    let mut control_block = aa_section("DUT 控制权", p);
    match &lease {
        Some(lease) => {
            control_block =
                control_block.child(aa_lease_header(p).border_b_1().border_color(p.line));
            for (dut_id, dut_name) in &duts_list {
                let status = lease.status(dut_id);
                control_block = control_block.child(aa_lease_row(dut_id, dut_name, &status, p, cx));
            }
            if duts_list.is_empty() {
                control_block = control_block.child(subtle("暂无已保存 DUT", p));
            }
        }
        None => {
            control_block = control_block.child(subtle("租约管理器不可用（demo 构建无租约层）", p));
        }
    }
    body = body.child(control_block);

    // ④ 审计事件列表：最近 100 条（脱敏不含 token）
    let mut audit_block = aa_section("审计事件（最近 100 条）", p);
    match &audit_events {
        Some((events, now)) => {
            if events.is_empty() {
                audit_block = audit_block.child(subtle("暂无审计事件", p));
            } else {
                let mut list = div().flex().flex_col();
                for event in events.iter().rev() {
                    list = list.child(aa_audit_row(event, *now, p));
                }
                audit_block = audit_block.child(list);
            }
        }
        None => {
            audit_block = audit_block.child(subtle("审计落盘不可用（demo 构建）", p));
        }
    }
    body = body.child(audit_block);

    settings_panel(
        "shield",
        "Agent 接入",
        Some("settings.json · agentAccess"),
        body,
        p,
    )
}

/// 「Agent 接入」面板内的小节标题。
fn aa_section(title: &str, p: &'static Palette) -> Div {
    div().pt(px(14.)).pb(px(4.)).child(
        div()
            .text_size(px(12.))
            .font_weight(gpui_kit::FontWeight(600.0))
            .text_color(p.text)
            .child(SharedString::from(title.to_string())),
    )
}

/// 授权矩阵的可点勾选块（与 check_box 同视觉，加动态 id + on_click）。
fn grant_check(id: impl Into<SharedString>, checked: bool, p: &'static Palette) -> Stateful<Div> {
    div()
        .id(id.into())
        .size(px(16.))
        .rounded(px(4.))
        .border_1()
        .border_color(if checked { p.accent } else { p.border })
        .bg(if checked { p.accent } else { p.bg })
        .flex()
        .items_center()
        .justify_center()
        .text_color(p.accent_contrast)
        .text_size(px(11.))
        .cursor_pointer()
        .hover(|s| s.border_color(p.accent))
        .when(checked, |el| el.child("✓"))
}

const GRANT_ACTIONS: [(DutAction, &str); 1] = [(DutAction::Read, "读")];

/// 单张凭据卡：名称 + 启停/复制/撤销 + Token 原文 + 每 DUT 授权矩阵。
fn aa_credential_card(
    cred: &duthub_core::AgentCredential,
    port: Option<u16>,
    duts_list: &[(String, String)],
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let cred_id = cred.id.clone();
    let token = cred.token.clone();
    let mut card = div()
        .mt(px(8.))
        .rounded(px(10.))
        .border_1()
        .border_color(p.line)
        .bg(p.surface)
        .px(px(12.))
        .py(px(10.))
        .flex()
        .flex_col()
        .gap(px(6.));

    // 行 1：名称 + 启停 chip + 弹性空隙 + 复制 / 撤销
    let toggle_id = cred_id.clone();
    let copy_id = cred_id.clone();
    let revoke_id = cred_id.clone();
    let copy_token = token.clone();
    card = card.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(gpui_kit::FontWeight(600.0))
                    .text_color(p.text)
                    .child(SharedString::from(cred.agent_name.clone())),
            )
            .child(subtle(
                &port.map_or_else(
                    || crate::i18n::label("mcpListenerUnavailable").to_owned(),
                    |port| format!("127.0.0.1:{port}"),
                ),
                p,
            ))
            .child(
                chip(
                    format!("aa-cred-toggle-{cred_id}"),
                    if cred.enabled {
                        "已启用"
                    } else {
                        "已停用"
                    },
                    p,
                )
                .when(cred.enabled, |el| el.text_color(p.accent))
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.toggle_agent_credential(&toggle_id);
                    cx.notify();
                })),
            )
            .child(div().flex_1())
            .child(
                chip(format!("aa-cred-copy-{copy_id}"), "复制 Token", p).on_click(cx.listener(
                    move |this, _ev, _win, cx| {
                        copy_text(cx, &copy_token);
                        this.notify_toast("Token 已复制到剪贴板");
                        cx.notify();
                    },
                )),
            )
            .child(
                chip(format!("aa-cred-revoke-{revoke_id}"), "撤销", p)
                    .text_color(p.red)
                    .on_click(cx.listener(move |this, _ev, _win, cx| {
                        this.revoke_agent_credential(&revoke_id);
                        cx.notify();
                    })),
            ),
    );

    // 行 2：Token 原文（等宽；明文可见为用户明确要求）
    card = card.child(
        div()
            .text_size(px(11.))
            .font_family(crate::theme::MONO_FONT)
            .text_color(p.muted)
            .child(SharedString::from(token)),
    );

    // 行 3：每 DUT 授权矩阵（deny by default：未勾选即拒绝）
    if duts_list.is_empty() {
        card = card.child(subtle("暂无已保存 DUT——该凭据未授权任何 DUT", p));
    } else {
        let mut matrix = div().flex().flex_col().mt(px(2.));
        let mut header = div().flex().items_center().gap(px(4.)).py(px(2.));
        header = header.child(
            div()
                .w(px(110.))
                .flex_none()
                .text_size(px(10.5))
                .text_color(p.faint)
                .child("DUT / 动作"),
        );
        for (_, label) in GRANT_ACTIONS {
            header = header.child(
                div()
                    .w(px(56.))
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(p.faint)
                    .child(SharedString::from(label.to_string())),
            );
        }
        matrix = matrix.child(header);
        for (dut_id, dut_name) in duts_list {
            let mut row = div()
                .flex()
                .items_center()
                .gap(px(4.))
                .py(px(3.))
                .border_t_1()
                .border_color(p.line);
            row = row.child(
                div()
                    .w(px(110.))
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(p.text)
                    .child(SharedString::from(dut_name.clone())),
            );
            let granted = |action: DutAction| {
                cred.grants
                    .iter()
                    .find(|g| g.dut_id == *dut_id)
                    .is_some_and(|g| g.actions.contains(&action))
            };
            for (action, _) in GRANT_ACTIONS {
                let lid = cred_id.clone();
                let ldut = dut_id.clone();
                row = row.child(
                    div().w(px(56.)).flex_none().flex().justify_center().child(
                        grant_check(
                            format!("aa-g-{}-{}-{}", cred_id, dut_id, action.as_str()),
                            granted(action),
                            p,
                        )
                        .on_click(cx.listener(
                            move |this, _ev, _win, cx| {
                                this.toggle_agent_grant(&lid, &ldut, action);
                                cx.notify();
                            },
                        )),
                    ),
                );
            }
            matrix = matrix.child(row);
        }
        card = card.child(matrix);
    }
    card
}

/// 控制权面板表头。
fn aa_lease_header(p: &'static Palette) -> Div {
    let mut head = div().flex().items_center().gap(px(8.)).py(px(4.));
    for (width, label) in [
        (px(110.), "DUT"),
        (px(64.), "状态"),
        (px(150.), "持有者"),
        (px(56.), "剩余"),
        (px(56.), "队列"),
    ] {
        head = head.child(
            div()
                .w(width)
                .flex_none()
                .text_size(px(10.5))
                .text_color(p.faint)
                .child(SharedString::from(label.to_string())),
        );
    }
    head.child(
        div()
            .w(px(170.))
            .flex_none()
            .text_size(px(10.5))
            .text_color(p.faint)
            .child("操作"),
    )
}

/// 控制权面板单 DUT 行：状态/持有者/剩余 TTL/队列 + 操作按钮。
fn aa_lease_row(
    dut_id: &str,
    dut_name: &str,
    status: &duthub_core::LeaseStatus,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let (phase_text, phase_color) = match status.phase {
        LeasePhase::Free => ("空闲", p.muted),
        LeasePhase::Held => ("占用", p.accent),
        LeasePhase::Protected => ("保护中", p.warn),
        LeasePhase::NeedsInspection => ("待检查", p.red),
    };
    let holder = status.holder.clone().unwrap_or_else(|| "-".to_string());
    let holder_is_gui = holder.starts_with("GUI");
    let remaining = status
        .remaining_secs
        .map(|s| format!("{s}s"))
        .unwrap_or_else(|| "-".to_string());

    let mut row = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .py(px(5.))
        .border_t_1()
        .border_color(p.line);
    row = row.child(
        div()
            .w(px(110.))
            .flex_none()
            .text_size(px(11.5))
            .text_color(p.text)
            .child(SharedString::from(dut_name.to_string())),
    );
    row = row.child(
        div()
            .w(px(64.))
            .flex_none()
            .text_size(px(11.))
            .font_family(crate::theme::MONO_FONT)
            .text_color(phase_color)
            .child(SharedString::from(
                match (&status.phase, &status.protected_op) {
                    (LeasePhase::Protected, Some(op)) => format!("保护中·{op}"),
                    _ => phase_text.to_string(),
                },
            )),
    );
    row = row.child(
        div()
            .w(px(150.))
            .flex_none()
            .text_size(px(11.))
            .font_family(crate::theme::MONO_FONT)
            .text_color(p.muted)
            .child(SharedString::from(holder)),
    );
    row = row.child(
        div()
            .w(px(56.))
            .flex_none()
            .text_size(px(11.))
            .font_family(crate::theme::MONO_FONT)
            .text_color(p.muted)
            .child(SharedString::from(remaining)),
    );
    row = row.child(
        div()
            .w(px(56.))
            .flex_none()
            .text_size(px(11.))
            .font_family(crate::theme::MONO_FONT)
            .text_color(p.muted)
            .child(SharedString::from(status.queue_len.to_string())),
    );

    let mut actions = div()
        .w(px(170.))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(6.));
    match status.phase {
        LeasePhase::Protected => {
            actions = actions.child(subtle("不可交接", p));
        }
        LeasePhase::NeedsInspection => {
            let id = dut_id.to_string();
            actions = actions.child(chip(format!("aa-inspect-{id}"), "安全恢复", p).on_click(
                cx.listener(move |this, _ev, _win, cx| {
                    this.gui_resolve_inspection(&id);
                    cx.notify();
                }),
            ));
        }
        LeasePhase::Free => {
            actions = actions.child(subtle("-", p));
        }
        LeasePhase::Held => {
            if holder_is_gui {
                let id = dut_id.to_string();
                actions = actions.child(chip(format!("aa-release-{id}"), "释放", p).on_click(
                    cx.listener(move |this, _ev, _win, cx| {
                        this.gui_release_lease(&id);
                        cx.notify();
                    }),
                ));
            } else {
                let id = dut_id.to_string();
                actions = actions.child(chip(format!("aa-takeover-{id}"), "强制接管", p).on_click(
                    cx.listener(move |this, _ev, _win, cx| {
                        this.gui_force_takeover(&id);
                        cx.notify();
                    }),
                ));
            }
        }
    }
    row.child(actions)
}

/// 审计事件单行（脱敏文本；不记 token——audit.rs 写入前已 redact）。
fn aa_audit_row(event: &duthub_core::AuditEvent, now_ms: u64, p: &'static Palette) -> Div {
    let (result_text, result_color) = match event.result {
        duthub_core::AuditOutcome::Ok => ("ok", p.muted),
        duthub_core::AuditOutcome::Denied => ("denied", p.warn),
        duthub_core::AuditOutcome::Error => ("error", p.red),
    };
    let agent = event.agent.clone().unwrap_or_else(|| "GUI".to_string());
    let mut row = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .py(px(3.))
        .border_t_1()
        .border_color(p.line);
    for (width, text, color) in [
        (
            px(70.),
            crate::alerts::format_alert_time(event.ts_unix_ms, now_ms),
            p.faint,
        ),
        (px(96.), agent, p.text),
        (px(170.), event.action.clone(), p.muted),
        (
            px(90.),
            event.dut.clone().unwrap_or_else(|| "-".into()),
            p.muted,
        ),
        (px(52.), result_text.to_string(), result_color),
    ] {
        row = row.child(
            div()
                .w(width)
                .flex_none()
                .text_size(px(10.5))
                .font_family(crate::theme::MONO_FONT)
                .text_color(color)
                .child(SharedString::from(text)),
        );
    }
    row.child(
        div()
            .flex_1()
            .min_w(px(0.))
            .text_size(px(10.5))
            .font_family(crate::theme::MONO_FONT)
            .text_color(p.faint)
            .child(SharedString::from(event.detail.clone().unwrap_or_default())),
    )
}

fn appearance_panel(
    w: &mut Workbench,
    p: &'static Palette,
    _window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let locales = crate::i18n::language_options(w.lang);
    let selected = locales
        .iter()
        .position(|(code, _)| *code == w.settings.appearance.language)
        .unwrap_or_default();
    let locale_options = locales
        .iter()
        .enumerate()
        .map(|(index, (_, name))| (name.to_string(), index))
        .collect();
    let body = div()
        .child(setting_row(
            crate::i18n::t(w.lang, "language"),
            crate::i18n::t(w.lang, "languageHint"),
            w.select_field(
                "opt-language",
                locales[selected].1.to_string(),
                locale_options,
                Some(selected),
                std::sync::Arc::new(
                    move |this: &mut Workbench,
                          value: usize,
                          _win: &mut Window,
                          cx: &mut Context<Workbench>| {
                        let Some(&(code, _)) = locales.get(value) else {
                            return;
                        };
                        this.settings.appearance.language = code.to_string();
                        this.lang = crate::i18n::Lang::from_setting(code);
                        this.lang.activate();
                        this.persist_settings();
                        cx.notify();
                    },
                ),
                p,
                cx,
            ),
            p,
        ))
        .child(setting_row(
            crate::i18n::t(w.lang, "theme"),
            crate::i18n::t(w.lang, "themeHint"),
            w.select_field(
                "opt-theme",
                match w.theme_mode {
                    crate::theme::ThemeMode::Dark => crate::i18n::label("themeDark").into(),
                    crate::theme::ThemeMode::Light => crate::i18n::label("themeLight").into(),
                },
                vec![
                    (crate::i18n::label("themeDark").to_string(), 0),
                    (crate::i18n::label("themeLight").to_string(), 1),
                ],
                Some(match w.theme_mode {
                    crate::theme::ThemeMode::Dark => 0,
                    crate::theme::ThemeMode::Light => 1,
                }),
                std::sync::Arc::new(
                    move |this: &mut Workbench,
                          value: usize,
                          _win: &mut Window,
                          cx: &mut Context<Workbench>| {
                        let code = if value == 1 { "light" } else { "dark" };
                        this.settings.appearance.theme = code.to_string();
                        this.theme_mode = crate::theme::ThemeMode::from_setting(code);
                        crate::theme::apply_brand(cx, this.theme_mode);
                        this.persist_settings();
                        cx.notify();
                    },
                ),
                p,
                cx,
            ),
            p,
        ))
        .child(setting_row(
            crate::i18n::t(w.lang, "jumpLatest"),
            crate::i18n::t(w.lang, "jumpHint"),
            toggle("opt-jump", w.settings.terminal.jump_to_latest, p).on_click(cx.listener(
                |this, _ev, _win, cx| {
                    this.settings.terminal.jump_to_latest = !this.settings.terminal.jump_to_latest;
                    this.persist_settings();
                    cx.notify();
                },
            )),
            p,
        ))
        .child(setting_row(
            crate::i18n::t(w.lang, "commandBar"),
            crate::i18n::t(w.lang, "commandBarHint"),
            toggle("opt-commandbar", w.settings.terminal.show_command_bar, p).on_click(
                cx.listener(|this, _ev, _win, cx| {
                    this.settings.terminal.show_command_bar =
                        !this.settings.terminal.show_command_bar;
                    this.persist_settings();
                    cx.notify();
                }),
            ),
            p,
        ));
    settings_panel(
        "settings",
        crate::i18n::t(w.lang, "appearance"),
        None,
        body,
        p,
    )
}

fn terminal_font_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    use crate::i18n::label;
    use crate::workspace::{MAX_TERMINAL_FONT_SIZE, MIN_TERMINAL_FONT_SIZE};

    let size = w.terminal_font_size();
    let size_controls = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            btn("terminal-font-smaller", "−", &BtnVariant::Normal, p).on_click(cx.listener(
                |this, _, _, cx| {
                    let next = (this.terminal_font_size() - 1.).max(MIN_TERMINAL_FONT_SIZE);
                    this.set_terminal_font_size(Some(next));
                    cx.notify();
                },
            )),
        )
        .child(
            div()
                .min_w(px(46.))
                .text_center()
                .text_color(p.text)
                .child(format!("{} px", (size * 10.).round() / 10.)),
        )
        .child(
            btn("terminal-font-larger", "+", &BtnVariant::Normal, p).on_click(cx.listener(
                |this, _, _, cx| {
                    let next = (this.terminal_font_size() + 1.).min(MAX_TERMINAL_FONT_SIZE);
                    this.set_terminal_font_size(Some(next));
                    cx.notify();
                },
            )),
        )
        .child(
            btn(
                "terminal-font-reset",
                label("terminalFontReset"),
                &BtnVariant::Normal,
                p,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.set_terminal_font_size(None);
                cx.notify();
            })),
        );
    let (family, _, _) = w.terminal_font(window);
    let body = div()
        .child(setting_row(
            label("terminalFontFamily"),
            label("terminalFontFamilyHint"),
            w.terminal_font_picker(window, cx),
            p,
        ))
        .child(setting_row(
            label("terminalFontSize"),
            if cfg!(target_os = "macos") {
                label("terminalFontShortcutsMac")
            } else {
                label("terminalFontShortcuts")
            },
            size_controls,
            p,
        ))
        .child(
            div()
                .mt(px(10.))
                .p(px(10.))
                .rounded(px(6.))
                .bg(p.term)
                .text_color(p.text)
                .font_family(family)
                .text_size(px(size))
                .child("$ ls -l /dev/ttyUSB0"),
        );
    settings_panel("terminal", label("terminalFont"), None, body, p)
}

fn regex_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let regex = w.settings.terminal.prompt_highlight.regex.clone();
    let color = w.settings.terminal.prompt_highlight.color.clone();
    let sample = "root@rk3576:~# dmesg | tail";
    let mut body = div();
    body = body.child(field(
        crate::i18n::t(w.lang, "regexPattern"),
        w.multiline_field(
            In::Regex,
            &regex,
            r#"(^[^s@]+@[^s:]+:[^#
]*[#5.042002)"#,
            p,
            window,
            cx,
        ),
        Some(crate::i18n::label("regexHint")),
        p,
    ));
    body = body.child(field(
        crate::i18n::t(w.lang, "regexColor"),
        div()
            .flex()
            .gap(px(8.))
            .items_center()
            .child(color_swatch(&color, p))
            .children(["#edbe7a", "#71ddbf", "#88b7ed", "#ec8291"].map(|c| {
                chip(SharedString::from(format!("regex-color-{c}")), c, p).on_click(cx.listener(
                    move |this, _ev, _win, cx| {
                        this.settings.terminal.prompt_highlight.color = c.to_string();
                        this.persist_settings();
                        cx.notify();
                    },
                ))
            })),
        None,
        p,
    ));
    // 预览
    let mut preview = div()
        .mt(px(11.))
        .px(px(12.))
        .py(px(10.))
        .rounded(px(8.))
        .border_1()
        .border_color(p.border)
        .bg(p.bg)
        .text_size(px(12.))
        .font_family(crate::theme::MONO_FONT)
        .flex()
        .flex_wrap();
    match regex::Regex::new(&regex) {
        Ok(re) if !regex.trim().is_empty() => {
            let mut last = 0;
            for m in re.find_iter(sample).take(20) {
                if m.start() > last {
                    preview =
                        preview.child(SharedString::from(sample[last..m.start()].to_string()));
                }
                preview = preview.child(
                    div()
                        .text_color(hex_to_rgba(&color, p))
                        .font_weight(gpui_kit::FontWeight(650.0))
                        .child(SharedString::from(m.as_str().to_string())),
                );
                last = m.end();
            }
            preview = preview.child(SharedString::from(sample[last..].to_string()));
        }
        Ok(_) => {
            preview = preview.child(
                div()
                    .text_color(p.prompt)
                    .font_weight(gpui_kit::FontWeight(600.0))
                    .child(SharedString::from("root@rk3576:~#".to_string())),
            );
            preview = preview.child(SharedString::from(" dmesg | tail".to_string()));
        }
        Err(err) => {
            preview = preview.child(
                div()
                    .text_color(p.red)
                    .child(SharedString::from(format!("正则无效: {err}"))),
            );
        }
    }
    body = body.child(
        div()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .font_weight(gpui_kit::FontWeight(550.0))
                    .child(SharedString::from(
                        crate::i18n::t(w.lang, "regexPreview").to_string(),
                    )),
            )
            .child(preview),
    );
    body = body.child(
        div().flex().gap(px(8.)).mt(px(12.)).child(
            btn(
                "save-regex",
                crate::i18n::t(w.lang, "save").to_string(),
                &BtnVariant::Primary,
                p,
            )
            .on_click(cx.listener(|this, _ev, _win, cx| {
                this.persist_settings();
                this.notify_toast(crate::i18n::label("regexSaved"));
                cx.notify();
            })),
        ),
    );
    settings_panel(
        "terminal",
        crate::i18n::t(w.lang, "regexTitle"),
        Some("REGEX"),
        body,
        p,
    )
}

fn color_swatch(color: &str, p: &'static Palette) -> Div {
    div()
        .size(px(12.))
        .rounded_full()
        .bg(hex_to_rgba(color, p))
        .border_1()
        .border_color(p.border)
}

pub fn hex_to_rgba(hex: &str, _p: &'static Palette) -> gpui_kit::Rgba {
    let hex = hex.trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16).unwrap_or(0xedbe7a);
    let r = ((value >> 16) & 0xff) as f32 / 255.0;
    let g = ((value >> 8) & 0xff) as f32 / 255.0;
    let b = (value & 0xff) as f32 / 255.0;
    gpui_kit::Rgba { r, g, b, a: 1.0 }
}

/// 烧录工具预设页（自设置独立成分类）。
pub fn render_flash_tools(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let mut page = div()
        .flex()
        .flex_col()
        .min_h_0()
        .max_w(px(980.))
        .mx_auto()
        .w_full()
        .px(px(24.))
        .pb(px(22.));
    page = page.child(
        div().pt(px(20.)).pb(px(8.)).child(
            div()
                .text_size(px(21.))
                .font_weight(gpui_kit::FontWeight(650.0))
                .text_color(p.text)
                .child(crate::i18n::label("flashTitle")),
        ),
    );
    page.child(flash_preset_panel(w, p, cx))
}

pub fn render_usb_debug(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let adb_input = w.path_input(In::AdbExe, "adb", p, window, cx);
    let hdc_input = w.path_input(In::HdcExe, "hdc", p, window, cx);
    let tool_panel = |name, input, hint| {
        settings_panel(
            "usb",
            name,
            None,
            field(crate::i18n::label("debugExecutable"), input, Some(hint), p),
            p,
        )
    };
    div()
        .flex()
        .flex_col()
        .min_h_0()
        .max_w(px(980.))
        .mx_auto()
        .w_full()
        .px(px(24.))
        .py(px(20.))
        .gap(px(12.))
        .child(subtle(crate::i18n::label("debugToolsHint"), p))
        .child(tool_panel("ADB", adb_input, "adb devices -l"))
        .child(tool_panel("HDC", hdc_input, "hdc list targets"))
        .child(
            div().flex().justify_end().child(
                btn(
                    "save-usb-tools",
                    crate::i18n::label("saveDebugTools"),
                    &BtnVariant::TinyPrimary,
                    p,
                )
                .flex_none()
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    this.save_usb_tools();
                    cx.notify();
                })),
            ),
        )
}

fn flash_preset_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let mut body = div().child(subtle(crate::i18n::label("flashPresetHint"), p));
    for (index, preset) in w.settings.flash_tools.iter().enumerate() {
        let id = preset.id.clone();
        let test = w.flash_preset_tests.get(&id);
        let running = matches!(test, Some(FlashPresetTest::Running));
        let row = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .py(px(9.))
            .px(px(11.))
            .child(
                div()
                    .min_w(px(105.))
                    .font_weight(gpui_kit::FontWeight(600.0))
                    .text_size(px(12.))
                    .text_color(p.text)
                    .child(SharedString::from(preset.name.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.muted)
                    .overflow_hidden()
                    .child(SharedString::from(preset.executable.clone())),
            )
            .child(
                btn(
                    SharedString::from(format!("preset-test-{id}")),
                    crate::i18n::label(if running {
                        "flashProbeRunning"
                    } else {
                        "flashProbeRun"
                    }),
                    &BtnVariant::Tiny,
                    p,
                )
                .flex_shrink_0()
                .disabled(running)
                .on_click(cx.listener(move |this, _, _, cx| this.test_flash_preset(&id, cx))),
            )
            .child(
                icon_btn(
                    SharedString::from(format!("preset-del-{index}")),
                    "trash",
                    p,
                )
                .disabled(running)
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    let removed = this.settings.flash_tools.remove(index);
                    this.flash_preset_tests.remove(&removed.id);
                    this.persist_settings();
                    cx.notify();
                })),
            );
        let mut card = div()
            .mt(px(8.))
            .rounded(px(8.))
            .border_1()
            .border_color(p.border)
            .bg(p.bg)
            .child(row);
        if let Some(FlashPresetTest::Finished(result)) = test {
            let mut output = div()
                .border_t_1()
                .border_color(p.line)
                .p(px(11.))
                .text_size(px(12.));
            match result {
                Ok(result) => {
                    let code = result
                        .exit_code
                        .map_or_else(|| "—".to_owned(), |code| code.to_string());
                    let elapsed = result.elapsed_ms.to_string();
                    output = output.child(crate::i18n::message(
                        "flashProbeExit",
                        &[("code", &code), ("elapsed", &elapsed)],
                    ));
                    let mut streams = div()
                        .id(SharedString::from(format!("preset-output-{}", preset.id)))
                        .mt(px(6.))
                        .max_h(px(180.))
                        .overflow_y_scroll()
                        .font_family(crate::theme::MONO_FONT);
                    for (name, stream) in [("stdout", &result.stdout), ("stderr", &result.stderr)] {
                        let stream = stream.trim_end();
                        if !stream.is_empty() {
                            streams =
                                streams.child(div().pb(px(6.)).child(format!("{name}\n{stream}")));
                        }
                    }
                    output = if result.stdout.trim().is_empty() && result.stderr.trim().is_empty() {
                        output.child(
                            div()
                                .mt(px(6.))
                                .text_color(p.muted)
                                .child(crate::i18n::label("flashProbeNoOutput")),
                        )
                    } else {
                        output.child(streams)
                    };
                }
                Err(error) => {
                    output = output.child(crate::i18n::message(
                        "flashProbeFailed",
                        &[("error", &error.to_string())],
                    ));
                }
            }
            card = card.child(output);
        }
        body = body.child(card);
    }
    body = body.child(
        btn(
            "add-preset",
            crate::i18n::label("addFlashTool"),
            &BtnVariant::Tiny,
            p,
        )
        .mt(px(11.))
        .on_click(cx.listener(|this, _ev, window, cx| {
            this.open_modal(crate::workspace::ModalKind::NewPreset, window, cx);
            cx.notify();
        }))
        .child(
            icon("plus")
                .size(px(14.))
                .text_color(p.muted)
                .into_any_element(),
        ),
    );
    settings_panel(
        "hard-drive",
        crate::i18n::label("flashPresets"),
        Some("settings.json"),
        body,
        p,
    )
}

fn a2a_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let default_a2a = duthub_core::A2aConfig::default();
    let a2a = w
        .settings
        .a2a
        .clone()
        .unwrap_or_else(|| default_a2a.clone());
    let card_url = format!(
        "http://{}:{}/.well-known/agent-card.json",
        a2a.host, a2a.port
    );
    let mut body = div();
    body = body.child(subtle(crate::i18n::label("a2aOverview"), p));
    let grid = div()
        .grid()
        .grid_cols(2)
        .gap(px(16.))
        .mt(px(12.))
        .child(field(
            crate::i18n::label("a2aAgentName"),
            w.text_field(
                In::A2aName,
                &a2a.agent_name,
                "Duthub Agent",
                false,
                p,
                window,
                cx,
            ),
            None,
            p,
        ))
        .child(field(
            crate::i18n::label("a2aPort"),
            w.text_field(
                In::A2aPort,
                &a2a.port.to_string(),
                "8790",
                true,
                p,
                window,
                cx,
            ),
            None,
            p,
        ))
        .child(field(
            crate::i18n::label("listenAddress"),
            div()
                .min_h(px(36.))
                .px(px(11.))
                .py(px(8.))
                .rounded(px(8.))
                .bg(p.bg)
                .border_1()
                .border_color(p.border)
                .text_size(px(12.5))
                .font_family(crate::theme::MONO_FONT)
                .text_color(p.muted)
                .child(SharedString::from(a2a.host.clone())),
            Some(crate::i18n::label("localOnlyHint")),
            p,
        ))
        .child(field(
            "Agent Card URL",
            div()
                .min_h(px(36.))
                .px(px(11.))
                .py(px(8.))
                .rounded(px(8.))
                .bg(p.bg)
                .border_1()
                .border_color(p.border)
                .text_size(px(12.5))
                .font_family(crate::theme::MONO_FONT)
                .text_color(p.accent)
                .child(SharedString::from(card_url.clone())),
            None,
            p,
        ));
    body = body.child(grid);
    body = body.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .mt(px(4.))
            .child(
                btn(
                    "a2a-save",
                    crate::i18n::label("saveConfig"),
                    &BtnVariant::TinyPrimary,
                    p,
                )
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    this.persist_settings();
                    this.notify_toast(crate::i18n::label("a2aSaved"));
                    cx.notify();
                })),
            )
            .child(
                btn(
                    "a2a-export",
                    crate::i18n::label("exportServerConfig"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    this.export_a2a_config();
                    cx.notify();
                })),
            )
            .child(
                btn(
                    "a2a-check",
                    crate::i18n::label("checkService"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    this.notify_toast(crate::i18n::label("a2aServiceStopped"));
                    cx.notify();
                })),
            )
            .child(subtle(
                &format!(
                    "状态：{}",
                    if w.a2a_running {
                        crate::i18n::label("running")
                    } else {
                        crate::i18n::label("notChecked")
                    }
                ),
                p,
            )),
    );
    body = body.child(
        div()
            .border_t_1()
            .border_color(p.line)
            .mt(px(16.))
            .pt(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui_kit::FontWeight(600.0))
                            .text_color(p.text)
                            .child(crate::i18n::label("peerAgents")),
                    )
                    .child(
                        btn(
                            "a2a-add-peer",
                            crate::i18n::label("addServer"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.open_modal(crate::workspace::ModalKind::NewPeer, window, cx);
                            cx.notify();
                        }))
                        .child(
                            icon("plus")
                                .size(px(14.))
                                .text_color(p.muted)
                                .into_any_element(),
                        ),
                    ),
            ),
    );
    if a2a.peers.is_empty() {
        body = body.child(
            div()
                .mt(px(10.))
                .child(subtle(crate::i18n::label("noRemoteAgents"), p)),
        );
    }
    for (index, peer) in a2a.peers.iter().enumerate() {
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .py(px(8.))
                .border_b_1()
                .border_color(p.line)
                .child(
                    div()
                        .font_weight(gpui_kit::FontWeight(600.0))
                        .text_size(px(12.))
                        .text_color(p.text)
                        .child(SharedString::from(peer.name.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(p.muted)
                        .overflow_hidden()
                        .child(SharedString::from(peer.card_url.clone())),
                )
                .child(
                    icon_btn(SharedString::from(format!("peer-del-{index}")), "trash", p).on_click(
                        cx.listener(move |this, _ev, _win, cx| {
                            if let Some(a2a) = this.settings.a2a.as_mut() {
                                a2a.peers.remove(index);
                            }
                            this.persist_settings();
                            cx.notify();
                        }),
                    ),
                ),
        );
    }
    body = body.child(
        div()
            .mt(px(14.))
            .child(subtle(crate::i18n::label("a2aExportHint"), p)),
    );
    settings_panel(
        "network",
        crate::i18n::label("agentCollaboration"),
        Some("A2A 1.0 · Agent Card"),
        body,
        p,
    )
}

fn commands_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let mut body = div();
    for (index, command) in w.settings.commands.iter().enumerate() {
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(9.))
                .py(px(9.))
                .px(px(11.))
                .mb(px(8.))
                .rounded(px(8.))
                .border_1()
                .border_color(p.border)
                .bg(p.bg)
                .child(
                    div()
                        .min_w(px(105.))
                        .font_weight(gpui_kit::FontWeight(600.0))
                        .text_size(px(12.))
                        .text_color(p.text)
                        .child(SharedString::from(command.name.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(p.accent)
                        .overflow_hidden()
                        .child(SharedString::from(command.value.clone())),
                )
                .child(
                    icon_btn(SharedString::from(format!("cmd-del-{index}")), "trash", p).on_click(
                        cx.listener(move |this, _ev, _win, cx| {
                            this.settings.commands.remove(index);
                            this.persist_settings();
                            cx.notify();
                        }),
                    ),
                ),
        );
    }
    body = body.child(
        btn(
            "add-command",
            crate::i18n::label("addCommandButton"),
            &BtnVariant::Normal,
            p,
        )
        .mt(px(9.))
        .on_click(cx.listener(|this, _ev, window, cx| {
            this.open_modal(crate::workspace::ModalKind::NewCommand, window, cx);
            cx.notify();
        }))
        .child(
            icon("plus")
                .size(px(15.))
                .text_color(p.muted)
                .into_any_element(),
        ),
    );
    settings_panel(
        "bolt",
        crate::i18n::t(w.lang, "customCommands"),
        Some(crate::i18n::label("globalShortcuts")),
        body,
        p,
    )
}

fn config_files_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let row = |title: &'static str,
               detail: &'static str,
               base: &'static str,
               p: &'static Palette,
               cx: &mut Context<Workbench>| {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(22.))
            .py(px(13.))
            .border_b_1()
            .border_color(p.line)
            .child(
                div()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_family(crate::theme::MONO_FONT)
                            .font_weight(gpui_kit::FontWeight(600.0))
                            .text_color(p.text)
                            .child(title),
                    )
                    .child(div().mt(px(5.)).child(subtle(detail, p))),
            )
            .child(
                btn(
                    SharedString::from(format!("{base}-import")),
                    crate::i18n::label("import"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.import_config(base, cx);
                    cx.notify();
                })),
            )
    };
    let body = div()
        .child(subtle(crate::i18n::label("configPathHint"), p))
        .child(subtle(&w.config_dir().display().to_string(), p))
        .child(row(
            "settings.json",
            crate::i18n::label("personalSettings"),
            "settings",
            p,
            cx,
        ))
        .child(
            row(
                "duts.json",
                crate::i18n::label("deviceSettings"),
                "duts",
                p,
                cx,
            )
            .border_b_0(),
        );
    settings_panel("code", crate::i18n::label("configFiles"), None, body, p)
}

#[cfg(test)]
mod usb_discovery_tests {
    use super::*;

    #[test]
    fn absent_saved_adb_binding_is_not_a_discovery_candidate() {
        let lang = crate::i18n::Lang::from_setting("zh-CN");
        let empty = Ok(Vec::new());
        let (options, status) = usb_debug_display(Some(&empty), "RK3576USB003", false, lang);
        assert!(options.is_empty());
        assert_eq!(status, "当前未检测到 ADB 设备");

        let connected = Ok(vec![duthub_core::AdbDevice {
            serial: "different-device".into(),
            state: "device".into(),
            model: None,
        }]);
        let (options, status) = usb_debug_display(Some(&connected), "RK3576USB003", false, lang);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].0, "different-device");
        assert_eq!(status, "当前未检测到绑定的设备");
    }
}
