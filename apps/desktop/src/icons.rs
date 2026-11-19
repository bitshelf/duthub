//! 图标资产（1:1 移植自原型 SVG sprite）+ gpui AssetSource。

use std::borrow::Cow;

use gpui_kit::{Svg, svg};

include!("icons_gen.rs");

/// App-specific icons kept outside the generated sprite table so `scripts/gen-icons.pl`
/// does not overwrite them. All use root `stroke="#000"` so GPUI tints them via `text_color`.
const EXTRA_ICONS: &[(&str, &str)] = &[
    (
        "maximize-2",
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M15 3h6v6M21 3l-7 7M3 21l7-7M9 21H3v-6"/></svg>"##,
    ),
    (
        "shrink",
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 10l7-7M20 10h-6V4M3 21l7-7M4 14h6v6"/></svg>"##,
    ),
    (
        "chevron-down",
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>"##,
    ),
    (
        "chevron-right",
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg>"##,
    ),
];

/// `icon("menu")` → gpui svg 元素（颜色由 `.text_color()` 控制，尺寸由 `.size()` 控制）。
pub fn icon(name: &str) -> Svg {
    svg().path(format!("icons/{name}.svg"))
}

/// kit Icon 包装：复用 sprite 资产路径（经全局 AssetSource 解析），可进 kit Button 等容器。
pub fn kit_icon(name: &str) -> gpui_kit::component::Icon {
    gpui_kit::component::Icon::empty().path(format!("icons/{name}.svg"))
}

/// gpui AssetSource：从编译期嵌入的图标表读取（无文件系统依赖）。
pub struct IconAssets;

impl gpui_kit::AssetSource for IconAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        // 首页品牌图（gen-icon.mjs 生成）
        if path == "brand/app-256.png" {
            return Ok(Some(Cow::Borrowed(include_bytes!("../assets/app-256.png"))));
        }
        if path == "brand/app-1024.png" {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/brand-1024.png"
            ))));
        }
        if path == "icons/tab-search.svg" {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/tab-search.svg"
            ))));
        }
        let Some(name) = path
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"))
        else {
            return kit_fallback(path);
        };
        match lookup(name) {
            Some(doc) => Ok(Some(Cow::Borrowed(doc.as_bytes()))),
            // kit 内嵌 Lucide 图标回退（TitleBar 窗口控制钮等 kit 自用图标）
            None => kit_fallback(path),
        }
    }

    fn list(&self, _path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        Ok(ICONS
            .iter()
            .chain(EXTRA_ICONS.iter())
            .map(|(name, _)| gpui_kit::SharedString::from(format!("icons/{name}.svg")))
            .chain(std::iter::once("icons/tab-search.svg".into()))
            .collect())
    }
}

fn lookup(name: &str) -> Option<&'static str> {
    EXTRA_ICONS
        .iter()
        .chain(ICONS.iter())
        .find(|(n, _)| *n == name)
        .map(|(_, doc)| *doc)
}

/// kit 内嵌资产回退（gpui-kit-assets 的 Lucide 全集）。
fn kit_fallback(path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
    gpui_kit::AssetSource::load(&gpui_kit::assets::Assets, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_sprite_icons_resolve() {
        assert!(ICONS.len() >= 40);
        assert!(lookup("menu").is_some());
        assert!(lookup("spark").is_some());
        assert!(lookup("panel-right").is_some());
        assert!(lookup("maximize-2").is_some());
        assert!(lookup("shrink").is_some());
        assert!(lookup("chevron-down").is_some());
        assert!(lookup("chevron-right").is_some());
        assert!(lookup("not-exist").is_none());
        // Every SVG carries a root stroke so GPUI can tint it via text_color.
        for (name, doc) in ICONS.iter().chain(EXTRA_ICONS.iter()) {
            assert!(doc.contains("stroke=\"#000\""), "icon {name} 缺少根 stroke");
        }
    }
}
