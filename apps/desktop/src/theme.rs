//! Theme token — 1:1 migration of `docs/ui-spec/theme.md` (prototype: `:root` with both dark and
//! light themes).
//!
//! `const fn` replicates the semantics of `gpui_kit::rgb` (hexadecimal to Rgba format, with alpha
//! value set to 1.0).

use gpui_kit::Rgba;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    pub fn from_setting(value: &str) -> ThemeMode {
        match value {
            "light" => ThemeMode::Light,
            _ => ThemeMode::Dark, // System: The desktop interface defaults to a dark color scheme.
        }
    }
}

/// Theme color palette (variable names correspond one-to-one with the original CSS variables).
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Rgba,
    pub rail: Rgba,
    pub surface: Rgba,
    #[expect(dead_code, reason = "design token kept in complete Palette definition")]
    pub panel: Rgba,
    pub raised: Rgba,
    pub hover: Rgba,
    pub border: Rgba,
    pub line: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub faint: Rgba,
    pub accent: Rgba,
    pub accent_weak: Rgba,
    pub accent_contrast: Rgba,
    pub blue: Rgba,
    pub warn: Rgba,
    pub red: Rgba,
    pub term: Rgba,
    pub term_fg: Rgba,
    pub prompt: Rgba,
    pub match_: Rgba,
    pub backdrop: Rgba,
    // The color of the box-shadow effect in the prototype .dot file is #48cbb91a.
    pub accent_ring: Rgba,
    // Original configuration: .config-sidebar; color-mix(surface 86%, black)
    pub sidebar: Rgba,
}

const fn rgba_const(hex: u32) -> Rgba {
    rgba_alpha(hex, 1.0)
}

#[expect(
    clippy::cast_precision_loss,
    reason = "8-bit RGB channels (0..=255) are exact in f32"
)]
const fn rgba_alpha(hex: u32, a: f32) -> Rgba {
    let r = ((hex >> 16) & 0xff) as f32 / 255.0;
    let g = ((hex >> 8) & 0xff) as f32 / 255.0;
    let b = (hex & 0xff) as f32 / 255.0;
    Rgba { r, g, b, a }
}

/// The 0xRRGGBB of the terminal grid snapshot is converted to Rgba (used for serial screen color
/// rendering).
pub fn rgba_hex(hex: u32) -> Rgba {
    rgba_alpha(hex, 1.0)
}

pub const DARK: Palette = Palette {
    bg: rgba_const(0x0a_10_19),
    rail: rgba_const(0x0c_13_1d),
    surface: rgba_const(0x10_19_26),
    panel: rgba_const(0x12_1d_2b),
    raised: rgba_const(0x18_26_38),
    hover: rgba_const(0x1e_2c_3d),
    border: rgba_const(0x26_34_45),
    line: rgba_const(0x1e_2b_3b),
    text: rgba_const(0xe8_f0_f7),
    muted: rgba_const(0x8c_a0_b5),
    faint: rgba_const(0x5f_75_8b),
    accent: rgba_const(0x56_cb_b9),
    accent_weak: rgba_const(0x11_3d_3c),
    accent_contrast: rgba_const(0x07_1b_1b),
    blue: rgba_const(0x88_b7_ed),
    warn: rgba_const(0xe9_ba_78),
    red: rgba_const(0xec_82_91),
    term: rgba_const(0x09_0f_18),
    term_fg: rgba_const(0xcf_db_e8),
    prompt: rgba_const(0x71_dd_bf),
    match_: rgba_const(0xed_be_7a),
    backdrop: rgba_alpha(0x03_09_11, 0.68),
    accent_ring: rgba_alpha(0x48_cb_b9, 0.10),
    sidebar: rgba_const(0x0e_15_20),
};

