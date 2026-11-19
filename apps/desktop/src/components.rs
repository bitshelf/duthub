//! 共享 UI 构件（对照原型 CSS 的 1:1 尺寸/圆角/配色）。

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::switch::Switch;
use gpui_kit::{
    App, Div, InteractiveElement, IntoElement, ParentElement, SharedString, Stateful, Styled, div,
    px,
};

use crate::icons::{icon, kit_icon};
use crate::theme::Palette;

/// `.icon-btn`：32px 图标按钮（topbar/tabbar 工具区）——kit Button ghost 变体。
pub fn icon_btn(id: impl Into<SharedString>, name: &str, p: &Palette) -> Button {
    let id: SharedString = id.into();
    Button::new(id)
        .ghost()
        .icon(
            kit_icon(name)
                .size(px(17.0))
                .text_color(gpui_kit::Hsla::from(p.muted)),
        )
        .w(px(32.))
        .h(px(32.))
        .rounded(px(7.))
}

/// `.btn`：raised 按钮；variant 决定 primary/danger/tiny。
pub enum BtnVariant {
    Normal,
    Primary,
    Tiny,
    TinyPrimary,
}

pub fn btn(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    variant: &BtnVariant,
    _p: &Palette,
) -> Button {
    let id: SharedString = id.into();
    let mut b = Button::new(id).label(label);
    b = match variant {
        BtnVariant::Primary | BtnVariant::TinyPrimary => b.primary(),
        _ => b.secondary(),
    };
    if matches!(variant, BtnVariant::Tiny | BtnVariant::TinyPrimary) {
        b = b.compact();
    }
    b
}

/// `.panel-head`：面板标题行（icon + 标题 + 右侧附件由调用方追加）。
pub fn panel_head(icon_name: &str, title: &str, p: &Palette) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(10.))
        .px(px(18.))
        .pt(px(15.))
        .pb(px(14.))
        .border_b_1()
        .border_color(p.line)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(9.))
                .text_size(px(13.))
                .font_weight(gpui_kit::FontWeight(670.0))
                .text_color(p.text)
                .child(icon(icon_name).size(px(17.)).text_color(p.accent))
                .child(SharedString::from(title.to_string())),
        )
}

/// `.field`：label + 控件 + 可选 help 的纵向表单域（控件由调用方给）。
pub fn field(label: &str, control: impl IntoElement, help: Option<&str>, p: &Palette) -> Div {
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .min_w_0()
        .mb(px(14.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(p.muted)
                .font_weight(gpui_kit::FontWeight(550.0))
                .child(SharedString::from(label.to_string())),
        )
        .child(control);
    if let Some(help) = help {
        col = col.child(
            div()
                .text_size(px(11.))
                .text_color(p.faint)
                .child(SharedString::from(help.to_string())),
        );
    }
    col
}

/// `.chip`：快速波特率等小按钮。
pub fn chip(id: impl Into<SharedString>, label: &str, p: &Palette) -> Stateful<Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .px(px(9.))
        .py(px(4.))
        .rounded(px(7.))
        .bg(p.raised)
        .border_1()
        .border_color(p.border)
        .text_size(px(11.))
        .font_family(crate::theme::MONO_FONT)
        .text_color(p.muted)
        .hover(|s| s.text_color(p.text).bg(p.hover))
        .child(SharedString::from(label.to_string()))
}

/// 开关（kit Switch）：受控组件，on 为当前态，on_click 收到请求值。
pub fn toggle(id: &'static str, on: bool, p: &Palette) -> Switch {
    Switch::new(id)
        .checked(on)
        .color(gpui_kit::Hsla::from(p.accent))
}

/// `.subtle`。
pub fn subtle(text: &str, p: &Palette) -> Div {
    div()
        .text_size(px(11.))
        .text_color(p.muted)
        .child(SharedString::from(text.to_string()))
}

/// `.pill`。
#[allow(dead_code)]
pub fn pill(label: &str, p: &Palette) -> Div {
    div()
        .px(px(7.))
        .py(px(3.))
        .rounded(px(6.))
        .bg(p.raised)
        .border_1()
        .border_color(p.border)
        .text_size(px(10.))
        .font_family(crate::theme::MONO_FONT)
        .text_color(p.muted)
        .child(SharedString::from(label.to_string()))
}

/// 输入框统一按键取字符（供 pages/workspace 复用）。
pub fn typed_char(event: &gpui_kit::KeyDownEvent) -> Option<String> {
    let k = &event.keystroke;
    if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
        return None;
    }
    if let Some(ch) = k.key_char.as_ref().filter(|s| s.chars().count() == 1) {
        return Some(ch.clone());
    }
    (k.key.chars().count() == 1).then(|| k.key.clone())
}

/// App 级剪贴板写入（复制终端输出等）。
pub fn copy_text(cx: &App, text: &str) {
    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text.to_string()));
}
