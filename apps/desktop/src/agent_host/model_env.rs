//! Model binding → goose environment variables (W2).
//!
//! The model configuration is stored in duthub: it utilizes the provider catalog
//! (`settings.agent.providers`). The key is kept in the OS key store at `model/<credential_id>`.
//! Additionally, the legacy field `agent.provider/slow` is also used for storage purposes.
//! Goose is only injected through environment variables; no configuration details related to Goose
//! are written down, and all operations are logged.

use std::fmt;
use std::path::Path;

use duthub_agent::validate_model_base;
use duthub_core::model::SettingsFile;
use duthub_core::{Provider, model_provider_credential_id};

/// The model is bound to goose. `api_key` exists only in memory and environment variables.
#[derive(Clone, PartialEq, Eq)]
pub struct GooseBinding {
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl fmt::Debug for GooseBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GooseBinding")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl GooseBinding {
    /// Masked display values (available in UI/log formats).
    #[must_use]
    pub fn redacted_debug(&self) -> String {
        format!(
            "{} · {} · {}",
            self.provider.label(),
            self.model,
            if self.base_url.is_empty() {
                "<default>"
            } else {
                &self.base_url
            }
        )
    }
}

/// Parsing the current model binding from settings (the actual API key is read from the key
/// repository).
pub fn resolve_binding(settings: &SettingsFile) -> Result<GooseBinding, String> {
    resolve_binding_with(settings, |credential_id| {
        duthub_core::get_key(credential_id).map_err(|error| error.to_string())
    })
}

/// Implementation of parsing methods for querying with keys (for testing/integration purposes).
///
/// F7: The explicitly selected `agent.activeProviderId/activeModelId` must be exactly matched. If
/// not selected, then the system will revert to the legacy preferred order and selection order.
/// Errors related to the key store of a single provider can be ignored and aggregated. The reason
/// for aggregation will only be returned if there are no available bindings at all. No silent
/// switching to another provider will occur in such cases.
pub fn resolve_binding_with<F>(settings: &SettingsFile, lookup: F) -> Result<GooseBinding, String>
where
    F: Fn(&str) -> Result<Option<String>, String>,
{
    let active_provider = non_empty(settings.agent.active_provider_id.as_deref());
    let active_model = non_empty(settings.agent.active_model_id.as_deref());
    if active_provider.is_some() || active_model.is_some() {
        return resolve_active(settings, active_provider, active_model, &lookup);
    }
    resolve_catalog_or_legacy(settings, &lookup)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// F7: Prioritize the providers/models explicitly selected by the user. If any are missing or
/// incomplete, a clear error will be reported (no rollback is performed).
fn resolve_active<F>(
    settings: &SettingsFile,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    lookup: &F,
) -> Result<GooseBinding, String>
where
    F: Fn(&str) -> Result<Option<String>, String>,
{
    let provider_id = provider_id
        .ok_or_else(|| "选中的会话模型缺少 provider id：请在「Agent 模型」中重新选择".to_owned())?;
    let model_id = model_id
        .ok_or_else(|| "选中的会话模型缺少模型名：请在「Agent 模型」中重新选择".to_owned())?;
    let provider = settings
        .agent
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| {
            format!("选中的模型提供方 {provider_id} 不存在：请在「Agent 模型」中重新选择")
        })?;
    if !provider.enabled {
        return Err(format!("选中的模型提供方 {} 已停用", provider.name));
    }
    ensure_protocol_supported(provider.protocol)?;
    let model = provider
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or_else(|| {
            format!(
                "提供方 {} 中不存在选中的模型 {model_id}：请在「Agent 模型」中重新选择",
                provider.name
            )
        })?;
    if !model.enabled {
        return Err(format!(
            "提供方 {} 的模型 {model_id} 已停用：请启用或重新选择",
            provider.name
        ));
    }
    let credential_id =
        model_provider_credential_id(&provider.id, provider.protocol, &provider.base_url);
    let api_key = lookup(&credential_id)
        .map_err(|error| format!("读取提供方 {} 的 API Key 失败: {error}", provider.name))?
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            format!(
                "提供方 {} · 模型 {model_id} 的 API Key 未保存：请重新保存 Token 或选择其他模型",
                provider.name
            )
        })?;
    build_binding(provider.protocol, &provider.base_url, &model.id, api_key)
}