pub const LIGHT: Palette = Palette {
    bg: rgba_const(0xf4_f7_fb),
    rail: rgba_const(0xea_f0_f6),
    surface: rgba_const(0xff_ff_ff),
    panel: rgba_const(0xf8_fb_ff),
    raised: rgba_const(0xed_f3_fa),
    hover: rgba_const(0xe2_eb_f4),
    border: rgba_const(0xd3_de_e9),
    line: rgba_const(0xe2_ea_f2),
    text: rgba_const(0x18_2c_40),
    muted: rgba_const(0x63_79_8c),
    faint: rgba_const(0x84_95_a6),
    accent: rgba_const(0x16_7e_75),
    accent_weak: rgba_const(0xd8_f2_ed),
    accent_contrast: rgba_const(0x07_1b_1b),
    blue: rgba_const(0x17_66_ac),
    warn: rgba_const(0xa6_6e_1d),
    red: rgba_const(0xa9_3e_55),
    term: rgba_const(0xfb_fd_ff),
    term_fg: rgba_const(0x24_40_58),
    prompt: rgba_const(0x13_82_6b),
    match_: rgba_const(0xa1_6b_11),
    backdrop: rgba_alpha(0x03_09_11, 0.68),
    accent_ring: rgba_alpha(0x48_cb_b9, 0.10),
    sidebar: rgba_const(0xdf_e5_ec),
};

impl ThemeMode {
    #[expect(dead_code, reason = "palette lookup retained alongside ThemeMode")]
    pub fn palette(self) -> &'static Palette {
        match self {
            ThemeMode::Dark => &DARK,
            ThemeMode::Light => &LIGHT,
        }
    }
}

pub const MONO_FONT: &str = "Consolas";
pub const UI_FONT: &str = "Segoe UI";

use gpui_kit::component::theme::Theme as KitTheme;
use gpui_kit::component::theme::ThemeMode as KitThemeMode;
use gpui_kit::px;

