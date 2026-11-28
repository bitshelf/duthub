//! Desktop message catalogs keyed by stable IDs. Device output and user values are opaque.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use duthub_mcp::i18n::{self as mcp_i18n, BlockedNoticeKind, McpLocale};

struct Locale {
    code: &'static str,
    name: &'static str,
    kit: &'static str,
    source: &'static str,
    catalog: OnceLock<HashMap<String, String>>,
}

impl Locale {
    const fn new(
        code: &'static str,
        name: &'static str,
        kit: &'static str,
        source: &'static str,
    ) -> Self {
        Self {
            code,
            name,
            kit,
            source,
            catalog: OnceLock::new(),
        }
    }

    fn messages(&self) -> &HashMap<String, String> {
        self.catalog
            .get_or_init(|| serde_json::from_str(self.source).expect("valid locale resource"))
    }
}

static CHINESE: Locale = Locale::new(
    "zh-CN",
    "简体中文",
    "zh-CN",
    include_str!("../locales/zh-CN.json"),
);
static ENGLISH: Locale = Locale::new(
    "en-US",
    "English",
    "en",
    include_str!("../locales/en-US.json"),
);
static LOCALES: &[&Locale] = &[&CHINESE, &ENGLISH];
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lang(usize);

impl Lang {
    #[cfg(test)]
    #[expect(
        non_upper_case_globals,
        reason = "matches locale enum-variant naming used across i18n unit tests"
    )]
    const ZhCn: Self = Self(0);
    #[cfg(test)]
    #[expect(
        non_upper_case_globals,
        reason = "matches locale enum-variant naming used across i18n unit tests"
    )]
    const EnUs: Self = Self(1);

    pub fn from_setting(value: &str) -> Lang {
        let value = value.trim();
        let system = (value.is_empty() || value == "system")
            .then(sys_locale::get_locale)
            .flatten();
        let value = system.as_deref().unwrap_or(value);
        let requested = value.split(['-', '_']).next().unwrap_or_default();
        Self(
            LOCALES
                .iter()
                .position(|locale| locale.code.eq_ignore_ascii_case(value))
                .or_else(|| {
                    LOCALES.iter().position(|locale| {
                        locale
                            .code
                            .split('-')
                            .next()
                            .is_some_and(|language| language.eq_ignore_ascii_case(requested))
                    })
                })
                .unwrap_or(1),
        )
    }

    pub fn activate(self) {
        ACTIVE.store(self.0, Ordering::Relaxed);
        gpui_kit::component::set_locale(LOCALES[self.0].kit);
    }

    pub fn usb_debug_title(self) -> &'static str {
        t(self, "usbDebugTitle")
    }
}

pub fn language_options(lang: Lang) -> Vec<(&'static str, &'static str)> {
    std::iter::once(("system", t(lang, "systemLanguage")))
        .chain(LOCALES.iter().map(|locale| (locale.code, locale.name)))
        .collect()
}

/// Use a stable translation ID with the English catalog as fallback.
pub fn t(lang: Lang, key: &str) -> &'static str {
    LOCALES[lang.0]
        .messages()
        .get(key)
        .filter(|text| !text.is_empty())
        .or_else(|| LOCALES[1].messages().get(key))
        .or_else(|| LOCALES[0].messages().get(key))
        .map(String::as_str)
        .or_else(|| {
            let locale = McpLocale::from_setting(LOCALES[lang.0].code);
            mcp_i18n::blocked_notice_template(locale, key)
                .or_else(|| mcp_i18n::blocked_notice_template(McpLocale::EnUs, key))
        })
        .unwrap_or("(i18n missing)")
}

pub fn label(key: &str) -> &'static str {
    t(Lang(ACTIVE.load(Ordering::Relaxed)), key)
}

/// Return the BCP-47 locale code (`"zh-CN"` or `"en-US"`) of the currently active desktop language.
pub fn active_code() -> &'static str {
    LOCALES[ACTIVE.load(Ordering::Relaxed)].code
}

pub fn message(key: &str, arguments: &[(&str, &str)]) -> String {
    mcp_i18n::format_message(label(key), arguments)
}

pub fn blocked_notice_texts(kind: BlockedNoticeKind<'_>) -> (&'static str, String, &'static str) {
    let lang = Lang(ACTIVE.load(Ordering::Relaxed));
    mcp_i18n::blocked_notice_texts_with(kind, |key| t(lang, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_size_and_lookup() {
        assert!(LOCALES[0].messages().len() >= 90);
        assert_eq!(t(Lang::ZhCn, "connectionTitle"), "连接管理");
        assert!(
            t(Lang::EnUs, "connectionTitle")
                .to_lowercase()
                .contains("connection")
        );
        assert_eq!(t(Lang::ZhCn, "missing-key"), "(i18n missing)");
    }

    #[test]
    fn usb_debug_label_uses_only_the_selected_language() {
        assert_eq!(Lang::ZhCn.usb_debug_title(), "USB 调试");
        assert_eq!(Lang::EnUs.usb_debug_title(), "USB Debug");
    }

    #[test]
    fn blocked_notice_catalog_matches_desktop_control_labels() {
        for lang in [Lang::ZhCn, Lang::EnUs] {
            for (detail, button) in [
                ("mcpBlockedGuiHeldDetail", "leaseActionRelease"),
                ("mcpBlockedManualTakeoverDetail", "releaseManualControl"),
                ("mcpBlockedNeedsInspectionDetail", "leaseActionRecover"),
                ("mcpBlockedUnauthorizedDetail", "mcpServiceTitle"),
                ("mcpBlockedUnauthorizedDetail", "connectionTitle"),
            ] {
                assert!(t(lang, detail).contains(t(lang, button)), "{detail}");
            }
        }
    }

    #[test]
    fn shipped_catalogs_cover_the_same_keys() {
        let expected = LOCALES[1].messages();
        for locale in LOCALES {
            assert_eq!(locale.messages().len(), expected.len(), "{}", locale.code);
            for key in expected.keys() {
                assert!(
                    locale
                        .messages()
                        .get(key)
                        .is_some_and(|value| !value.is_empty()),
                    "{}: {key}",
                    locale.code
                );
            }
        }
        assert_eq!(Lang::from_setting("en_GB"), Lang::EnUs);
        assert_eq!(Lang::from_setting("zh-Hans"), Lang::ZhCn);
    }
}
