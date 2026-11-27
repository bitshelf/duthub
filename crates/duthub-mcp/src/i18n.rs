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

#[cfg(test)]
mod tests {
    use super::*;

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
}
