//! OS credential store wrapper (Windows Credential Manager).
//!
//! - Model API keys live under `duthub/model/<model_id>`.
//! - Per-DUT A2A tokens live under `duthub/a2a/<dut_id>`.
//! - MCP Bearer tokens live under `duthub/mcp/<cred_id>`.
//! - Exports, logs, and Agent Cards must never contain raw secrets (`export_contains_secret`).

use crate::{DeviceError, DeviceResult};

const SERVICE_NAME: &str = "duthub";

pub(crate) fn get_raw_entry(entry: &str) -> Result<Option<String>, String> {
    match keyring::Entry::new(SERVICE_NAME, entry) {
        Ok(entry) => match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        },
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn set_raw_entry(entry: &str, secret: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE_NAME, entry).map_err(|error| error.to_string())?;
    entry
        .set_password(secret)
        .map_err(|error| error.to_string())
}

pub(crate) fn delete_raw_entry(entry: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE_NAME, entry).map_err(|error| error.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn scoped_entry(namespace: &str, id: &str, label: &str) -> Result<keyring::Entry, DeviceError> {
    if id.trim().is_empty() {
        return Err(DeviceError::Invalid(format!("{label} ID is empty")));
    }
    keyring::Entry::new(SERVICE_NAME, &format!("{namespace}/{id}"))
        .map_err(|e| DeviceError::Invalid(format!("failed to create {label} keyring entry: {e}")))
}

/// Store or overwrite a secret under `duthub/<namespace>/<id>`.
pub fn set_secret(namespace: &str, id: &str, label: &str, secret: &str) -> DeviceResult<()> {
    scoped_entry(namespace, id, label)?
        .set_password(secret)
        .map_err(|e| DeviceError::Invalid(format!("failed to store {label} secret: {e}")))
}

/// Read a secret under `duthub/<namespace>/<id>`; missing entry yields `Ok(None)`.
pub fn get_secret(namespace: &str, id: &str, label: &str) -> DeviceResult<Option<String>> {
    match scoped_entry(namespace, id, label)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(DeviceError::Invalid(format!(
            "failed to read {label} secret: {e}"
        ))),
    }
}

/// Delete a secret under `duthub/<namespace>/<id>` (idempotent).
pub fn delete_secret(namespace: &str, id: &str, label: &str) -> DeviceResult<()> {
    match scoped_entry(namespace, id, label)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(DeviceError::Invalid(format!(
            "failed to delete {label} secret: {e}"
        ))),
    }
}

/// Store or overwrite a model API key (`duthub/model/<model_id>`).
pub fn set_key(model_id: &str, api_key: &str) -> DeviceResult<()> {
    set_secret("model", model_id, "model", api_key)
}

/// Read a model API key (`duthub/model/<model_id>`); missing -> `Ok(None)`.
pub fn get_key(model_id: &str) -> DeviceResult<Option<String>> {
    get_secret("model", model_id, "model")
}

/// Delete a model API key (idempotent).
pub fn delete_key(model_id: &str) -> DeviceResult<()> {
    delete_secret("model", model_id, "model")
}

/// Store or overwrite a per-DUT A2A token (`duthub/a2a/<dut_id>`).
pub fn set_a2a_token(dut_id: &str, token: &str) -> DeviceResult<()> {
    set_secret("a2a", dut_id, "A2A", token)
}

/// Read a per-DUT A2A token (`duthub/a2a/<dut_id>`); missing -> `Ok(None)`.
pub fn get_a2a_token(dut_id: &str) -> DeviceResult<Option<String>> {
    get_secret("a2a", dut_id, "A2A")
}

/// Delete a per-DUT A2A token (idempotent).
pub fn delete_a2a_token(dut_id: &str) -> DeviceResult<()> {
    delete_secret("a2a", dut_id, "A2A")
}

/// Store or overwrite an MCP Bearer token (`duthub/mcp/<credential_id>`).
pub fn set_mcp_token(credential_id: &str, token: &str) -> DeviceResult<()> {
    set_secret("mcp", credential_id, "MCP", token)
}

/// Read an MCP Bearer token (`duthub/mcp/<credential_id>`); missing -> `Ok(None)`.
pub fn get_mcp_token(credential_id: &str) -> DeviceResult<Option<String>> {
    get_secret("mcp", credential_id, "MCP")
}

/// Delete an MCP Bearer token (idempotent).
pub fn delete_mcp_token(credential_id: &str) -> DeviceResult<()> {
    delete_secret("mcp", credential_id, "MCP")
}

/// Fail-closed export scanner: returns true if any JSON key or long string value
/// looks like a secret/token.
pub fn export_contains_secret(json: &serde_json::Value) -> bool {
    const SUSPICIOUS: &[&str] = &["apikey", "api_key", "secret", "password", "token"];
    match json {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let k_lower = k.to_ascii_lowercase();
                if SUSPICIOUS.iter().any(|s| k_lower.contains(s)) {
                    return true;
                }
                if let serde_json::Value::String(vs) = v
                    && vs.len() > 8
                {
                    let v_lower = vs.to_ascii_lowercase();
                    if SUSPICIOUS.iter().any(|s| v_lower.contains(s)) {
                        return true;
                    }
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
    #[ignore = "requires live OS credential store; run manually"]
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
