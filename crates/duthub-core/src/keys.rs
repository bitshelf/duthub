//! OS 密钥库封装（Windows Credential Manager；`docs/MODEL_CREDENTIALS.md` D8）。
//!
//! - API Key 存 `duthub/model/<model_id>` 条目；settings.json 只存模型元数据。
//! - 导出/日志/Agent Card 永不含密钥（`export_safe` 断言）。

use crate::{DeviceError, DeviceResult};

fn entry(model_id: &str) -> Result<keyring::Entry, DeviceError> {
    if model_id.trim().is_empty() {
        return Err(DeviceError::Invalid("模型 ID 为空".into()));
    }
    keyring::Entry::new("duthub", &format!("model/{model_id}"))
        .map_err(|e| DeviceError::Invalid(format!("密钥库条目创建失败: {e}")))
}

/// 写入/覆盖模型 API Key。
pub fn set_key(model_id: &str, api_key: &str) -> DeviceResult<()> {
    let entry = entry(model_id)?;
    entry
        .set_password(api_key)
        .map_err(|e| DeviceError::Invalid(format!("密钥写入失败: {e}")))
}

/// 读取模型 API Key；未存储 → Ok(None)。
pub fn get_key(model_id: &str) -> DeviceResult<Option<String>> {
    match entry(model_id)?.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(DeviceError::Invalid(format!("密钥读取失败: {e}"))),
    }
}

/// 删除模型 API Key（模型删除时联动）。
pub fn delete_key(model_id: &str) -> DeviceResult<()> {
    match entry(model_id)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()), // 幂等
        Err(e) => Err(DeviceError::Invalid(format!("密钥删除失败: {e}"))),
    }
}

/// 导出安全断言：任何要落盘/导出的 JSON 不得包含密钥字段。
/// 遍历一层字段名与字符串值做兜底扫描（true=发现疑似密钥）。
pub fn export_contains_secret(json: &serde_json::Value) -> bool {
    let suspicious = ["apikey", "api_key", "secret", "password", "token"];
    match json {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if suspicious
                    .iter()
                    .any(|s| k.to_ascii_lowercase().contains(s))
                {
                    return true;
                }
                if let serde_json::Value::String(vs) = v
                    && suspicious
                        .iter()
                        .any(|s| vs.to_ascii_lowercase().contains(s) && vs.len() > 8)
                {
                    return true;
                }
                if export_contains_secret(v) {
                    return true;
                }
            }
            false
        }
        serde_json::Value::Array(items) => items.iter().any(export_contains_secret),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_scanner_flags_keys() {
        let clean = serde_json::json!({
            "provider": "openai",
            "model": "gpt-4o-mini",
            "baseUrl": "https://api.openai.com/v1"
        });
        assert!(!export_contains_secret(&clean));

        let with_key = serde_json::json!({ "apiKey": "sk-1234567890" });
        assert!(export_contains_secret(&with_key));

        let with_token = serde_json::json!({ "models": [ { "token": "abcdefgh1234" } ] });
        assert!(export_contains_secret(&with_token));
    }

    #[test]
    fn empty_model_id_rejected() {
        assert!(set_key("", "x").is_err());
        assert!(get_key("  ").is_err());
    }

    // 真实密钥库读写集成（Windows Credential Manager；手动运行）
    #[test]
    #[ignore]
    fn credential_roundtrip() {
        set_key("test-model", "sk-test-123").unwrap();
        assert_eq!(
            get_key("test-model").unwrap().as_deref(),
            Some("sk-test-123")
        );
        delete_key("test-model").unwrap();
        assert_eq!(get_key("test-model").unwrap(), None);
    }
}
