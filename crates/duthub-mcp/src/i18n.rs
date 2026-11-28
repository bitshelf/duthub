//! Bilingual (`zh-CN` / `en-US`) message helpers for MCP output that follows
//! duthub's active desktop display language.

/// Active display locale for localized MCP output (follows duthub's active UI language).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpLocale {
    #[default]
    ZhCn,
    EnUs,
}

impl McpLocale {
    /// Parse a duthub language setting or BCP-47 tag (`"zh-CN"`, `"en-US"`, `"en_GB"`, etc.).
    #[must_use]
    pub fn from_setting(value: &str) -> Self {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Self::ZhCn;
        }
        let primary = trimmed.split(['-', '_']).next().unwrap_or_default();
        if primary.eq_ignore_ascii_case("zh") {
            Self::ZhCn
        } else {
            Self::EnUs
        }
    }

    /// Return the canonical BCP-47 locale code (`"zh-CN"` or `"en-US"`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }
}

/// Placeholder shown in exported `SKILL.md` when no bound HTTP endpoint is known.
#[must_use]
pub const fn skill_unbound_endpoint(locale: McpLocale) -> &'static str {
    match locale {
        McpLocale::ZhCn => "未提供已绑定的 HTTP 地址，请向管理员确认。",
        McpLocale::EnUs => {
            "No bound HTTP endpoint is advertised; confirm the URL with the operator."
        }
    }
}

/// Scope footer appended to `skill_export` for a read-only DUT-bound listener.
#[must_use]
pub fn skill_bound_dut_readonly(locale: McpLocale, bound_dut: &str) -> String {
    match locale {
        McpLocale::ZhCn => format!(
            "\n> 本端点仅可读 DUT `{bound_dut}`（仅开放只读权限 `read`，不开放写工具）；跨 DUT 请求返回 `LeaseRequired/UNAUTHORIZED`。"
        ),
        McpLocale::EnUs => format!(
            "\n> This endpoint has read-only access to DUT `{bound_dut}` (`read` grant only; write tools are disabled). Cross-DUT requests return `LeaseRequired/UNAUTHORIZED`."
        ),
    }
}

/// Scope footer appended to `skill_export` for a DUT-bound listener with explicit grants.
#[must_use]
pub fn skill_bound_dut_grants(locale: McpLocale, bound_dut: &str, granted: &[&str]) -> String {
    let grants = granted.join("`, `");
    match locale {
        McpLocale::ZhCn => format!(
            "\n> 本端点限定于 DUT `{bound_dut}`（当前凭据已授予权限：`{grants}`）；跨 DUT 请求返回 `LeaseRequired/UNAUTHORIZED`。"
        ),
        McpLocale::EnUs => format!(
            "\n> This endpoint is scoped to DUT `{bound_dut}` (granted permissions for this credential: `{grants}`). Cross-DUT requests return `LeaseRequired/UNAUTHORIZED`."
        ),
    }
}

/// Scope caveat for `notify_user` in MCP `initialize.instructions`.
#[must_use]
pub const fn notify_user_scope_note(locale: McpLocale) -> &'static str {
    match locale {
        McpLocale::ZhCn => "（仅用户配置的 MCP 监听器提供，桌面发起的 Agent 会话没有该工具）",
        McpLocale::EnUs => {
            " (available only on user-configured MCP listeners; desktop-launched Agent sessions do not expose this tool)"
        }
    }
}

/// Structured category for blocked-access notices emitted by `attach_blocked_notice`.
#[derive(Debug, Clone, Copy)]
pub enum BlockedNoticeKind<'a> {
    Unauthorized {
        requester: &'a str,
        action: &'a str,
        dut: &'a str,
    },
    ManualTakeover {
        requester: &'a str,
        action: &'a str,
        dut: &'a str,
    },
    NeedsInspection {
        requester: &'a str,
        action: &'a str,
        dut: &'a str,
    },
    Protected {
        requester: &'a str,
        action: &'a str,
        dut: &'a str,
        op: &'a str,
    },
    GuiHeld {
        requester: &'a str,
        action: &'a str,
        dut: &'a str,
    },
    MultiAgent {
        requester: &'a str,
        action: &'a str,
        dut: &'a str,
        holder: &'a str,
        waited_ms: u64,
        queue_len: usize,
    },
    Generic {
        requester: &'a str,
        action: &'a str,
        dut: &'a str,
        error_msg: &'a str,
    },
}

