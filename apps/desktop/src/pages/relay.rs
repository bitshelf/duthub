use super::{Workbench, Palette, Window, Context, IntoElement, FluentBuilder, InteractiveElement, ParentElement, Styled, div, px, StatefulInteractiveElement, SharedString, AppContext, InputState, InputEvent, In, setting_row, Input, icon_btn, Disableable, btn, BtnVariant, subtle, RelayConfig, switch_panel_header, toggle, RelayChannels, collapsible_panel, field, Checkbox, chip};

pub(crate) fn render_relay_boards(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let lang = w.lang;
    // Defensive: a delete may shrink the list while an index is still set.
    let board_count = w.duts.file.relay_boards.len();
    if board_count > 0 && w.relay_board_selected >= board_count {
        w.relay_board_selected = board_count - 1;
        w.relay_board_draft = None;
        w.relay_board_name = None;
    }
    let dirty = w.relay_board_dirty();
    let saved_id = w.selected_relay_board().map(|board| board.id.clone());

    // Left rail (macOS-Settings / Rex style): one selectable row per configured
    // device with its serial port underneath; "+ add" sits at the bottom.
    let mut list = div().flex().flex_col().w(px(220.)).flex_none().child(
        div()
            .px(px(10.))
            .pb(px(6.))
            .text_size(px(10.5))
            .font_family(crate::theme::MONO_FONT)
            .text_color(p.faint)
            .child(crate::i18n::t(lang, "relayBoard")),
    );
    for (index, board) in w.duts.file.relay_boards.iter().enumerate() {
        let active = index == w.relay_board_selected;
        let here_dirty = active && dirty;
        let name = board.name.clone();
        let port = if board.node.is_empty() {
            crate::i18n::label("noSerialPortChosen").to_string()
        } else {
            board.node.clone()
        };
        list = list.child(
            div()
                .id(SharedString::from(format!("relay-board-row-{index}")))
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(px(10.))
                .py(px(8.))
                .rounded(px(8.))
                .cursor_pointer()
                .when(active, |row| row.bg(p.hover))
                .hover(|row| row.bg(p.hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.relay_board_selected != index {
                        // Switching discards an unsaved draft: edits commit
                        // only through "save device".
                        this.relay_board_selected = index;
                        this.relay_board_draft = None;
                        this.relay_board_name = None;
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_size(px(12.5))
                                .text_color(if active { p.text } else { p.muted })
                                .child(SharedString::from(name)),
                        )
                        .when(here_dirty, |row| {
                            row.child(div().flex_none().size(px(6.)).rounded(px(3.)).bg(p.accent))
                        }),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(p.faint)
                        .child(SharedString::from(port)),
                ),
        );
    }
    list = list.child(
        div()
            .id("relay-board-add")
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(8.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(p.accent)
            .hover(|row| row.bg(p.hover))
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
                this.relay_board_draft = None;
                this.relay_board_name = None;
                this.duts.dirty = true;
                // Persist immediately so a half-configured board is never a
                // phantom entry that disappears on the next reload.
                this.save_duts();
                cx.notify();
            }))
            .child("+")
            .child(crate::i18n::t(lang, "addRelayBoard")),
    );

    // Right: grouped settings rows (label left, control right), with the
    // primary action at the bottom right and the destructive action separated
    // at the bottom left.
    let mut detail = div().flex_1().min_w_0().flex().flex_col();
    if let Some(edit) = w.relay_board_edit() {
        if w.relay_board_name
            .as_ref()
            .is_none_or(|(id, _)| *id != edit.id)
        {
            let id = edit.id.clone();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(edit.name.clone()));
            cx.subscribe(&input, move |w, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    // Draft-only: the board list is written on "save device".
                    w.set_relay_board_field(In::RelayBoardName, input.read(cx).value().to_string());
                    cx.notify();
                }
            })
            .detach();
            w.relay_board_name = Some((id, input));
        }
        let name = &w.relay_board_name.as_ref().expect("relay name input").1;
        detail = detail.child(setting_row(
            crate::i18n::t(lang, "name"),
            "",
            div().w(px(320.)).child(Input::new(name)),
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
            &edit.node,
            "COM25",
            true,
            &ports,
            window,
            cx,
        );
        detail = detail.child(setting_row(
            crate::i18n::t(lang, "relaySerialPort"),
            crate::i18n::t(lang, "selectAConnectedPortOrType"),
            div()
                .w(px(320.))
                .flex()
                .items_center()
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
            p,
        ));
        detail = detail.child(setting_row(
            crate::i18n::t(lang, "channelCount"),
            "",
            div().w(px(120.)).child(
                w.select_field(
                    "relay-board-channel-count",
                    edit.channel_count.to_string(),
                    (1..=usize::from(edit.channel_count.max(8)))
                        .map(|count| (count.to_string(), count))
                        .collect(),
                    Some(usize::from(edit.channel_count)),
                    std::sync::Arc::new(|this, count, _, cx| {
                        this.set_relay_board_channel_count(count as u8);
                        cx.notify();
                    }),
                    p,
                    cx,
                ),
            ),
            p,
        ));
        // Only the CH340 driver exists today, so this is a read-only value
        // rather than a single-option dropdown.
        let driver_text = if edit.driver.is_empty() || edit.driver.eq_ignore_ascii_case("CH340") {
            crate::i18n::label("ch340RelayDriver").to_string()
        } else {
            edit.driver.clone()
        };
        detail = detail.child(setting_row(
            crate::i18n::label("controlDeviceDriver"),
            "",
            div()
                .text_size(px(12.5))
                .text_color(p.muted)
                .child(SharedString::from(driver_text)),
            p,
        ));
        detail = detail.child(
            div()
                .pt(px(14.))
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .id("control-device-delete")
                        .px(px(8.))
                        .py(px(6.))
                        .rounded(px(6.))
                        .text_size(px(12.))
                        .text_color(p.red)
                        .cursor_pointer()
                        .hover(|el| el.bg(p.hover))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // Destructive: confirm first (same dialog family as
                            // deleting a connection).
                            if let Some(id) = saved_id.clone() {
                                this.open_modal(
                                    crate::workspace::ModalKind::RemoveControlDevice(id),
                                    window,
                                    cx,
                                );
                            }
                        }))
                        .child(crate::i18n::label("deleteControlDevice")),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("relay-board-revert")
                                .px(px(10.))
                                .py(px(6.))
                                .rounded(px(6.))
                                .text_size(px(12.))
                                .text_color(if dirty { p.muted } else { p.faint })
                                .cursor_pointer()
                                .when(dirty, |el| el.hover(|s| s.bg(p.hover).text_color(p.text)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.revert_relay_board();
                                    cx.notify();
                                }))
                                .child(crate::i18n::t(lang, "revertChanges")),
                        )
                        .child(
                            btn(
                                "relay-board-save",
                                crate::i18n::t(lang, "saveRelayBoard"),
                                &BtnVariant::Primary,
                                p,
                            )
                            .disabled(!dirty)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_relay_board();
                                cx.notify();
                            })),
                        ),
                ),
        );
    } else {
        detail = detail.child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .py(px(40.))
                .child(subtle(crate::i18n::t(lang, "addARelayBoardToBind"), p)),
        );
    }

    div()
        .w_full()
        .max_w(px(980.))
        .mx_auto()
        .p(px(24.))
        .flex()
        .items_start()
        .gap(px(20.))
        .child(list)
        .child(detail)
}

