use super::{Workbench, Palette, Window, Context, IntoElement, Styled, div, px, ParentElement, SharedString, StatefulInteractiveElement, InteractiveElement, FluentBuilder, Switch, AppContext, icon_btn, btn, BtnVariant, field, In, chip, settings_panel, Div, setting_row, toggle};

// ---------------------------------------------------------------------------
// 串口终端设置（设置浮层导航，与「控制设备」同级）
// ---------------------------------------------------------------------------

/// 串口设置页：Prompt 高亮规则 / 键盘映射 / 终端缓冲。
pub fn render_serial_settings(
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
                        crate::i18n::t(w.lang, "serialTitle").to_string(),
                    )),
            )
            .child(
                div()
                    .mt(px(2.))
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .child(SharedString::from(
                        crate::i18n::t(w.lang, "serialSub").to_string(),
                    )),
            ),
    );
    let mut grid = div().flex().flex_col();
    grid = grid.child(prompt_rules_panel(w, p, window, cx));
    grid = grid.child(keyboard_panel(w, p, cx));
    grid = grid.child(terminal_mouse_panel(w, p, cx));
    grid = grid.child(buffer_panel(w, p, window, cx));
    page.child(grid)
}

pub(super) fn default_prompt_rule_description(
    lang: crate::i18n::Lang,
    regex: &str,
) -> &'static str {
    use duthub_core::model::PromptRule;

    let key = match regex {
        PromptRule::USER_HOST_PATTERN => "promptRuleUserHost",
        PromptRule::ANDROID_HOST_PATTERN => "promptRuleAndroidHost",
        PromptRule::PATH_PATTERN => "promptRulePath",
        PromptRule::SUFFIX_PATTERN => "promptRuleSuffix",
        _ => "promptRuleCustom",
    };
    crate::i18n::t(lang, key)
}

pub(crate) fn prompt_rule_description(
    lang: crate::i18n::Lang,
    rule: &duthub_core::model::PromptRule,
) -> &str {
    rule.description
        .as_deref()
        .unwrap_or_else(|| default_prompt_rule_description(lang, &rule.regex))
}

