//! Single-use approvals for write operations.
//!
//! Generate unpredictable tokens that expire after five minutes.
//! Bind each token to the DUT, action, and parameter hash.
//! Reject absent, expired, replayed, or mismatched tokens.
//! Audit only the SHA-256 fingerprint.
//! Record issuance and consumption without logging token contents.
//! Clients request approval through the MCP tool.
//! The GUI can require a confirmation before issuing the token.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent_access::generate_token;
use crate::audit::{AuditOutcome, AuditSink};

pub const APPROVAL_TTL_SECS: u64 = 300;

/// Maximum number of recent approval fingerprints kept in memory.
const AUDIT_TAIL_CAP: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    /// Missing approval token.
    #[error("Missing approval token. Request one with approval.request")]
    Missing,
    /// Unknown, expired, or already-consumed approval token.
    #[error(
        "Approval token is invalid, used, or expired. Request a new token with approval.request"
    )]
    Unknown,
    /// Token does not match the requested DUT, action, or argument hash.
    #[error("Approval token does not match the DUT, action, or parameters")]
    Mismatch,
}

impl ApprovalError {
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            ApprovalError::Missing => "Missing approval token. Request one with approval.request",
            ApprovalError::Unknown => {
                "Approval token is invalid, used, or expired. Request a new token with approval.request"
            }
            ApprovalError::Mismatch => {
                "Approval token does not match the DUT, action, or parameters"
            }
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

/// Audit-safe approval fingerprint: hashes only, never the plaintext token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalFingerprint {
    /// SHA-256 of the issued token (the token itself is single-use and short-lived).
    pub token_sha256: String,
    pub dut_id: String,
    pub action: String,
    /// Same hash used to bind the token to its arguments.
    pub args_sha256: String,
}

impl ApprovalFingerprint {
    /// One-line summary for the audit tail and `AuditSink` detail.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "token_sha256={} dut={} action={} args_sha256={}",
            self.token_sha256, self.dut_id, self.action, self.args_sha256
        )
    }
}

#[must_use]
pub fn args_hash(dut_id: &str, action: &str, args_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dut_id.as_bytes());
    hasher.update(b"|");
    hasher.update(action.as_bytes());
    hasher.update(b"|");
    hasher.update(args_json.as_bytes());
    crate::util::hex_lower(&hasher.finalize())
}

fn token_sha256(token: &str) -> String {
    crate::util::sha256_hex(token.as_bytes())
}

/// In-process store for issuing and consuming single-use approvals.
pub struct ApprovalManager {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    /// Bounded fingerprint ring; contains no plaintext secrets.
    audit: Mutex<VecDeque<ApprovalFingerprint>>,
    audit_sink: Option<Arc<AuditSink>>,
}

