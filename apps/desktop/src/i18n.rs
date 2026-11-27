//! Desktop message catalogs keyed by stable IDs. Device output and user values are opaque.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        .map_or("(i18n missing)", String::as_str)
}

pub fn label(key: &str) -> &'static str {
    t(Lang(ACTIVE.load(Ordering::Relaxed)), key)
}

pub fn message(key: &str, arguments: &[(&str, &str)]) -> String {
    static PLACEHOLDER: OnceLock<regex::Regex> = OnceLock::new();
    let pattern =
        PLACEHOLDER.get_or_init(|| regex::Regex::new(r"\{([A-Za-z][A-Za-z0-9_]*)\}").unwrap());
    pattern
        .replace_all(label(key), |matched: &regex::Captures<'_>| {
            arguments
                .iter()
                .find(|(name, _)| *name == &matched[1])
                .map_or_else(|| matched[0].to_owned(), |(_, value)| (*value).to_owned())
        })
        .into_owned()
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