/// Brand-specific colors are incorporated into the gpui-kit global theme settings. The component
/// structure and behavior are handled by the kit, while the color scheme follows the V35 brand
/// guidelines. This function must be called after `gpui_kit::init`; it should also be called when
/// switching pages or importing the settings.json file.
pub fn apply_brand(cx: &mut gpui_kit::App, mode: ThemeMode) {
    let p = match mode {
        ThemeMode::Dark => &DARK,
        ThemeMode::Light => &LIGHT,
    };
    let t = KitTheme::global_mut(cx);
    t.mode = match mode {
        ThemeMode::Dark => KitThemeMode::Dark,
        ThemeMode::Light => KitThemeMode::Light,
    };
    t.font_family = UI_FONT.into();
    t.mono_font_family = MONO_FONT.into();
    t.font_size = px(13.0); // Original benchmark font size
    t.mono_font_size = px(12.5);
    t.radius = px(6.0);
    t.radius_lg = px(10.0);
    t.window_radius = px(8.0);

    let c = &mut t.colors;
    // base
    c.background = p.bg.into();
    c.foreground = p.text.into();
    c.border = p.border.into();
    c.input = p.raised.into();
    c.ring = p.accent.into();
    c.overlay = p.backdrop.into();
    c.window_border = p.border.into();
    c.selection = p.accent_weak.into();
    // Main Button / Link
    c.primary = p.accent.into();
    c.primary_foreground = p.accent_contrast.into();
    c.primary_hover = p.accent.into();
    c.primary_active = p.accent.into();
    c.link = p.blue.into();
    // Secondary/Hover highlighting (Shadcn semantics: accent refers to hovering effects)
    c.secondary = p.raised.into();
    c.secondary_foreground = p.text.into();
    c.secondary_hover = p.hover.into();
    c.secondary_active = p.hover.into();
    c.muted = p.hover.into();
    c.muted_foreground = p.muted.into();
    c.accent = p.accent_weak.into();
    c.accent_foreground = p.text.into();
    // Button group token (uses the variant of the Kit Button)
    c.button = p.raised.into();
    c.button_foreground = p.text.into();
    c.button_hover = p.hover.into();
    c.button_active = p.hover.into();
    c.button_primary = p.accent.into();
    c.button_primary_foreground = p.accent_contrast.into();
    c.button_primary_hover = p.accent.into();
    c.button_primary_active = p.accent.into();
    c.button_secondary = p.raised.into();
    c.button_secondary_foreground = p.text.into();
    c.button_secondary_hover = p.hover.into();
    c.button_secondary_active = p.hover.into();
    c.button_danger = p.red.into();
    c.button_danger_foreground = p.bg.into();
    c.button_danger_hover = p.red.into();
    c.button_danger_active = p.red.into();
    // Status colors
    c.danger = p.red.into();
    c.danger_foreground = p.bg.into();
    c.danger_hover = p.red.into();
    c.danger_active = p.red.into();
    c.warning = p.warn.into();
    c.warning_foreground = p.bg.into();
    c.warning_hover = p.warn.into();
    c.warning_active = p.warn.into();
    c.success = p.accent.into();
    c.success_foreground = p.accent_contrast.into();
    c.success_hover = p.accent.into();
    c.success_active = p.accent.into();
    c.info = p.blue.into();
    c.info_foreground = p.bg.into();
    c.info_hover = p.blue.into();
    c.info_active = p.blue.into();
    // Layer / Cards
    c.popover = p.surface.into();
    c.popover_foreground = p.text.into();
    // Title Bar / Sidebar
    c.title_bar = p.rail.into();
    c.title_bar_border = p.line.into();
    c.sidebar = p.sidebar.into();
    c.sidebar_border = p.line.into();
    c.sidebar_foreground = p.muted.into();
    c.sidebar_accent = p.hover.into();
    c.sidebar_accent_foreground = p.text.into();
    c.sidebar_primary = p.accent.into();
    c.sidebar_primary_foreground = p.accent_contrast.into();
    // Tabs
    c.tab = p.surface.into();
    c.tab_bar = p.surface.into();
    c.tab_active = p.term.into();
    c.tab_foreground = p.muted.into();
    c.tab_active_foreground = p.text.into();
    // Lists / Tables
    c.list = p.surface.into();
    c.list_hover = p.hover.into();
    c.list_active = p.hover.into();
    c.list_active_border = p.border.into();
    c.list_even = p.bg.into();
    c.list_head = p.raised.into();
    c.table = p.surface.into();
    c.table_head = p.raised.into();
    c.table_head_foreground = p.muted.into();
    c.table_even = p.bg.into();
    c.table_hover = p.hover.into();
    c.table_active = p.hover.into();
    c.table_active_border = p.border.into();
    c.table_row_border = p.line.into();
    c.table_foot = p.surface.into();
    c.table_foot_foreground = p.muted.into();
    // The remaining controls
    c.switch = p.faint.into();
    c.switch_thumb = p.text.into();
    c.progress_bar = p.accent.into();
    c.skeleton = p.hover.into();
    c.caret = p.prompt.into();
    c.slider_bar = p.hover.into();
    c.slider_thumb = p.accent.into();
    c.accordion = p.surface.into();
    c.group_box = p.surface.into();
    c.group_box_foreground = p.muted.into();
    c.description_list_label = p.muted.into();
    c.description_list_label_foreground = p.text.into();
    c.status_bar = p.rail.into();
    c.status_bar_border = p.line.into();
    c.drag_border = p.border.into();
    c.drop_target = p.accent_weak.into();
    c.scrollbar = p.line.into();
    c.scrollbar_thumb = p.faint.into();
    c.scrollbar_thumb_hover = p.muted.into();
    // Primary color slots (directly connected to kit red, blue, and green)
    c.red = p.red.into();
    c.red_light = p.red.into();
    c.blue = p.blue.into();
    c.blue_light = p.blue.into();
    c.green = p.accent.into();
    c.green_light = p.accent.into();

    // For older versions of tokens, legacy components such as Notification/Dialog directly access
    // tokens.* These tokens do not synchronize automatically with colors—if they are missing, it
    // will result in white text on a white background.
    let t2 = &mut t.tokens;
    t2.background = gpui_kit::Hsla::from(p.bg).into();
    t2.popover = gpui_kit::Hsla::from(p.surface).into();
    t2.primary = gpui_kit::Hsla::from(p.accent).into();
    t2.secondary = gpui_kit::Hsla::from(p.raised).into();
    t2.muted = gpui_kit::Hsla::from(p.hover).into();
    t2.accent = gpui_kit::Hsla::from(p.accent_weak).into();
    t2.danger = gpui_kit::Hsla::from(p.red).into();
    t2.button = gpui_kit::Hsla::from(p.raised).into();
    t2.button_hover = gpui_kit::Hsla::from(p.hover).into();
    t2.button_active = gpui_kit::Hsla::from(p.hover).into();
    t2.button_primary = gpui_kit::Hsla::from(p.accent).into();
    t2.button_primary_hover = gpui_kit::Hsla::from(p.accent).into();
    t2.button_primary_active = gpui_kit::Hsla::from(p.accent).into();
    t2.button_secondary = gpui_kit::Hsla::from(p.raised).into();
    t2.button_secondary_hover = gpui_kit::Hsla::from(p.hover).into();
    t2.button_secondary_active = gpui_kit::Hsla::from(p.hover).into();
}
