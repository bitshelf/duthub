//! 主题 token —— 1:1 移植 `docs/ui-spec/theme.md`（原型 `:root` dark + light）。
//!
//! `const fn` 复刻 `gpui_kit::rgb` 语义（hex → Rgba，alpha=1.0）。

use gpui_kit::Rgba;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[allow(dead_code)]
impl ThemeMode {
    pub fn from_setting(value: &str) -> ThemeMode {
        match value {
            "light" => ThemeMode::Light,
            _ => ThemeMode::Dark, // system：桌面端默认深色
        }
    }
}

/// 主题调色板（变量名与原型 CSS 变量一一对应）。
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Rgba,
    #[allow(dead_code)]
    pub rail: Rgba,
    pub surface: Rgba,
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub warn: Rgba,
    pub red: Rgba,
    pub term: Rgba,
    pub term_fg: Rgba,
    pub prompt: Rgba,
    pub match_: Rgba,
    pub backdrop: Rgba,
    // 原型 .dot 的 box-shadow 光环色 #48cbb91a
    pub accent_ring: Rgba,
    // 原型 .config-sidebar：color-mix(surface 86%, black)
    pub sidebar: Rgba,
}

const fn rgba_const(hex: u32) -> Rgba {
    rgba_alpha(hex, 1.0)
}

const fn rgba_alpha(hex: u32, a: f32) -> Rgba {
    let r = ((hex >> 16) & 0xff) as f32 / 255.0;
    let g = ((hex >> 8) & 0xff) as f32 / 255.0;
    let b = (hex & 0xff) as f32 / 255.0;
    Rgba { r, g, b, a }
}

/// 终端网格快照的 0xRRGGBB → Rgba（SerialScreen 行色渲染用）。
pub fn rgba_hex(hex: u32) -> Rgba {
    rgba_alpha(hex, 1.0)
}

pub const DARK: Palette = Palette {
    bg: rgba_const(0x0a1019),
    rail: rgba_const(0x0c131d),
    surface: rgba_const(0x101926),
    panel: rgba_const(0x121d2b),
    raised: rgba_const(0x182638),
    hover: rgba_const(0x1e2c3d),
    border: rgba_const(0x263445),
    line: rgba_const(0x1e2b3b),
    text: rgba_const(0xe8f0f7),
    muted: rgba_const(0x8ca0b5),
    faint: rgba_const(0x5f758b),
    accent: rgba_const(0x56cbb9),
    accent_weak: rgba_const(0x113d3c),
    accent_contrast: rgba_const(0x071b1b),
    blue: rgba_const(0x88b7ed),
    warn: rgba_const(0xe9ba78),
    red: rgba_const(0xec8291),
    term: rgba_const(0x090f18),
    term_fg: rgba_const(0xcfdbe8),
    prompt: rgba_const(0x71ddbf),
    match_: rgba_const(0xedbe7a),
    backdrop: rgba_alpha(0x030911, 0.68),
    accent_ring: rgba_alpha(0x48cbb9, 0.10),
    sidebar: rgba_const(0x0e1520),
};

pub const LIGHT: Palette = Palette {
    bg: rgba_const(0xf4f7fb),
    rail: rgba_const(0xeaf0f6),
    surface: rgba_const(0xffffff),
    panel: rgba_const(0xf8fbff),
    raised: rgba_const(0xedf3fa),
    hover: rgba_const(0xe2ebf4),
    border: rgba_const(0xd3dee9),
    line: rgba_const(0xe2eaf2),
    text: rgba_const(0x182c40),
    muted: rgba_const(0x63798c),
    faint: rgba_const(0x8495a6),
    accent: rgba_const(0x167e75),
    accent_weak: rgba_const(0xd8f2ed),
    accent_contrast: rgba_const(0x071b1b),
    blue: rgba_const(0x1766ac),
    warn: rgba_const(0xa66e1d),
    red: rgba_const(0xa93e55),
    term: rgba_const(0xfbfdff),
    term_fg: rgba_const(0x244058),
    prompt: rgba_const(0x13826b),
    match_: rgba_const(0xa16b11),
    backdrop: rgba_alpha(0x030911, 0.68),
    accent_ring: rgba_alpha(0x48cbb9, 0.10),
    sidebar: rgba_const(0xdfe5ec),
};

impl ThemeMode {
    #[allow(dead_code)]
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

/// 品牌色注入 gpui-kit 全局主题：组件结构与行为交给 kit，配色沿用 V35 品牌 token。
/// 必须在 `gpui_kit::init` 之后调用；设置页切换主题 / 导入 settings.json 时再次调用。
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
    t.font_size = px(13.0); // 原型基准字号
    t.mono_font_size = px(12.5);
    t.radius = px(6.0);
    t.radius_lg = px(10.0);
    t.window_radius = px(8.0);

    let c = &mut t.colors;
    // 基底
    c.background = p.bg.into();
    c.foreground = p.text.into();
    c.border = p.border.into();
    c.input = p.raised.into();
    c.ring = p.accent.into();
    c.overlay = p.backdrop.into();
    c.window_border = p.border.into();
    c.selection = p.accent_weak.into();
    // 主按钮 / 链接
    c.primary = p.accent.into();
    c.primary_foreground = p.accent_contrast.into();
    c.primary_hover = p.accent.into();
    c.primary_active = p.accent.into();
    c.link = p.blue.into();
    // 次级 / 悬浮高亮（shadcn 语义 accent 即 hover 底）
    c.secondary = p.raised.into();
    c.secondary_foreground = p.text.into();
    c.secondary_hover = p.hover.into();
    c.secondary_active = p.hover.into();
    c.muted = p.hover.into();
    c.muted_foreground = p.muted.into();
    c.accent = p.accent_weak.into();
    c.accent_foreground = p.text.into();
    // 按钮组 token（kit Button 按变体取用）
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
    // 状态色
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
    // 弹层 / 卡片
    c.popover = p.surface.into();
    c.popover_foreground = p.text.into();
    // 标题栏 / 侧栏
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
    // 列表 / 表格
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
    // 其余控件
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
    // 原色槽位（kit red/blue/green 直配）
    c.red = p.red.into();
    c.red_light = p.red.into();
    c.blue = p.blue.into();
    c.blue_light = p.blue.into();
    c.green = p.accent.into();
    c.green_light = p.accent.into();

    // 旧版 tokens 面（Notification/Dialog 等 legacy 组件直接读 tokens.*，
    // 不随 colors 自动同步——缺了会出现白底白字）
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
