//! Approval token unpredictability and audit hardening (W1).
//!
//! - Issued tokens come from the CSPRNG, are unique per call and cannot be
//!   derived from the bound `(dut, action, args)` hash.
//! - The in-process audit tail and the authoritative `AuditSink` only ever see
//!   SHA-256 fingerprints; the plaintext token never reaches either.
//! - Free-text redaction normalizes key spellings before suffix matching, so
//!   `access_token` / `api-key` / `clientSecret` are all erased.
//! - The JSONL audit file rotates before crossing [`MAX_AUDIT_FILE_BYTES`].

use duthub_core::ApprovalManager;
use duthub_core::approval::{ApprovalFingerprint, args_hash};
use duthub_core::audit::redact_detail;
use duthub_core::audit::{AuditOutcome, AuditSink, MAX_AUDIT_FILE_BYTES, MAX_AUDIT_ROTATIONS};

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-approval-audit-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn issued_tokens_are_random_and_not_derived_from_the_args_hash() {
    let manager = ApprovalManager::new();
    let args = r#"{"text":"uname -a"}"#;
    let first = manager.issue("dut-a", "serial.write", args);
    let second = manager.issue("dut-a", "serial.write", args);
    let hash = args_hash("dut-a", "serial.write", args);
    let hash_prefix = &hash[..16];

    assert!(first.token.starts_with("apr-"), "{}", first.token);
    assert_ne!(
        first.token, second.token,
        "identical arguments must not produce identical tokens"
    );
    assert!(
        !first.token.contains(&hash),
        "token must not embed the full args hash"
    );
    assert!(
        !first.token.contains(hash_prefix),
        "token must not embed the hash prefix"
    );
    assert_ne!(
        first.token,
        format!("apr-{hash_prefix}"),
        "the legacy predictable shape must be gone"
    );
    // The random part is a full CSPRNG token (`apr-` + `dth-` + 43 base64 chars).
    assert!(first.token.len() >= 4 + 4 + 43, "{}", first.token.len());
}

#[test]
fn audit_tail_and_sink_carry_fingerprints_but_never_the_plaintext_token() {
    let sink = AuditSink::memory();
    let manager = ApprovalManager::new().with_audit(sink.clone());
    let args = r#"{"on":true}"#;
    let token = manager.issue("dut-a", "control.power", args);
    manager
        .validate(&token.token, "dut-a", "control.power", args)
        .unwrap();

    let tail = manager.audit_tail(10).join("\n");
    assert!(tail.contains("token_sha256="), "{tail}");
    assert!(tail.contains("args_sha256="), "{tail}");
    assert!(
        !tail.contains(&token.token),
        "audit tail leaked the plaintext token"
    );

    let events = sink.recent(10);
    let issue = events
        .iter()
        .find(|event| event.action == "approval.issue")
        .expect("approval.issue must reach the AuditSink");
    let consume = events
        .iter()
        .find(|event| event.action == "approval.consume")
        .expect("approval.consume must reach the AuditSink");
    assert_eq!(issue.dut.as_deref(), Some("dut-a"));
    assert_eq!(issue.result, AuditOutcome::Ok);
    assert_eq!(consume.dut.as_deref(), Some("dut-a"));
    for event in [issue, consume] {
        let detail = event.detail.as_deref().unwrap_or_default();
        assert!(detail.contains("token_sha256="), "{detail}");
        assert!(!detail.contains(&token.token), "detail leaked token");
    }
    let raw = format!("{events:?}");
    assert!(!raw.contains(&token.token), "AuditSink leaked the token");

    // The public fingerprint type exposes hashes only (contract guard).
    let fingerprint = ApprovalFingerprint {
        token_sha256: "a".repeat(64),
        dut_id: "dut-a".into(),
        action: "control.power".into(),
        args_sha256: args_hash("dut-a", "control.power", args),
    };
    assert!(fingerprint.summary().contains("token_sha256="));
}

#[test]
fn redaction_normalizes_key_spellings_and_suffixes() {
    // New spellings required by the hardening task.
    assert_eq!(
        redact_detail("access_token=sk-live-123"),
        "access_token=[REDACTED]"
    );
    assert_eq!(redact_detail("api-key: leaked"), "api-key: [REDACTED]");
    assert_eq!(
        redact_detail("clientSecret=top-secret"),
        "clientSecret=[REDACTED]"
    );
    assert_eq!(
        redact_detail("db_password=hunter2"),
        "db_password=[REDACTED]"
    );
    assert_eq!(
        redact_detail(r#"{"apiKey":"abc","dut":"d1"}"#),
        r#"{"apiKey":"[REDACTED]","dut":"d1"}"#
    );
    // Legacy forms stay covered.
    assert_eq!(redact_detail("token=abc"), "token=[REDACTED]");
    assert_eq!(
        redact_detail("Authorization: Bearer sk-1"),
        "Authorization: [REDACTED] [REDACTED]"
    );
    // Non-sensitive identifiers are preserved.
    assert_eq!(
        redact_detail("credential_id=cred-1 lease=lease-9 token_sha256=deadbeef"),
        "credential_id=cred-1 lease=lease-9 token_sha256=deadbeef"
    );
}

#[test]
fn audit_file_rotates_instead_of_growing_without_bound() {
    let dir = tmp_dir("rotation");
    let sink = AuditSink::at_dir(dir.clone());
    // Each event is ~1 MiB; nine of them force exactly one rotation.
    let blob = "x".repeat(1024 * 1024);
    for _ in 0..9 {
        sink.record("tool.call", None, None, None, AuditOutcome::Ok, Some(&blob));
    }

    let files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("agent-access-") && name.ends_with(".jsonl")
        })
        .collect();
    assert!(files.len() >= 2, "expected a rotated file: {files:?}");
    assert!(
        files.len() as u32 <= MAX_AUDIT_ROTATIONS + 1,
        "rotation must stay bounded: {files:?}"
    );
    for path in &files {
        let size = std::fs::metadata(path).unwrap().len();
        assert!(
            size <= MAX_AUDIT_FILE_BYTES,
            "{} exceeded the cap: {size}",
            path.display()
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}