/// Fallback catalog resolution when no active provider/model is pinned.
/// Per-provider keyring errors are recorded so subsequent providers are still tried.
fn resolve_catalog_or_legacy<F>(settings: &SettingsFile, lookup: &F) -> Result<GooseBinding, String>
where
    F: Fn(&str) -> Result<Option<String>, String>,
{
    let candidates: Vec<&duthub_core::model::ModelProvider> = settings
        .agent
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .collect();
    let mut skipped: Vec<String> = Vec::new();
    for provider in candidates {
        if let Err(reason) = ensure_protocol_supported(provider.protocol) {
            skipped.push(format!("{}: {reason}", provider.name));
            continue;
        }
        let Some(model) = first_enabled_model(provider, None) else {
            skipped.push(format!("{}: no enabled models", provider.name));
            continue;
        };
        let credential_id =
            model_provider_credential_id(&provider.id, provider.protocol, &provider.base_url);
        match lookup(&credential_id) {
            Ok(Some(api_key)) if !api_key.trim().is_empty() => {
                return build_binding(
                    provider.protocol,
                    &provider.base_url,
                    &model.id,
                    api_key.trim().to_owned(),
                );
            }
            Ok(_) => skipped.push(format!("{} · {}: API Key missing", provider.name, model.id)),
            Err(error) => skipped.push(format!("{}: keyring error: {error}", provider.name)),
        }
    }

    Err(append_skipped(
        "No configured model provider found; add and enable a provider in Agent Models".to_owned(),
        &skipped,
    ))
}

fn first_enabled_model<'a>(
    provider: &'a duthub_core::model::ModelProvider,
    hint: Option<&str>,
) -> Option<&'a duthub_core::model::ProviderModel> {
    let hint = non_empty(hint);
    if let Some(hint) = hint
        && let Some(found) = provider
            .models
            .iter()
            .find(|model| model.enabled && model.id.trim() == hint)
    {
        return Some(found);
    }
    provider
        .models
        .iter()
        .find(|model| model.enabled && !model.id.trim().is_empty())
}

fn append_skipped(message: String, skipped: &[String]) -> String {
    if skipped.is_empty() {
        message
    } else {
        format!("{message}（已尝试：{}）", skipped.join("；"))
    }
}

/// F19: The product protocol allowlist must be consistent with the API format options of the "Agent
/// model".
///
/// `OpenAI`-compatible services (including DeepSeek/Qwen aliases) and Anthropic are directly
/// supported by `goose_env`. There are no available products for Gemini yet (the UI does not offer
/// any such options). The legacy configuration `agent.provider=gemini` is also explicitly disabled.
/// This avoids the situation where the UI is unavailable, but the legacy configuration can still be
/// used.
#[must_use]
pub fn protocol_supported(provider: Provider) -> bool {
    !matches!(provider, Provider::Gemini)
}

fn ensure_protocol_supported(provider: Provider) -> Result<(), String> {
    if protocol_supported(provider) {
        return Ok(());
    }
    Err(format!(
        "{} 协议未启用：请在「Agent 模型」中选择 OpenAI 兼容或 Anthropic Messages",
        provider.label()
    ))
}

/// F19: Normalization binding before host startup: Trim all fields, and verify the protocol, Base
/// URL, model, and keys.
pub fn normalize_binding(binding: &GooseBinding) -> Result<GooseBinding, String> {
    ensure_protocol_supported(binding.provider)?;
    let base_url = if binding.base_url.trim().is_empty() {
        binding.provider.default_base().to_owned()
    } else {
        validate_model_base(&binding.base_url).map_err(|error| error.to_string())?
    };
    if base_url.trim().is_empty() {
        return Err("OpenAI Compatible provider 必须显式填写 Base URL".into());
    }
    let model = binding.model.trim();
    if model.is_empty() {
        return Err("模型名为空".into());
    }
    let api_key = binding.api_key.trim();
    if api_key.is_empty() {
        return Err("API Key 为空".into());
    }
    Ok(GooseBinding {
        provider: binding.provider,
        base_url,
        model: model.to_owned(),
        api_key: api_key.to_owned(),
    })
}