type Catalog = std::collections::HashMap<String, String>;

/// Look up a shared MCP message template for an explicit display locale.
#[must_use]
pub fn blocked_notice_template(locale: McpLocale, key: &str) -> Option<&'static str> {
    blocked_notice_catalog(locale).get(key).map(String::as_str)
}

fn blocked_notice_catalog(locale: McpLocale) -> &'static Catalog {
    use std::sync::OnceLock;

    static ZH: OnceLock<Catalog> = OnceLock::new();
    static EN: OnceLock<Catalog> = OnceLock::new();
    let (catalog, source) = match locale {
        McpLocale::ZhCn => (&ZH, include_str!("../locales/zh-CN.json")),
        McpLocale::EnUs => (&EN, include_str!("../locales/en-US.json")),
    };
    catalog.get_or_init(|| serde_json::from_str(source).expect("valid MCP locale resource"))
}

/// Substitute named placeholders in one pass, preserving opaque argument values.
#[must_use]
pub fn format_message(template: &str, arguments: &[(&str, &str)]) -> String {
    use std::sync::OnceLock;

    static PLACEHOLDER: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PLACEHOLDER.get_or_init(|| {
        regex::Regex::new(r"\{([A-Za-z][A-Za-z0-9_]*)\}").expect("valid placeholder pattern")
    });
    pattern
        .replace_all(template, |matched: &regex::Captures<'_>| {
            arguments
                .iter()
                .find(|(name, _)| *name == &matched[1])
                .map_or_else(|| matched[0].to_owned(), |(_, value)| (*value).to_owned())
        })
        .into_owned()
}

/// Return localized `(title, detail, suggested_action)` for a blocked DUT access event.
#[must_use]
pub fn blocked_notice_texts(
    locale: McpLocale,
    kind: BlockedNoticeKind<'_>,
) -> (&'static str, String, &'static str) {
    blocked_notice_texts_with(kind, |key| {
        blocked_notice_template(locale, key)
            .or_else(|| blocked_notice_template(McpLocale::EnUs, key))
            .unwrap_or("(i18n missing)")
    })
}