impl std::fmt::Debug for ApprovalManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending = crate::util::lock_unpoisoned(&self.entries).len();
        f.debug_struct("ApprovalManager")
            .field("pending", &pending)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl Default for ApprovalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(APPROVAL_TTL_SECS),
            audit: Mutex::new(VecDeque::new()),
            audit_sink: None,
        }
    }

    /// Attach the authoritative audit sink (desktop runtime); issue/consume then
    /// emit `approval.issue` / `approval.consume` events with fingerprints only.
    #[must_use]
    pub fn with_audit(mut self, sink: Arc<AuditSink>) -> Self {
        self.audit_sink = Some(sink);
        self
    }

    /// Issue a token and audit its time, target, action, and parameter hash.
    ///
    /// The token is generated from the CSPRNG and never derived from the
    /// arguments hash, so guessing the bound arguments does not reveal it.
    pub fn issue(&self, dut_id: &str, action: &str, args_json: &str) -> ApprovalToken {
        let hash = args_hash(dut_id, action, args_json);
        let token = format!("apr-{}", generate_token());
        {
            let mut entries = crate::util::lock_unpoisoned(&self.entries);
            entries.retain(|_, entry| entry.issued.elapsed() <= self.ttl);
            entries.insert(
                token.clone(),
                Entry {
                    dut_id: dut_id.to_string(),
                    action: action.to_string(),
                    args_hash: hash.clone(),
                    issued: Instant::now(),
                },
            );
        }
        let fingerprint = ApprovalFingerprint {
            token_sha256: token_sha256(&token),
            dut_id: dut_id.to_string(),
            action: action.to_string(),
            args_sha256: hash,
        };
        self.record_fingerprint("approval.issue", &fingerprint);
        ApprovalToken {
            token,
            dut_id: dut_id.to_string(),
            action: action.to_string(),
            expires_in_secs: self.ttl.as_secs(),
        }
    }

    /// Check and consume a valid token exactly once.
    ///
    /// Combines `Self::peek` with `Self::consume`.
    /// Reject expired, mismatched, and used tokens.
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

    /// Check expiry and request binding without consuming the token.
    ///
    /// Callers can verify leases and permissions before committing consumption.
    /// Discard expired entries.
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
        let mut entries = crate::util::lock_unpoisoned(&self.entries);
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

    /// Remove an approved token and record its consumption.
    /// Call after validation. Absent or already used tokens return an error.
    pub fn consume(&self, token: &str) -> Result<(), ApprovalError> {
        if token.trim().is_empty() {
            return Err(ApprovalError::Missing);
        }
        let Some(entry) = crate::util::lock_unpoisoned(&self.entries).remove(token) else {
            return Err(ApprovalError::Unknown);
        };
        let fingerprint = ApprovalFingerprint {
            token_sha256: token_sha256(token),
            dut_id: entry.dut_id,
            action: entry.action,
            args_sha256: entry.args_hash,
        };
        self.record_fingerprint("approval.consume", &fingerprint);
        Ok(())
    }

    /// Recent audit records contain fingerprints only.
    pub fn audit_tail(&self, limit: usize) -> Vec<String> {
        let audit = crate::util::lock_unpoisoned(&self.audit);
        audit
            .iter()
            .rev()
            .take(limit)
            .rev()
            .map(|fingerprint| format!("[audit] {}", fingerprint.summary()))
            .collect()
    }

    fn record_fingerprint(&self, action: &str, fingerprint: &ApprovalFingerprint) {
        {
            let mut audit = crate::util::lock_unpoisoned(&self.audit);
            if audit.len() >= AUDIT_TAIL_CAP {
                audit.pop_front();
            }
            audit.push_back(fingerprint.clone());
        }
        if let Some(sink) = &self.audit_sink {
            sink.record(
                action,
                None,
                None,
                Some(fingerprint.dut_id.as_str()),
                AuditOutcome::Ok,
                Some(&fingerprint.summary()),
            );
        }
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
        // The second consumption must report an unknown token.
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
            mgr.validate(&token.token, "dut-a", "control.reset", "{}"),
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

    /// Validation without consumption leaves the token available.
    /// The combined API still validates and consumes in one call.
    #[test]
    fn peek_does_not_consume_and_consume_removes() {
        let mgr = ApprovalManager::new();
        let token = mgr.issue("dut-a", "serial.write", r#"{"text":"uname"}"#);
        // Repeated validation does not consume the token.
        mgr.peek(&token.token, "dut-a", "serial.write", r#"{"text":"uname"}"#)
            .unwrap();
        mgr.peek(&token.token, "dut-a", "serial.write", r#"{"text":"uname"}"#)
            .unwrap();
        // Validation rejects a different request.
        assert_eq!(
            mgr.peek(&token.token, "dut-b", "serial.write", r#"{"text":"uname"}"#),
            Err(ApprovalError::Mismatch)
        );
        // Once consumed, the token cannot be validated or consumed again.
        mgr.consume(&token.token).unwrap();
        assert_eq!(mgr.consume(&token.token), Err(ApprovalError::Unknown));
        assert_eq!(
            mgr.peek(&token.token, "dut-a", "serial.write", r#"{"text":"uname"}"#),
            Err(ApprovalError::Unknown)
        );
        // The combined operation consumes a valid token.
        let token2 = mgr.issue("dut-a", "control.power", r#"{"on":true}"#);
        mgr.validate(&token2.token, "dut-a", "control.power", r#"{"on":true}"#)
            .unwrap();
        assert_eq!(
            mgr.validate(&token2.token, "dut-a", "control.power", r#"{"on":true}"#),
            Err(ApprovalError::Unknown)
        );
    }
}