fn build_binding(
    provider: Provider,
    base_url: &str,
    model: &str,
    api_key: String,
) -> Result<GooseBinding, String> {
    let base_url = if base_url.trim().is_empty() {
        provider.default_base().to_owned()
    } else {
        validate_model_base(base_url).map_err(|error| error.to_string())?
    };
    if base_url.is_empty() {
        return Err("OpenAI Compatible provider 必须显式填写 Base URL".into());
    }
    if model.trim().is_empty() {
        return Err("模型名为空".into());
    }
    Ok(GooseBinding {
        provider,
        base_url,
        model: model.trim().to_owned(),
        api_key,
    })
}

/// The environment variables required to generate "goose" (excluding inherited environments; use a
/// whitelist for integration).
///
/// Key points for path convergence (**not an OS-mandated boundary**; restricted tokens /
/// `AppContainer` are still deferred):
/// - `GOOSE_PATH_ROOT` is fixed to be within each DUT's directory; config/sessions is not located
///   in the user's home directory.
/// - `GOOSE_DISABLE_KEYRING=1`: By default, goose attempts to use the system keyring. In
///   non-interactive sessions, this could lead to infinite blocking issues. The model keys stored in
///   duthub are only injected via environment variables and do not affect goose configuration
///   settings.
pub fn goose_env(
    binding: &GooseBinding,
    goose_root: &Path,
) -> Result<Vec<(String, String)>, String> {
    if !goose_root.is_absolute() {
        return Err(format!(
            "GOOSE_PATH_ROOT 必须是绝对路径: {}",
            goose_root.display()
        ));
    }
    if binding.model.trim().is_empty() {
        return Err("模型名为空".into());
    }
    if binding.api_key.trim().is_empty() {
        return Err("API Key 为空".into());
    }
    let mut env = vec![
        (
            "GOOSE_PROVIDER".to_owned(),
            goose_provider(binding.provider).to_owned(),
        ),
        ("GOOSE_MODEL".to_owned(), binding.model.trim().to_owned()),
        ("GOOSE_DISABLE_KEYRING".to_owned(), "1".to_owned()),
        (
            "GOOSE_PATH_ROOT".to_owned(),
            goose_root.to_string_lossy().into_owned(),
        ),
        // In the "auto" mode, goose directly executes tools without ever sending any permission
        // requests. In the "approve" mode, permission requests for tools are handed over to the ACP
        // client (this client refuses all such requests).
        ("GOOSE_MODE".to_owned(), "approve".to_owned()),
    ];
    let base = normalize_base(binding.provider, &binding.base_url);
    match binding.provider {
        Provider::Openai
        | Provider::OpenaiResponses
        | Provider::Deepseek
        | Provider::Qwen
        | Provider::Compatible => {
            env.push(("OPENAI_API_KEY".to_owned(), binding.api_key.clone()));
            env.push(("OPENAI_BASE_URL".to_owned(), base));
        }
        Provider::Anthropic => {
            env.push(("ANTHROPIC_API_KEY".to_owned(), binding.api_key.clone()));
            env.push(("ANTHROPIC_HOST".to_owned(), base));
        }
        Provider::Gemini => {
            env.push(("GOOGLE_API_KEY".to_owned(), binding.api_key.clone()));
            env.push(("GOOGLE_HOST".to_owned(), base));
        }
    }
    for (key, value) in &env {
        if key.contains('\0') || value.contains('\0') {
            return Err("模型环境变量包含 NUL 字符".into());
        }
    }
    Ok(env)
}