pub(super) fn relay_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let boards: Vec<_> = w
        .duts
        .file
        .relay_boards
        .iter()
        .filter(|board| !board.node.trim().is_empty())
        .collect();
    let board_ids: Vec<_> = boards.iter().map(|board| board.id.clone()).collect();
    let enabled = !matches!(dut.relay, RelayConfig::Disabled);
    let initial_board = board_ids.first().cloned();
    let toggle_dut = dut.id.clone();
    let header = switch_panel_header(
        "bolt",
        crate::i18n::t(w.lang, "relay"),
        toggle("dut-relay-toggle", enabled, p)
            .disabled(!enabled && initial_board.is_none())
            .on_click(cx.listener(move |this, checked, _, cx| {
                let relay = if *checked {
                    let Some(board_id) = initial_board.clone() else {
                        return;
                    };
                    RelayConfig::UsbBoard {
                        board_id,
                        channels: RelayChannels::default(),
                    }
                } else {
                    this.release_relay(cx);
                    RelayConfig::Disabled
                };
                if let Some(dut) = this.duts.connection_mut(&toggle_dut) {
                    dut.relay = relay;
                    this.duts.mark_connection_dirty();
                }
                cx.notify();
            })),
        p,
    );
    if !enabled {
        return collapsible_panel(header, None, p);
    }
    let options: Vec<_> = boards
        .iter()
        .enumerate()
        .map(|(index, board)| (format!("{} · {}", board.name, board.node), index))
        .collect();
    let selected = match &dut.relay {
        RelayConfig::UsbBoard { board_id, .. } => board_ids.iter().position(|id| id == board_id),
        _ => None,
    };
    let value = selected.map_or_else(|| crate::i18n::label("controlDeviceUnavailable").into(), |i| options[i].0.clone());
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
                if let Some(board_id) = board_ids.get(value)
                    && let Some(dut) = this.duts.selected_mut()
                {
                    dut.relay = RelayConfig::UsbBoard {
                        board_id: board_id.clone(),
                        channels: RelayChannels::default(),
                    };
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
                                this.press_relay(&target, key, channel, window, cx);
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
    collapsible_panel(header, Some(body), p)
}

pub(super) fn set_relay_channel(w: &mut Workbench, key: &str, channel: Option<u8>) {
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
