//! 模型绑定 → goose 环境变量（W2）。
//!
//! 模型配置留在 duthub：复用 provider catalog（`settings.agent.providers`，
//! 密钥在 OS 密钥库 `model/<credential id>`）与 legacy `agent.provider/slow` 字段。
//! 只经环境变量注入 goose；Token 不写 goose 配置、不进日志。

use std::fmt;
use std::path::Path;

use duthub_agent::validate_model_base;
use duthub_core::model::SettingsFile;
use duthub_core::{Provider, model_provider_credential_id};

/// 传给 goose 的模型绑定。`api_key` 只存在于内存与环境变量。
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
    /// 脱敏后的显示值（UI/日志可用）。
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

/// 从 settings 解析当前模型绑定（密钥库读取真实 API Key）。
pub fn resolve_binding(settings: &SettingsFile) -> Result<GooseBinding, String> {
    resolve_binding_with(settings, |credential_id| {
        duthub_core::get_key(credential_id).map_err(|error| error.to_string())
    })
}

/// 可注入密钥查询的解析实现（测试 / 嵌入方）。
///
/// F7：显式选中的 `agent.activeProviderId/activeModelId` 必须精确命中，未选择时
/// 才按 legacy preferred + 启用顺序回退。单个 provider 的密钥库错误跳过并聚合，
/// 只有完全没有可用绑定时才把聚合原因返回，不静默改选其他 provider。
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

/// F7：用户显式选中的 provider/model 优先，缺失或不完整时给明确错误（不回退）。
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

/// F19：产品协议白名单与「Agent 模型」的 API 格式选项保持一致。
///
/// OpenAI 兼容族（含 DeepSeek/Qwen 别名）与 Anthropic 由 `goose_env` 直接支持；
/// Gemini 尚无产品接线（UI 不提供），legacy `agent.provider=gemini` 也显式拒绝，
/// 避免「UI 禁用但 legacy 可进入」。
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

/// F19：Host 启动前归一化绑定：trim 所有字段，并校验协议、Base URL、模型与密钥。
pub fn normalize_binding(binding: GooseBinding) -> Result<GooseBinding, String> {
    ensure_protocol_supported(binding.provider)?;
    let base_url = if binding.base_url.trim().is_empty() {
        binding.provider.default_base().to_owned()
    } else {
        validate_model_base(&binding.base_url)?
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
        validate_model_base(base_url)?
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

/// 生成 goose 需要的环境变量（不含继承环境；调用方用白名单叠加）。
///
/// 路径收敛要点（**不是 OS 强制边界**；受限令牌 / AppContainer 仍 deferred）：
/// - `GOOSE_PATH_ROOT` 固定到每 DUT 目录，config/sessions 不落用户主目录；
/// - `GOOSE_DISABLE_KEYRING=1`：goose 默认会尝试系统 keyring，在无交互会话中
///   可能无限阻塞；duthub 的模型密钥只经环境变量注入，不落 goose 配置。
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
        // F1：auto 模式下 goose 直接执行工具、永远不会发权限请求；
        // approve 模式把 tool 权限请求交给 ACP 客户端（本客户端一律拒绝）。
        ("GOOSE_MODE".to_owned(), "approve".to_owned()),
    ];
    let base = normalize_base(binding.provider, &binding.base_url);
    match binding.provider {
        Provider::Openai | Provider::Deepseek | Provider::Qwen | Provider::Compatible => {
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
        Provider::Openai | Provider::Deepseek | Provider::Qwen | Provider::Compatible => "openai",
        Provider::Anthropic => "anthropic",
        Provider::Gemini => "google",
    }
}

/// goose 自己拼版本段：Anthropic 去掉尾部 `/v1`，Google 去掉 `/v1beta`。
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

/// Managed config：只禁用 goose 内置本地扩展。
///
/// 2026-11-21 对真实 `goose-acp 1.60.2` 实测：写入本段后 `session/new` 的
/// `_meta.extensionResults` 从 4 项（Extension Manager / summon / developer / tom）
/// 变为 `[]`。
///
/// 2026-11-25 用**通用 `goose.exe` 1.60.2** 复核：它还带 `apps` 与 `analyze` 两个
/// 内置扩展，未关闭时工具面是 `[duthub, apps, analyze]`，会被
/// `validate_extension_results`（要求恰好一个 duthub）按 fail-closed 拒绝，表现为
/// 会话停在 Failed。工具面校验不放松，这里把后来新增的内置扩展一并关掉；goose
/// 再新增内置扩展时应同步本表与校验测试（AGENTS #62）。
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

/// 把 managed config 落到 `<goose_root>/config/config.yaml`（内容不变时不动 mtime）。
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
            models: vec![ProviderModel {
                id: "model-a".into(),
                enabled: true,
            }],
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
                models: vec![ProviderModel {
                    id: "model-a".into(),
                    enabled: true,
                }],
            },
            ModelProvider {
                id: "b".into(),
                name: "Provider B".into(),
                base_url: "https://b.example.com/v1".into(),
                protocol: Provider::Openai,
                enabled: true,
                models: vec![ProviderModel {
                    id: "model-b".into(),
                    enabled: true,
                }],
            },
        ];
        settings
    }

    fn credential(id: &str, base_url: &str) -> String {
        model_provider_credential_id(id, Provider::Openai, base_url)
    }

    #[test]
    fn active_selection_wins_over_enabled_order() {
        // F7：选中 B 就必须用 B 的 base_url/model/key，不能按启用顺序选 A。
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
        .expect_err("选中的 provider 缺 key 时不得静默改选 A");
        assert!(error.contains("Provider B"), "{error}");
        assert!(error.contains("API Key 未保存"), "{error}");
    }

    #[test]
    fn provider_keyring_error_is_skipped_and_next_provider_wins() {
        // F7：单个 provider 的密钥库错误不得中断后续 provider。
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
            .expect_err("没有可用绑定时必须报错");
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
        let binding = normalize_binding(GooseBinding {
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
                normalize_binding(broken).is_err(),
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
            "GOOSE_DISABLE_KEYRING=1 必须保留（避免 keyring 无限阻塞）"
        );
        assert!(env.contains(&(
            "GOOSE_PATH_ROOT".into(),
            root.to_string_lossy().into_owned()
        )));
        // 模型密钥只经环境变量注入，不进入配置/路径变量。
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
            "F1：必须用 approve 让权限请求回落到 ACP 拒绝"
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