fn goose_provider(provider: Provider) -> &'static str {
    match provider {
        Provider::Openai
        | Provider::OpenaiResponses
        | Provider::Deepseek
        | Provider::Qwen
        | Provider::Compatible => "openai",
        Provider::Anthropic => "anthropic",
        Provider::Gemini => "google",
    }
}

/// Goose manually modifies the version numbers: Anthropic removes the `/v1` suffix, while Google
/// removes the `/v1beta` suffix.
fn normalize_base(provider: Provider, base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    match provider {
        Provider::Anthropic => trimmed
            .strip_suffix("/v1")
            .map_or_else(|| trimmed.to_owned(), str::to_owned),
        Provider::Gemini => trimmed
            .strip_suffix("/v1beta")
            .map_or_else(|| trimmed.to_owned(), str::to_owned),
        _ => trimmed.to_owned(),
    }
}

/// Managed config: disables goose's built-in local extensions.
///
/// Verified against `goose-acp 1.60.2`: writing this section clears the built-in extensions
/// (`Extension Manager`, `summon`, `developer`, `tom`) from `session/new` `_meta.extensionResults`.
/// Universal `goose.exe` 1.60.2 also ships built-in `apps` and `analyze` extensions; leaving them
/// enabled would yield `[duthub, apps, analyze]` and fail `validate_extension_results` (which
/// requires strictly one `duthub` extension). Update this list whenever a newer goose release
/// adds built-in extensions.
#[must_use]
pub fn goose_config_yaml() -> &'static str {
    "# Managed by duthub. Do not edit: rewritten on every session start.\n\
extensions:\n\
\x20\x20developer:\n\
\x20\x20\x20\x20enabled: false\n\
\x20\x20summon:\n\
\x20\x20\x20\x20enabled: false\n\
\x20\x20tom:\n\
\x20\x20\x20\x20enabled: false\n\
\x20\x20extensionmanager:\n\
\x20\x20\x20\x20enabled: false\n\
\x20\x20apps:\n\
\x20\x20\x20\x20enabled: false\n\
\x20\x20analyze:\n\
\x20\x20\x20\x20enabled: false\n"
}

