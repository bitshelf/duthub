//! Bridge between the `notify_user` MCP tool and the host UI.
//!
//! The MCP crate owns validation and the response contract; the host owns
//! presentation and rate limiting. A notifier must answer with the queueing
//! result it actually observed — `Queued` means "accepted into the pending
//! alert queue", never "the user has seen it".

/// Severity accepted by the `notify_user` tool.
///
/// This is a domain type without serde: the tool takes `Option<String>` and
/// maps it explicitly, so an unknown literal becomes `invalid_params` instead
/// of silently degrading to `info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyLevel {
    Info,
    Warning,
    Error,
}

impl NotifyLevel {
    /// Parse the wire value. Only these three literals are accepted.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One validated notification handed to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyRequest {
    pub message: String,
    pub title: Option<String>,
    pub level: NotifyLevel,
    /// `None` = the caller did not target a DUT (a global notice).
    pub dut_id: Option<String>,
    /// Rate-limit key: the authenticated credential id; `None` for LocalAdmin
    /// (stdio) where no per-credential bucket exists.
    pub credential_id: Option<String>,
}

/// What the host did with a notification. Only [`NotifyOutcome::Queued`] means
/// the notice entered the presentation queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    Queued,
    Throttled(ThrottleReason),
    /// Suppressed by a DUT or source suppression rule.
    Suppressed,
    /// The per-DUT alerts switch is off, or this endpoint has no notifier.
    Disabled,
}

impl NotifyOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Throttled(_) => "throttled",
            Self::Suppressed => "suppressed",
            Self::Disabled => "disabled",
        }
    }
}

/// Why a notification was throttled; carried on the wire as `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleReason {
    /// Duplicate inside the dedup window.
    Dedup,
    /// The per-source pending queue is full.
    Capacity,
    /// Token bucket empty (per-credential or global).
    Rate,
}

impl ThrottleReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dedup => "dedup",
            Self::Capacity => "capacity",
            Self::Rate => "rate",
        }
    }
}

/// Host side of `notify_user`. Implemented by the desktop runtime; the MCP
/// crate never depends on the desktop crate.
pub trait UserNotifier: Send + Sync {
    fn notify(&self, request: &NotifyRequest) -> NotifyOutcome;
}

/// Maximum message length in characters (after control characters are removed).
pub const MESSAGE_MAX_CHARS: usize = 200;
/// Maximum title length in characters.
pub const TITLE_MAX_CHARS: usize = 48;
/// Source key the host must attach to these alerts (`AlertSource::AgentNotice`).
pub const AGENT_NOTICE_SOURCE: &str = "agentNotice";

/// Strip control characters, trim the ends and cap the length in characters.
///
/// Lengths are counted in characters so multi-byte text is never cut inside a
/// code point.
pub fn sanitize_notify_text(raw: &str, max_chars: usize) -> String {
    let stripped: String = raw.chars().filter(|ch| !ch.is_control()).collect();
    stripped.trim().chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_accepts_only_the_three_literals() {
        assert_eq!(NotifyLevel::parse("info"), Some(NotifyLevel::Info));
        assert_eq!(NotifyLevel::parse("warning"), Some(NotifyLevel::Warning));
        assert_eq!(NotifyLevel::parse("error"), Some(NotifyLevel::Error));
        for rejected in ["critical", "INFO", "warn", "", " info"] {
            assert_eq!(NotifyLevel::parse(rejected), None, "{rejected:?}");
        }
        assert_eq!(NotifyLevel::parse("warning").unwrap().as_str(), "warning");
    }

    #[test]
    fn outcome_and_throttle_strings_match_the_contract() {
        assert_eq!(NotifyOutcome::Queued.as_str(), "queued");
        assert_eq!(NotifyOutcome::Suppressed.as_str(), "suppressed");
        assert_eq!(NotifyOutcome::Disabled.as_str(), "disabled");
        assert_eq!(
            NotifyOutcome::Throttled(ThrottleReason::Dedup).as_str(),
            "throttled"
        );
        assert_eq!(ThrottleReason::Dedup.as_str(), "dedup");
        assert_eq!(ThrottleReason::Capacity.as_str(), "capacity");
        assert_eq!(ThrottleReason::Rate.as_str(), "rate");
    }

    #[test]
    fn sanitize_strips_controls_trims_and_counts_characters() {
        assert_eq!(sanitize_notify_text("  hi\u{0}\nthere ", 200), "hithere");
        assert_eq!(sanitize_notify_text("   ", 200), "");
        // Chinese characters count as one each: five fit, the sixth is cut.
        assert_eq!(sanitize_notify_text("测试消息内容超", 5), "测试消息内");
        // The cap is applied after trimming, so a padded value keeps its text.
        assert_eq!(sanitize_notify_text("  abcdef  ", 3), "abc");
    }
}
