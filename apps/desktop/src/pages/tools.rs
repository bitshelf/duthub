use super::{
    BtnVariant, Context, Disableable, FlashPresetTest, In, InteractiveElement, IntoElement,
    Palette, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, Workbench,
    btn, div, field, icon_btn, panel_head, px, settings_panel, settings_panel_with_header, subtle,
};

/// Pre-set page for burning tools (classified as a separate category).
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

/// Compact download/update/cancel controls for a functional page that
/// configures one tool's executable path (the ADB section below, `FFmpeg` on the
/// capture page). Shares the settings page's install center, proxy policy and
/// progress slot, so what it shows is the real runtime state: idle →
/// download/update, running → live progress + cancel, another tool running →
/// disabled.
pub(crate) fn tool_install_controls(
    w: &Workbench,
    tool_id: &'static str,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    use duthub_desktop::tool_ui::{ToolPurpose, format_bytes, progress_percent};

    let spec = duthub_tooldown::spec_by_id(tool_id);
    let installed = match (duthub_desktop::tool_resolver::config_tools_root(), spec) {
        (Some(root), Some(spec)) => duthub_tooldown::installed_executable(&root, spec).is_some(),
        _ => false,
    };
    let busy = w.tool_installs.active(tool_id).is_some();
    let progress = w.tool_progress_view.get(tool_id).copied();
    let status = if busy {
        if let Some(pct) =
            progress.and_then(|progress| progress_percent(progress.bytes, progress.total))
        {
            crate::i18n::message("toolDownloading", &[("pct", &pct.to_string())])
        } else {
            let bytes = format_bytes(progress.map_or(0, |progress| progress.bytes));
            crate::i18n::message("toolDownloadingBytes", &[("bytes", &bytes)])
        }
    } else {
        String::new()
    };
    let mut controls = div().flex().items_center().gap(px(6.));
    if !status.is_empty() {
        controls = controls.child(subtle(&status, p));
    }
    controls.child(
        btn(
            SharedString::from(format!("tool-install-{tool_id}")),
            if busy {
                crate::i18n::label("toolBtnCancel")
            } else if installed {
                crate::i18n::label("toolBtnUpdate")
            } else {
                crate::i18n::label("toolBtnDownload")
            },
            &BtnVariant::Tiny,
            p,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            if busy {
                this.cancel_tool_install(tool_id);
            } else if let Some(spec) = spec {
                let purpose = if installed {
                    ToolPurpose::Update
                } else {
                    ToolPurpose::Download
                };
                this.start_tool_install(spec, purpose);
            }
            cx.notify();
        })),
    )
}

pub fn render_usb_debug(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let adb_input = w.path_input(In::AdbExe, "adb", p, window, cx);
    let hdc_input = w.path_input(In::HdcExe, "hdc", p, window, cx);
    let adb_panel = settings_panel(
        "usb",
        "ADB",
        None,
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(field(
                crate::i18n::label("debugExecutable"),
                adb_input,
                Some("adb devices -l"),
                p,
            ))
            .child(tool_install_controls(w, "adb", p, cx)),
        p,
    );
    // HDC ships inside the OpenHarmony SDK and has no standalone official
    // package (the public mirror only offers the multi-GB `.tar.gz` SDK), so no
    // download control is offered for it.
    let hdc_panel = settings_panel(
        "usb",
        "HDC",
        None,
        field(
            crate::i18n::label("debugExecutable"),
            hdc_input,
            Some("hdc list targets"),
            p,
        ),
        p,
    );
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
        .child(adb_panel)
        .child(hdc_panel)
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

pub(super) fn flash_preset_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let mut body = div();
    for (index, preset) in w.settings.flash_tools.iter().enumerate() {
        let id = preset.id.clone();
        let edit_id = id.clone();
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
                icon_btn(format!("preset-edit-{edit_id}"), "pencil", p)
                    .tooltip(crate::i18n::label("editFlashTool"))
                    .disabled(running)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_modal(
                            crate::workspace::ModalKind::EditPreset(edit_id.clone()),
                            window,
                            cx,
                        );
                    })),
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
    let add = btn(
        "add-preset",
        crate::i18n::label("addFlashTool"),
        &BtnVariant::Tiny,
        p,
    )
    .icon(crate::icons::kit_icon("plus").size(px(14.)))
    .on_click(cx.listener(|this, _ev, window, cx| {
        this.open_modal(crate::workspace::ModalKind::NewPreset, window, cx);
        cx.notify();
    }));
    let header = panel_head("hard-drive", crate::i18n::label("flashPresets"), p)
        .child(div().flex_none().child(add));
    settings_panel_with_header(header, body, p)
}