/// Save the managed config in `<goose_root>/config/config.yaml` (do not change the mtime if the
/// content remains unchanged).
pub fn write_config(goose_root: &Path) -> Result<(), String> {
    let dir = duthub_core::store::ensure_agent_dir(goose_root, &["config"])
        .map_err(|error| error.to_string())?;
    let path = dir.join("config.yaml");
    let desired = goose_config_yaml();
    if std::fs::read_to_string(&path).ok().as_deref() == Some(desired) {
        return Ok(());
    }
    std::fs::write(&path, desired).map_err(|error| format!("写入 {} 失败: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::model::{ModelProvider, ProviderModel};

    fn catalog_settings(provider: Provider, base_url: &str) -> SettingsFile {
        let mut settings = duthub_core::store::default_settings();
        settings.agent.providers = vec![ModelProvider {
            id: "p1".into(),
            name: "Provider One".into(),
            base_url: base_url.into(),
            protocol: provider,
            enabled: true,
            models: vec![ProviderModel::new("model-a", true)],
        }];
        settings
    }

    #[test]
    fn catalog_binding_reads_scoped_credential() {
        let settings = catalog_settings(Provider::Openai, "https://api.example.com/v1");
        let credential =
            model_provider_credential_id("p1", Provider::Openai, "https://api.example.com/v1");
        let binding = resolve_binding_with(&settings, |id| {
            assert_eq!(id, credential);
            Ok(Some("sk-test".into()))
        })
        .unwrap();
        assert_eq!(binding.provider, Provider::Openai);
        assert_eq!(binding.model, "model-a");
        assert_eq!(binding.api_key, "sk-test");
    }

    fn two_providers() -> SettingsFile {
        let mut settings = duthub_core::store::default_settings();
        settings.agent.providers = vec![
            ModelProvider {
                id: "a".into(),
                name: "Provider A".into(),
                base_url: "https://a.example.com/v1".into(),
                protocol: Provider::Openai,
                enabled: true,
                models: vec![ProviderModel::new("model-a", true)],
            },
            ModelProvider {
                id: "b".into(),
                name: "Provider B".into(),
                base_url: "https://b.example.com/v1".into(),
                protocol: Provider::Openai,
                enabled: true,
                models: vec![ProviderModel::new("model-b", true)],
            },
        ];
        settings
    }

    fn credential(id: &str, base_url: &str) -> String {
        model_provider_credential_id(id, Provider::Openai, base_url)
    }

    #[test]
    fn active_selection_wins_over_enabled_order() {
        // F7: Once B is selected, it must be used with B's base_url/model/key. You cannot choose A
        // instead, as per the enabling order.
        let mut settings = two_providers();
        settings.agent.active_provider_id = Some("b".into());
        settings.agent.active_model_id = Some("model-b".into());
        let key_a = credential("a", "https://a.example.com/v1");
        let key_b = credential("b", "https://b.example.com/v1");
        let binding = resolve_binding_with(&settings, |id| {
            if id == key_a {
                Ok(Some("sk-a".into()))
            } else if id == key_b {
                Ok(Some("sk-b".into()))
            } else {
                Ok(None)
            }
        })
        .unwrap();
        assert_eq!(binding.model, "model-b");
        assert_eq!(binding.base_url, "https://b.example.com/v1");
        assert_eq!(binding.api_key, "sk-b");
    }

    #[test]
    fn active_selection_missing_key_is_explicit_error_not_fallback() {
        let mut settings = two_providers();
        settings.agent.active_provider_id = Some("b".into());
        settings.agent.active_model_id = Some("model-b".into());
        let key_a = credential("a", "https://a.example.com/v1");
        let error = resolve_binding_with(&settings, |id| {
            if id == key_a {
                Ok(Some("sk-a".into()))
            } else {
                Ok(None)
            }
        })
        .expect_err("If the selected provider lacks a key, it must be changed to option A without any warnings.");
        assert!(error.contains("Provider B"), "{error}");
        assert!(error.contains("API Key 未保存"), "{error}");
    }

    #[test]
    fn provider_keyring_error_is_skipped_and_next_provider_wins() {
        // F7: If there is an error with the key store of a single provider, the subsequent
        // providers should not be interrupted.
        let settings = two_providers();
        let key_b = credential("b", "https://b.example.com/v1");
        let binding = resolve_binding_with(&settings, |id| {
            if id == key_b {
                Ok(Some("sk-b".into()))
            } else {
                Err("keyring unavailable".into())
            }
        })
        .unwrap();
        assert_eq!(binding.model, "model-b");
        assert_eq!(binding.api_key, "sk-b");
    }

    #[test]
    fn keyring_errors_are_aggregated_when_no_binding_is_usable() {
        let settings = two_providers();
        let error = resolve_binding_with(&settings, |_| Err("vault locked".into()))
            .expect_err("An error must be reported when no binding options are available.");
        assert!(error.contains("vault locked"), "{error}");
        assert!(error.contains("Provider A"), "{error}");
        assert!(error.contains("Provider B"), "{error}");
    }

    #[test]
    fn gemini_protocol_is_rejected_with_reason() {
        let mut settings = catalog_settings(Provider::Gemini, "https://gemini.example.com/v1beta");
        settings.agent.providers[0].enabled = true;
        let key = model_provider_credential_id(
            "p1",
            Provider::Gemini,
            "https://gemini.example.com/v1beta",
        );
        let error = resolve_binding_with(&settings, |id| {
            if id == key {
                Ok(Some("sk-gemini".into()))
            } else {
                Ok(None)
            }
        })
        .expect_err("Gemini must be rejected");
        assert!(error.contains("未启用"), "{error}");
    }

    #[test]
    fn normalize_binding_trims_and_rejects_incomplete_values() {
        let binding = normalize_binding(&GooseBinding {
            provider: Provider::Openai,
            base_url: "  https://api.example.com/v1/  ".into(),
            model: "  model-a  ".into(),
            api_key: "  sk-test  ".into(),
        })
        .unwrap();
        assert_eq!(binding.base_url, "https://api.example.com/v1");
        assert_eq!(binding.model, "model-a");
        assert_eq!(binding.api_key, "sk-test");

        for broken in [
            GooseBinding {
                provider: Provider::Openai,
                base_url: "https://api.example.com/v1".into(),
                model: "   ".into(),
                api_key: "sk".into(),
            },
            GooseBinding {
                provider: Provider::Openai,
                base_url: "https://api.example.com/v1".into(),
                model: "m".into(),
                api_key: "  ".into(),
            },
            GooseBinding {
                provider: Provider::Gemini,
                base_url: "https://gemini.example.com".into(),
                model: "m".into(),
                api_key: "sk".into(),
            },
        ] {
            assert!(
                normalize_binding(&broken).is_err(),
                "must reject incomplete or unsupported bindings"
            );
        }
    }

    #[test]
    fn env_disables_keyring_and_roots_paths() {
        let binding = GooseBinding {
            provider: Provider::Openai,
            base_url: "https://api.example.com/v1".into(),
            model: "m".into(),
            api_key: "sk-env-key".into(),
        };
        let root = std::env::temp_dir().join("goose-root-keyring");
        let env = goose_env(&binding, &root).unwrap();
        assert!(
            env.contains(&("GOOSE_DISABLE_KEYRING".into(), "1".into())),
            "GOOSE_DISABLE_KEYRING=1 should be kept enabled (to prevent keyrings from causing infinite blocking issues)."
        );
        assert!(env.contains(&(
            "GOOSE_PATH_ROOT".into(),
            root.to_string_lossy().into_owned()
        )));
        // The model key is only injected into the environment variables, and does not affect
        // configuration/path variables.
        let key_values: Vec<&str> = env
            .iter()
            .filter(|(key, _)| key.contains("KEY"))
            .map(|(_, value)| value.as_str())
            .collect();
        assert!(key_values.contains(&"sk-env-key"));
        assert!(env.iter().all(|(_, value)| !value.contains("Block/goose")));
    }

    #[test]
    fn env_mapping_strips_duplicate_version_segments() {
        let anthropic = GooseBinding {
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            model: "m".into(),
            api_key: "k".into(),
        };
        let root = std::env::temp_dir().join("goose-root");
        let env = goose_env(&anthropic, &root).unwrap();
        assert!(env.contains(&("ANTHROPIC_HOST".into(), "https://api.anthropic.com".into())));
        assert!(env.contains(&("GOOSE_PROVIDER".into(), "anthropic".into())));
        assert!(
            env.contains(&("GOOSE_MODE".into(), "approve".into())),
            "F1: It is necessary to use the \"approve\" command to send permission requests back to the ACP for rejection."
        );

        let google = GooseBinding {
            provider: Provider::Gemini,
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "m".into(),
            api_key: "k".into(),
        };
        let env = goose_env(&google, &root).unwrap();
        assert!(env.contains(&(
            "GOOGLE_HOST".into(),
            "https://generativelanguage.googleapis.com".into()
        )));
    }

    #[test]
    fn env_rejects_relative_goose_root_and_redacts_debug() {
        let binding = GooseBinding {
            provider: Provider::Openai,
            base_url: "https://api.openai.com/v1".into(),
            model: "m".into(),
            api_key: "sk-secret-value".into(),
        };
        assert!(goose_env(&binding, Path::new("relative/root")).is_err());
        let text = format!("{binding:?}");
        assert!(!text.contains("sk-secret-value"));
        assert!(text.contains("[REDACTED]"));
    }

    #[test]
    fn managed_config_disables_all_builtins() {
        let yaml = goose_config_yaml();
        // `apps`/`analyze` ship with the general goose.exe; the lean goose-acp
        // build only had the first four. Both shapes must end up duthub-only.
        for name in [
            "developer",
            "summon",
            "tom",
            "extensionmanager",
            "apps",
            "analyze",
        ] {
            assert!(
                yaml.contains(&format!("  {name}:\n    enabled: false")),
                "{name}"
            );
        }
    }
}
