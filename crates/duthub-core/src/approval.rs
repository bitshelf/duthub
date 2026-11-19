//! 写操作审批令牌（`docs/API_CONTRACT.md`；`docs/SECURITY.md` #3）。
//!
//! - 令牌由 core 签发：单次有效、5 分钟过期、绑定 dut+action+参数哈希。
//! - 无 token / 过期 / 不匹配 / 已使用的写操作一律拒绝。
//! - M4 阶段 token 经 MCP `approval.request` 工具签发并写入审计日志；
//!   M5 接入 GUI 审批闸门后改为"申请 → 人工确认 → 发放"。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const APPROVAL_TTL_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    /// 未携带 token
    Missing,
    /// token 不存在（过期或已使用）
    Unknown,
    /// dut / action / 参数哈希 与签发时不匹配
    Mismatch,
}

impl ApprovalError {
    pub fn message(&self) -> &'static str {
        match self {
            ApprovalError::Missing => "缺少审批令牌：先调用 approval.request 签发",
            ApprovalError::Unknown => "审批令牌无效或已使用/过期：重新调用 approval.request",
            ApprovalError::Mismatch => "审批令牌与本次操作的 dut/action/参数不匹配",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalToken {
    pub token: String,
    pub dut_id: String,
    pub action: String,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone)]
struct Entry {
    dut_id: String,
    action: String,
    args_hash: String,
    issued: Instant,
}

pub fn args_hash(dut_id: &str, action: &str, args_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dut_id.as_bytes());
    hasher.update(b"|");
    hasher.update(action.as_bytes());
    hasher.update(b"|");
    hasher.update(args_json.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 审批令牌管理器（进程内，单次消费）。
pub struct ApprovalManager {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    audit: Mutex<Vec<String>>,
}

impl Default for ApprovalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalManager {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(APPROVAL_TTL_SECS),
            audit: Mutex::new(Vec::new()),
        }
    }