/// Prompt highlight rule list, editor, and preview for the selected rule.
pub(super) fn prompt_rules_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let selected = w
        .serial_rule_selected
        .min(w.settings.serial.highlight_rules.len().saturating_sub(1));
    let mut body = div();
    if w.settings.serial.highlight_rules.is_empty() {
        body = body.child(
            div()
                .py(px(10.))
                .text_size(px(12.))
                .text_color(p.muted)
                .child(SharedString::from(
                    crate::i18n::t(w.lang, "serialNoRules").to_string(),
                )),
        );
    }
    for (idx, rule) in w.settings.serial.highlight_rules.iter().enumerate() {
        let description = rule
            .description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| default_prompt_rule_description(w.lang, &rule.regex));
        let active = idx == selected;
        let summary = if rule.regex.trim().is_empty() {
            crate::i18n::t(w.lang, "serialEmptyRule").to_string()
        } else {
            rule.regex.clone()
        };
        body = body.child(
            div()
                .id(SharedString::from(format!("serial-rule-row-{idx}")))
                .flex()
                .items_center()
                .gap(px(10.))
                .w_full()
                .py(px(8.))
                .px(px(10.))
                .rounded(px(8.))
                .when(active, |el| el.bg(p.hover))
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    if this.serial_rule_selected != idx {
                        this.select_serial_rule(idx);
                    }
                    cx.notify();
                }))
                .child(color_swatch(&rule.color, p))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_size(px(12.5))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(if rule.enabled { p.text } else { p.muted })
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(p.muted)
                                .child(description.to_owned()),
                        )
                        .child(SharedString::from(summary)),
                )
                .child(
                    Switch::new(SharedString::from(format!("serial-rule-en-{idx}")))
                        .checked(rule.enabled)
                        .color(gpui_kit::Hsla::from(p.accent))
                        .on_click(cx.listener(move |this, _ev, _win, cx| {
                            if let Some(rule) = this.settings.serial.highlight_rules.get_mut(idx) {
                                rule.enabled = !rule.enabled;
                                this.persist_settings();
                            }
                            cx.notify();
                        })),
                )
                .child(
                    icon_btn(SharedString::from(format!("serial-rule-del-{idx}")), "x", p)
                        .on_click(cx.listener(move |this, _ev, _win, cx| {
                            if this.settings.serial.highlight_rules.get(idx).is_some() {
                                this.settings.serial.highlight_rules.remove(idx);
                                let selected = this
                                    .serial_rule_selected
                                    .saturating_sub(usize::from(idx < this.serial_rule_selected))
                                    .min(
                                        this.settings
                                            .serial
                                            .highlight_rules
                                            .len()
                                            .saturating_sub(1),
                                    );
                                this.select_serial_rule(selected);
                                this.persist_settings();
                            }
                            cx.notify();
                        })),
                ),
        );
    }
    body = body.child(
        div().flex().gap(px(8.)).pt(px(6.)).child(
            btn(
                "serial-rule-add",
                crate::i18n::t(w.lang, "serialAddRule").to_string(),
                &BtnVariant::Normal,
                p,
            )
            .on_click(cx.listener(|this, _ev, _win, cx| {
                this.settings
                    .serial
                    .highlight_rules
                    .push(duthub_core::model::PromptRule {
                        regex: String::new(),
                        description: None,
                        color: "#edbe7a".into(),
                        enabled: true,
                    });
                this.select_serial_rule(this.settings.serial.highlight_rules.len() - 1);
                cx.notify();
            })),
        ),
    );
    // 选中规则编辑器 + 实时预览
    if let Some(rule) = w.settings.serial.highlight_rules.get(selected) {
        let regex = rule.regex.clone();
        let color = rule.color.clone();
        let description = prompt_rule_description(w.lang, rule).to_owned();
        let default_description = default_prompt_rule_description(w.lang, &regex);
        body = body.child(field(
            crate::i18n::t(w.lang, "regexDescription"),
            w.text_field(
                In::SerialRuleDescription,
                &description,
                default_description,
                false,
                p,
                window,
                cx,
            ),
            None,
            p,
        ));
        body = body.child(field(
            crate::i18n::t(w.lang, "regexPattern"),
            w.text_field(
                In::SerialRuleRegex,
                &regex,
                r"(?i)(error|fail|panic)",
                false,
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
                    chip(
                        SharedString::from(format!("serial-rule-color-{selected}-{c}")),
                        c,
                        p,
                    )
                    .on_click(cx.listener(move |this, _ev, _win, cx| {
                        if let Some(rule) = this
                            .settings
                            .serial
                            .highlight_rules
                            .get_mut(this.serial_rule_selected)
                        {
                            rule.color = c.to_string();
                            this.persist_settings();
                        }
                        cx.notify();
                    }))
                })),
            None,
            p,
        ));
        body = body.child(regex_preview_block(w.lang, &regex, &color, p));
        body = body.child(
            div().flex().gap(px(8.)).mt(px(12.)).child(
                btn(
                    "save-serial-rule",
                    crate::i18n::t(w.lang, "save").to_string(),
                    &BtnVariant::Primary,
                    p,
                )
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    if let Some(rule) = this
                        .settings
                        .serial
                        .highlight_rules
                        .get_mut(this.serial_rule_selected)
                        && rule
                            .description
                            .as_deref()
                            .is_some_and(|text| text.trim().is_empty())
                    {
                        rule.description = None;
                    }
                    this.persist_settings();
                    this.notify_toast(crate::i18n::label("regexSaved"));
                    cx.notify();
                })),
            ),
        );
    }
    settings_panel(
        "terminal",
        crate::i18n::t(w.lang, "regexTitle"),
        None,
        body,
        p,
    )
}

