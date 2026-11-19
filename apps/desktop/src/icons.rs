//! 图标资产（1:1 移植自原型 SVG sprite）+ gpui AssetSource。

use std::borrow::Cow;

use gpui_kit::{Svg, svg};

include!("icons_gen.rs");

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
            .map(|(name, _)| gpui_kit::SharedString::from(format!("icons/{name}.svg")))
            .collect())
    }
}

fn lookup(name: &str) -> Option<&'static str> {
    ICONS.iter().find(|(n, _)| *n == name).map(|(_, doc)| *doc)
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
        assert!(lookup("not-exist").is_none());
        // 每个 svg 都带根 stroke，保证 gpui 以 text_color 着色可见
        for (name, doc) in ICONS {
            assert!(doc.contains("stroke=\"#000\""), "icon {name} 缺少根 stroke");
        }
    }
}