    /// 签发令牌（并记录审计行：时间戳 / dut / action / 参数哈希）。
    pub fn issue(&self, dut_id: &str, action: &str, args_json: &str) -> ApprovalToken {
        let hash = args_hash(dut_id, action, args_json);
        let token = format!("apr-{}", &hash[..16.min(hash.len())]);
        let token = format!(
            "{token}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        );
        self.entries.lock().unwrap().insert(
            token.clone(),
            Entry {
                dut_id: dut_id.to_string(),
                action: action.to_string(),
                args_hash: hash.clone(),
                issued: Instant::now(),
            },
        );
        self.audit.lock().unwrap().push(format!(
            "[audit] issue token={token} dut={dut_id} action={action} args_sha={hash}"
        ));
        ApprovalToken {
            token,
            dut_id: dut_id.to_string(),
            action: action.to_string(),
            expires_in_secs: self.ttl.as_secs(),
        }
    }

    /// 校验并消费令牌（单次）；不匹配 / 过期 / 已使用 → 错误。
    ///
    /// 语义 = [`Self::peek`] + [`Self::consume`]（P1-014 器件层前置拆分）：
    /// 既有调用方（control.* 写路径）行为不变。
    pub fn validate(
        &self,
        token: &str,
        dut_id: &str,
        action: &str,
        args_json: &str,
    ) -> Result<(), ApprovalError> {
        self.peek(token, dut_id, action, args_json)?;
        self.consume(token)
    }

    /// 校验令牌（存在 / 未过期 / dut+action+参数匹配）但**不消费**。
    ///
    /// 供审批链先做租约/授权复核、全部就绪后再 consume（M10：admit 不通过
    /// 不烧令牌）。过期令牌顺手清除（与旧 validate 行为一致）。
    pub fn peek(
        &self,
        token: &str,
        dut_id: &str,
        action: &str,
        args_json: &str,
    ) -> Result<(), ApprovalError> {
        if token.trim().is_empty() {
            return Err(ApprovalError::Missing);
        }
        let mut entries = self.entries.lock().unwrap();
        let Some(entry) = entries.get(token) else {
            return Err(ApprovalError::Unknown);
        };
        if entry.issued.elapsed() > self.ttl {
            entries.remove(token);
            return Err(ApprovalError::Unknown);
        }
        let hash = args_hash(dut_id, action, args_json);
        if entry.dut_id != dut_id || entry.action != action || entry.args_hash != hash {
            return Err(ApprovalError::Mismatch);
        }
        Ok(())
    }

    /// 消费令牌：移除并返回结果；已用 / 不存在 / 为空 → 错误（不记录审计）。
    /// 仅在 peek 通过后调用；重复消费（一次性语义）报 Unknown。
    pub fn consume(&self, token: &str) -> Result<(), ApprovalError> {
        if token.trim().is_empty() {
            return Err(ApprovalError::Missing);
        }
        let Some(entry) = self.entries.lock().unwrap().remove(token) else {
            return Err(ApprovalError::Unknown);
        };
        self.audit.lock().unwrap().push(format!(
            "[audit] consume token={token} dut={} action={}",
            entry.dut_id, entry.action
        ));
        Ok(())
    }

    /// 审计日志（近期记录）。
    pub fn audit_tail(&self, limit: usize) -> Vec<String> {
        let audit = self.audit.lock().unwrap();
        audit.iter().rev().take(limit).rev().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_validate_consume_once() {
        let mgr = ApprovalManager::new();
        let token = mgr.issue("dut-a", "serial.write", r#"{"text":"uname"}"#);
        mgr.validate(&token.token, "dut-a", "serial.write", r#"{"text":"uname"}"#)
            .unwrap();
        // 单次消费：第二次必须 Unknown
        assert_eq!(
            mgr.validate(&token.token, "dut-a", "serial.write", r#"{"text":"uname"}"#),
            Err(ApprovalError::Unknown)
        );
    }

    #[test]
    fn mismatch_rejected() {
        let mgr = ApprovalManager::new();
        let token = mgr.issue("dut-a", "control.power", r#"{"on":true}"#);
        assert_eq!(
            mgr.validate(&token.token, "dut-b", "control.power", r#"{"on":true}"#),
            Err(ApprovalError::Mismatch)
        );
        assert_eq!(
            mgr.validate(&token.token, "dut-a", "control.reset", r#"{}"#),
            Err(ApprovalError::Mismatch)
        );
        assert_eq!(
            mgr.validate(&token.token, "dut-a", "control.power", r#"{"on":false}"#),
            Err(ApprovalError::Mismatch)
        );
    }

    #[test]
    fn missing_and_unknown() {
        let mgr = ApprovalManager::new();
        assert_eq!(
            mgr.validate("", "d", "a", "{}"),
            Err(ApprovalError::Missing)
        );
        assert_eq!(
            mgr.validate("apr-nope", "d", "a", "{}"),
            Err(ApprovalError::Unknown)
        );
    }

    #[test]
    fn args_hash_is_stable_and_binds_all_inputs() {
        let h1 = args_hash("dut-a", "serial.write", r#"{"text":"x"}"#);
        let h2 = args_hash("dut-a", "serial.write", r#"{"text":"x"}"#);
        let h3 = args_hash("dut-a", "serial.write", r#"{"text":"y"}"#);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_ne!(h1, args_hash("dut-b", "serial.write", r#"{"text":"x"}"#));
    }

    /// P1-014 器件层前置：peek 只校验不消费，consume 才移除；
    /// validate = peek + consume（既有语义不变）。
    #[test]
    fn peek_does_not_consume_and_consume_removes() {
        let mgr = ApprovalManager::new();
        let token = mgr.issue("dut-a", "serial.write", r#"{"text":"uname"}"#);
        // peek 可重复：令牌不被消费
        mgr.peek(&token.token, "dut-a", "serial.write", r#"{"text":"uname"}"#)
            .unwrap();
        mgr.peek(&token.token, "dut-a", "serial.write", r#"{"text":"uname"}"#)
            .unwrap();
        // peek 仍会拒绝不匹配
        assert_eq!(
            mgr.peek(&token.token, "dut-b", "serial.write", r#"{"text":"uname"}"#),
            Err(ApprovalError::Mismatch)
        );
        // consume 移除后：再次 consume 与 peek 均 Unknown（一次性语义）
        mgr.consume(&token.token).unwrap();
        assert_eq!(mgr.consume(&token.token), Err(ApprovalError::Unknown));
        assert_eq!(
            mgr.peek(&token.token, "dut-a", "serial.write", r#"{"text":"uname"}"#),
            Err(ApprovalError::Unknown)
        );
        // validate 语义不变：peek + consume
        let token2 = mgr.issue("dut-a", "control.power", r#"{"on":true}"#);
        mgr.validate(&token2.token, "dut-a", "control.power", r#"{"on":true}"#)
            .unwrap();
        assert_eq!(
            mgr.validate(&token2.token, "dut-a", "control.power", r#"{"on":true}"#),
            Err(ApprovalError::Unknown)
        );
    }
}