/// 高亮预览块：样例行按当前规则染色（无效正则给红字提示）。
pub(super) fn regex_preview_block(
    lang: crate::i18n::Lang,
    regex: &str,
    color: &str,
    p: &'static Palette,
) -> Div {
    let sample = "root@rk3576:~# dmesg | tail";
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
    match regex::Regex::new(regex) {
        Ok(re) if !regex.trim().is_empty() => {
            let mut last = 0;
            for m in re.find_iter(sample).take(20) {
                if m.start() > last {
                    preview =
                        preview.child(SharedString::from(sample[last..m.start()].to_string()));
                }
                preview = preview.child(
                    div()
                        .text_color(hex_to_rgba(color, p))
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
            preview = preview.child(div().text_color(p.red).child(SharedString::from(
                crate::i18n::message("regexInvalid", &[("error", &err.to_string())]),
            )));
        }
    }
    div()
        .child(
            div()
                .text_size(px(12.))
                .text_color(p.muted)
                .font_weight(gpui_kit::FontWeight(550.0))
                .child(SharedString::from(
                    crate::i18n::t(lang, "regexPreview").to_string(),
                )),
        )
        .child(preview)
}

/// 键盘映射：退格字节 + 回车行尾（PuTTY 键盘路径消费）。
pub(super) fn keyboard_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    use duthub_core::model::{SerialBackspace, SerialEnterEnding};
    let backspace = w.settings.serial.backspace;
    let enter = w.settings.serial.enter_ending;
    let mut body = div();
    body = body.child(setting_row(
        crate::i18n::label("serialBackspace"),
        crate::i18n::label("serialBackspaceHint"),
        w.select_field(
            "serial-backspace",
            match backspace {
                Some(SerialBackspace::CtrlH) => {
                    crate::i18n::t(w.lang, "optBackspaceCtrlH").to_string()
                }
                _ => crate::i18n::t(w.lang, "optBackspaceDel").to_string(),
            },
            vec![
                (crate::i18n::t(w.lang, "optBackspaceDel").to_string(), 0),
                (crate::i18n::t(w.lang, "optBackspaceCtrlH").to_string(), 1),
            ],
            Some(usize::from(backspace == Some(SerialBackspace::CtrlH))),
            std::sync::Arc::new(|this, index, _window, cx| {
                this.settings.serial.backspace = match index {
                    1 => Some(SerialBackspace::CtrlH),
                    _ => Some(SerialBackspace::Del),
                };
                this.persist_settings();
                cx.notify();
            }),
            p,
            cx,
        ),
        p,
    ));
    body = body.child(setting_row(
        crate::i18n::label("serialEnterEnding"),
        crate::i18n::label("serialEnterHint"),
        w.select_field(
            "serial-enter-ending",
            match enter {
                Some(SerialEnterEnding::Lf) => crate::i18n::t(w.lang, "optEnterLf").to_string(),
                Some(SerialEnterEnding::Crlf) => crate::i18n::t(w.lang, "optEnterCrlf").to_string(),
                _ => crate::i18n::t(w.lang, "optEnterCr").to_string(),
            },
            vec![
                (crate::i18n::t(w.lang, "optEnterCr").to_string(), 0),
                (crate::i18n::t(w.lang, "optEnterLf").to_string(), 1),
                (crate::i18n::t(w.lang, "optEnterCrlf").to_string(), 2),
            ],
            Some(match enter {
                Some(SerialEnterEnding::Lf) => 1,
                Some(SerialEnterEnding::Crlf) => 2,
                _ => 0,
            }),
            std::sync::Arc::new(|this, index, _window, cx| {
                this.settings.serial.enter_ending = match index {
                    1 => Some(SerialEnterEnding::Lf),
                    2 => Some(SerialEnterEnding::Crlf),
                    _ => Some(SerialEnterEnding::Cr),
                };
                this.persist_settings();
                cx.notify();
            }),
            p,
            cx,
        ),
        p,
    ));
    settings_panel(
        "keyboard",
        crate::i18n::t(w.lang, "serialKeysTitle"),
        None,
        body,
        p,
    )
}

pub(super) fn terminal_mouse_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    use crate::i18n::label;

    let paste = usize::from(w.settings.serial.paste_on_right_click);
    let choices = [label("terminalRightClickMenu"), label("paste")];
    let right_click = w.select_field(
        "terminal-right-click",
        choices[paste].to_owned(),
        choices
            .iter()
            .enumerate()
            .map(|(i, name)| (name.to_string(), i))
            .collect(),
        Some(paste),
        std::sync::Arc::new(|w, index, _, cx| {
            w.settings.serial.paste_on_right_click = index == 1;
            w.persist_settings();
            cx.notify();
        }),
        p,
        cx,
    );
    let copy = toggle(
        "terminal-copy-on-select",
        w.settings.serial.copy_on_select,
        p,
    )
    .on_click(cx.listener(|w, checked: &bool, _, cx| {
        w.settings.serial.copy_on_select = *checked;
        w.persist_settings();
        cx.notify();
    }));
    let body = div()
        .child(setting_row(label("terminalRightClick"), "", right_click, p))
        .child(setting_row(label("terminalCopyOnSelect"), "", copy, p));
    settings_panel("mouse", label("terminalMouseTitle"), None, body, p)
}

/// 终端缓冲：滚动缓冲行数（草稿解析 + 保存按钮提交，Baud 式）。
pub(super) fn buffer_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let draft = w.input_value(In::SerialScrollback);
    let default = duthub_device::term::SCROLLBACK_HISTORY.to_string();
    let body = setting_row(
        crate::i18n::label("serialScrollback"),
        crate::i18n::label("serialScrollbackHint"),
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().w(px(125.)).child(w.text_field(
                In::SerialScrollback,
                &draft,
                &default,
                true,
                p,
                window,
                cx,
            )))
            .child(
                btn(
                    "save-serial-buffer",
                    crate::i18n::label("save"),
                    &BtnVariant::Primary,
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.save_serial_scrollback();
                    cx.notify();
                })),
            ),
        p,
    );
    settings_panel(
        "list",
        crate::i18n::t(w.lang, "serialBufferTitle"),
        None,
        body,
        p,
    )
}

pub(super) fn color_swatch(color: &str, p: &'static Palette) -> Div {
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