/// Render blocked DUT access text using the host's active catalog lookup.
#[must_use]
pub fn blocked_notice_texts_with(
    kind: BlockedNoticeKind<'_>,
    label: impl Fn(&str) -> &'static str,
) -> (&'static str, String, &'static str) {
    let render = |key, requester, action, dut, extra: &[(&str, &str)]| {
        let mut arguments = vec![("requester", requester), ("action", action), ("dut", dut)];
        arguments.extend_from_slice(extra);
        (
            label(&format!("{key}Title")),
            format_message(label(&format!("{key}Detail")), &arguments),
            label(&format!("{key}Action")),
        )
    };
    match kind {
        BlockedNoticeKind::Unauthorized {
            requester,
            action,
            dut,
        } => render("mcpBlockedUnauthorized", requester, action, dut, &[]),
        BlockedNoticeKind::ManualTakeover {
            requester,
            action,
            dut,
        } => render("mcpBlockedManualTakeover", requester, action, dut, &[]),
        BlockedNoticeKind::NeedsInspection {
            requester,
            action,
            dut,
        } => render("mcpBlockedNeedsInspection", requester, action, dut, &[]),
        BlockedNoticeKind::Protected {
            requester,
            action,
            dut,
            op,
        } => render("mcpBlockedProtected", requester, action, dut, &[("op", op)]),
        BlockedNoticeKind::GuiHeld {
            requester,
            action,
            dut,
        } => render("mcpBlockedGuiHeld", requester, action, dut, &[]),
        BlockedNoticeKind::MultiAgent {
            requester,
            action,
            dut,
            holder,
            waited_ms,
            queue_len,
        } => render(
            "mcpBlockedMultiAgent",
            requester,
            action,
            dut,
            &[
                ("holder", holder),
                ("waited_ms", &waited_ms.to_string()),
                ("queue_len", &queue_len.to_string()),
            ],
        ),
        BlockedNoticeKind::Generic {
            requester,
            action,
            dut,
            error_msg,
        } => render(
            "mcpBlockedGeneric",
            requester,
            action,
            dut,
            &[("error_msg", error_msg)],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_notice_catalogs_have_matching_keys_and_placeholders() {
        fn placeholders(text: &str) -> Vec<&str> {
            let mut names: Vec<_> = text
                .split('{')
                .skip(1)
                .map(|part| part.split_once('}').expect("closed placeholder").0)
                .collect();
            names.sort_unstable();
            names
        }

        let zh = blocked_notice_catalog(McpLocale::ZhCn);
        let en = blocked_notice_catalog(McpLocale::EnUs);
        assert_eq!(zh.len(), en.len());
        for (key, zh_text) in zh {
            let en_text = &en[key];
            assert!(!zh_text.is_empty() && !en_text.is_empty(), "{key}");
            assert_eq!(placeholders(zh_text), placeholders(en_text), "{key}");
        }
    }

    #[test]
    fn standalone_snapshot_uses_default_notice_language() {
        use crate::state::McpState;

        let state = crate::snapshot::SnapshotState::empty();
        let (title, detail, action) = state.blocked_notice_texts(BlockedNoticeKind::GuiHeld {
            requester: "agent-{action}",
            action: "adb_exec",
            dut: "dut-1",
        });
        assert_eq!(title, "需要人工介入：桌面端持有控制权");
        assert!(detail.contains("Agent「agent-{action}」"), "{detail}");
        assert!(detail.contains("释放控制权"), "{detail}");
        assert!(action.contains("释放控制权"), "{action}");
    }

    #[test]
    fn locale_parsing_handles_zh_and_en_variants() {
        assert_eq!(McpLocale::from_setting("zh-CN"), McpLocale::ZhCn);
        assert_eq!(McpLocale::from_setting("zh-Hans"), McpLocale::ZhCn);
        assert_eq!(McpLocale::from_setting("en-US"), McpLocale::EnUs);
        assert_eq!(McpLocale::from_setting("en_GB"), McpLocale::EnUs);
        assert_eq!(McpLocale::ZhCn.code(), "zh-CN");
        assert_eq!(McpLocale::EnUs.code(), "en-US");
    }

    #[test]
    fn skill_scope_notes_render_both_zh_and_en() {
        let zh_readonly = skill_bound_dut_readonly(McpLocale::ZhCn, "dut-1");
        let en_readonly = skill_bound_dut_readonly(McpLocale::EnUs, "dut-1");
        assert!(zh_readonly.contains("仅可读 DUT `dut-1`"), "{zh_readonly}");
        assert!(
            en_readonly.contains("read-only access to DUT `dut-1`"),
            "{en_readonly}"
        );

        let zh_grants = skill_bound_dut_grants(McpLocale::ZhCn, "dut-2", &["read", "flash"]);
        let en_grants = skill_bound_dut_grants(McpLocale::EnUs, "dut-2", &["read", "flash"]);
        assert!(zh_grants.contains("`read`, `flash`"), "{zh_grants}");
        assert!(en_grants.contains("`read`, `flash`"), "{en_grants}");
        assert!(en_grants.contains("scoped to DUT `dut-2`"), "{en_grants}");
    }

    #[test]
    fn blocked_notice_texts_follow_active_locale() {
        let (zh_title, zh_detail, _) = blocked_notice_texts(
            McpLocale::ZhCn,
            BlockedNoticeKind::ManualTakeover {
                requester: "Agent-A",
                action: "flash_update",
                dut: "DUT-2",
            },
        );
        let (en_title, en_detail, _) = blocked_notice_texts(
            McpLocale::EnUs,
            BlockedNoticeKind::ManualTakeover {
                requester: "Agent-A",
                action: "flash_update",
                dut: "DUT-2",
            },
        );
        assert!(zh_title.contains("人工接管"), "{zh_title}");
        assert!(zh_detail.contains("DUT-2"), "{zh_detail}");
        assert!(en_title.contains("Manual takeover"), "{en_title}");
        assert!(en_detail.contains("DUT-2"), "{en_detail}");
    }
}
