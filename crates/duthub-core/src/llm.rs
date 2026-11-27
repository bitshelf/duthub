//! LLM provider identifiers and credential scoping.
//!
//! Network HTTP probes live in `duthub-agent::llm` so `duthub-core` remains
//! free of HTTP/TLS runtime dependencies.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Openai,
    #[serde(rename = "openai_responses")]
    OpenaiResponses,
    Anthropic,
    Gemini,
    Deepseek,
    Qwen,
    Compatible,
}

impl Provider {
    #[must_use]
    pub fn from_setting(value: &str) -> Option<Provider> {
        match value {
            "openai" => Some(Provider::Openai),
            "openai_responses" | "openaiResponses" => Some(Provider::OpenaiResponses),
            "anthropic" => Some(Provider::Anthropic),
            "google" | "gemini" => Some(Provider::Gemini),
            "deepseek" => Some(Provider::Deepseek),
            "qwen" => Some(Provider::Qwen),
            "compatible" => Some(Provider::Compatible),
            _ => None,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Provider::Openai => "OpenAI",
            Provider::OpenaiResponses => "OpenAI Responses",
            Provider::Anthropic => "Anthropic",
            Provider::Gemini => "Google Gemini",
            Provider::Deepseek => "DeepSeek",
            Provider::Qwen => "Qwen / DashScope",
            Provider::Compatible => "OpenAI Compatible",
        }
    }

    /// Default API base URL (`Compatible` requires an explicit URL).
    #[must_use]
    pub const fn default_base(self) -> &'static str {
        match self {
            Provider::Openai | Provider::OpenaiResponses => "https://api.openai.com/v1",
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Provider::Deepseek => "https://api.deepseek.com",
            Provider::Qwen => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Provider::Compatible => "",
        }
    }
}

/// Scope a stored provider credential to its API format and destination.
#[must_use]
pub fn model_provider_credential_id(id: &str, provider: Provider, base_url: &str) -> String {
    let destination = format!("{}\n{base_url}", provider.label());
    let fingerprint = Sha256::digest(destination.as_bytes());
    format!("provider/{id}/{fingerprint:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_defaults_and_labels() {
        assert_eq!(Provider::Openai.default_base(), "https://api.openai.com/v1");
        assert_eq!(Provider::from_setting("google"), Some(Provider::Gemini));
        assert_eq!(Provider::Compatible.default_base(), "");
    }

    #[test]
    fn credentials_are_scoped_to_the_provider_destination() {
        let url = "https://example.com/v1";
        let key = model_provider_credential_id("example", Provider::Compatible, url);
        assert_ne!(
            key,
            model_provider_credential_id("other", Provider::Compatible, url)
        );
        assert_ne!(
            key,
            model_provider_credential_id("example", Provider::Anthropic, url)
        );
        assert_ne!(
            key,
            model_provider_credential_id(
                "example",
                Provider::Compatible,
                "https://other.example/v1"
            )
        );
    }
}
