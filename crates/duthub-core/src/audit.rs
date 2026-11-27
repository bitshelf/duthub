//! Audit events and writing to disk in JSONL (`docs/SECURITY.md`).
//!
//! - Event namespace: `auth.ok`/`auth.fail`, `grant.deny`,
//!   `lease.acquire`/`renew`/`release`/`expire`/`takeover`/`queue`/`cancel`/`protected.enter`/`protected.exit`,
//!   `write.admit`/`write.deny`, `control.execute`/`control.deny`, `approval.issue`/`consume`.
//! - Dual channel: in-memory ring buffer (capacity 1000, read by the GUI panel) plus daily rotated
//!   JSONL files at `<workdir>/audit/agent-access-YYYYMMDD.jsonl` (authoritative on-disk record).
//! - **Redaction boundary**: raw tokens, approval tokens, and `Authorization` values are never
//!   written to audit logs (including free-text `detail`, which is scrubbed via [`redact_detail`]
//!   before writing). `lease_id` is a high-entropy handle (SPEC §6.3: handle != permission) and is
//!   permitted.
//! - **Disk quota**: once a daily file exceeds [`MAX_AUDIT_FILE_BYTES`], it rotates to a new shard.
//!   After [`MAX_AUDIT_ROTATIONS`] rotations, on-disk persistence pauses for the rest of the day
//!   (while the in-memory ring keeps recording) so long-running processes cannot exhaust disk space.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Memory ring capacity (The GUI panel displays the last 200 items; there is also a buffer reserved
/// for the ring).
const RING_CAP: usize = 1000;

/// The maximum size of a single audit JSONL file is 8 MiB.
pub const MAX_AUDIT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// The maximum number of rotations per day; writing to disk stops after reaching the limit (the
/// memory ring is not affected).
pub const MAX_AUDIT_ROTATIONS: u32 = 4;

/// Categories of audit results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Ok,
    Denied,
    Error,
}

impl AuditOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AuditOutcome::Ok => "ok",
            AuditOutcome::Denied => "denied",
            AuditOutcome::Error => "error",
        }
    }
}

/// Individual audit events (in JSONL format, with camelCase naming conventions, consistent with the
/// settings family).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    /// Unix milliseconds timestamp
    pub ts_unix_ms: u64,
    /// Event name (see the module annotation namespace for details)
    pub action: String,
    /// Agent display name (for stdio clients, it is set to `stdio-admin`; for GUI clients, it is
    /// set to `None`).
    pub agent: Option<String>,
    /// Stable credential ID (not a token)
    pub credential_id: Option<String>,
    /// Target DUT (For authentication-related events, the value is "`None`").
    pub dut: Option<String>,
    pub result: AuditOutcome,
    /// Free text summary (sanitized before writing)
    pub detail: Option<String>,
}

impl AuditEvent {
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        // Single-line compression: jsonl allows one object per line.
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

#[must_use]
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Masked Free Text: The value portion is erased after the sensitive keys are removed
/// (case-insensitive).
///
/// Normalize keys by lowercasing them and removing underscores and hyphens. Redact exact
/// authorization and bearer keys and keys ending in token, key, secret, or password. This catches
/// `access_token`, api-key, clientSecret, and approvalToken while preserving `lease_id` and
/// `credential_id`.
#[must_use]
pub fn redact_detail(text: &str) -> String {
    let mut cut_ranges: Vec<(usize, usize)> = Vec::new();
    for (start, end) in key_candidates(text) {
        if is_sensitive_key(&text[start..end])
            && let Some(range) = value_range(text, end)
        {
            cut_ranges.push(range);
        }
    }
    if cut_ranges.is_empty() {
        return text.to_string();
    }
    cut_ranges.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (s, e) in cut_ranges {
        if s < cursor {
            continue; // Overlaps with the previous section (multiple keys with the same value were hit)
        }
        out.push_str(&text[cursor..s]);
        out.push_str("[REDACTED]");
        cursor = e;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Scanning for key candidates in the text: consecutive ASCII letters and numbers, underscores,
/// hyphens.
fn key_candidates(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut candidates = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' || bytes[index] == b'-' {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || bytes[index] == b'_'
                    || bytes[index] == b'-')
            {
                index += 1;
            }
            candidates.push((start, index));
        } else {
            index += 1;
        }
    }
    candidates
}

/// Determine whether the key names are sensitive after normalization (based on matching
/// `Authorization`/`Bearer` or suffix).
fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|ch| *ch != '_' && *ch != '-')
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    matches!(normalized.as_str(), "authorization" | "bearer")
        || ["token", "key", "secret", "password"]
            .iter()
            .any(|suffix| normalized.ends_with(suffix))
}

/// Locate the value segment consisting of "separator + quotation marks + value", based on the given
/// key end position (byte index, according to UTF-8 boundaries).
/// Separator formats: `key=value`, `key:value`, or JSON format `"key":"value""` (closed quotation
/// marks precede the separator).
/// Format: `key value` (Bearer header style; spaces indicate separation).
fn value_range(text: &str, key_end: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = key_end;
    let mut saw_space = false;
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        saw_space = true;
        i += 1;
    }
    match bytes.get(i).copied() {
        Some(b'"' | b'\'') => {
            // JSON keys end with a closing quote followed by `=` or `:`.
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if !matches!(bytes.get(i), Some(b'=' | b':')) {
                return None;
            }
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        Some(b'=' | b':') => {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        _ if saw_space => {}
        _ => return None,
    }
    let mut value_start = i;
    let quoted = i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'');
    let quote = if quoted { bytes[i] } else { 0 };
    if quoted {
        value_start += 1;
        i += 1;
    }
    while i < bytes.len() {
        let c = bytes[i];
        if quoted {
            if c == quote {
                return Some((value_start, i));
            }
        } else if matches!(c, b',' | b';' | b'}' | b' ' | b'\t' | b'\r' | b'\n') {
            return Some((value_start, i));
        }
        i += 1;
    }
    if i > value_start {
        Some((value_start, i))
    } else {
        None
    }
}

/// Audit receiving endpoint: Memory ring + daily JSONL files. When `dir` is `None`, only a memory
/// ring is used; this is for testing purposes.
pub struct AuditSink {
    ring: Mutex<VecDeque<AuditEvent>>,
    dir: Option<PathBuf>,
    /// The cache of current day's file handles is already open (key = YYYYMMDD). If the cache needs
    /// to be recreated across days or beyond the allowed limit, it will be done accordingly.
    day_file: Mutex<Option<DayFile>>,
}

impl std::fmt::Debug for AuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditSink")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// Audit file status on that day (Byte count-driven rotation; handles marked as `None` indicate
/// that writing has stopped).
struct DayFile {
    day: String,
    file: Option<std::fs::File>,
    bytes: u64,
    rotations: u32,
    /// Stop writing to disk when the maximum number of rotations is reached or the write operation
    /// fails, to prevent unlimited growth.
    stopped: bool,
}

impl DayFile {
    /// Close the current handle, rename the file for that day to `agent-access-YYYYMMDD.rN.jsonl`,
    /// and create a new empty file to continue writing. If there is a failure, stop writing the
    /// file for that day (without exceeding the volume limit).
    fn rotate(&mut self, dir: &Path) -> Result<(), String> {
        if self.rotations >= MAX_AUDIT_ROTATIONS {
            self.stopped = true;
            return Err(format!(
                "Audit file reached rotation limit ({MAX_AUDIT_ROTATIONS}); stopping disk persistence for today"
            ));
        }
        let base = dir.join(format!("agent-access-{}.jsonl", self.day));
        let rotated = dir.join(format!(
            "agent-access-{}.r{}.jsonl",
            self.day,
            self.rotations + 1
        ));
        if let Some(file) = self.file.take() {
            let _ = file.sync_all();
        }
        // Windows requires that the handle be closed before the name is changed.
        if let Err(err) = std::fs::rename(&base, &rotated) {
            self.stopped = true;
            return Err(format!(
                "Failed to rotate audit file {} -> {}: {err}",
                base.display(),
                rotated.display()
            ));
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&base)
        {
            Ok(file) => {
                self.file = Some(file);
                self.bytes = 0;
                self.rotations += 1;
                Ok(())
            }
            Err(err) => {
                self.stopped = true;
                Err(format!(
                    "Failed to open rotated audit file {}: {err}",
                    base.display()
                ))
            }
        }
    }
}

/// Open the files for that day and count the number of existing round-robin slices available (the
/// limit can be maintained even after the process is restarted).
fn open_day_file(dir: &Path, day: &str) -> Result<DayFile, String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("Failed to create audit directory: {err}"))?;
    let path = dir.join(format!("agent-access-{day}.jsonl"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("Failed to open audit file {}: {err}", path.display()))?;
    let bytes = file.metadata().map_or(0, |meta| meta.len());
    Ok(DayFile {
        day: day.to_string(),
        file: Some(file),
        bytes,
        rotations: count_rotations(dir, day),
        stopped: false,
    })
}

/// Count the number of shards in the `agent-access-YYYYMMDD.rN.jsonl` file.
fn count_rotations(dir: &Path, day: &str) -> u32 {
    let prefix = format!("agent-access-{day}.r");
    std::fs::read_dir(dir).map_or(0, |entries| {
        let count = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(&prefix) && name.ends_with(".jsonl")
            })
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    })
}

impl AuditSink {
    /// Memory ring mode (testing / scenario without a working directory).
    #[must_use]
    pub fn memory() -> Arc<Self> {
        Arc::new(Self {
            ring: Mutex::new(VecDeque::new()),
            dir: None,
            day_file: Mutex::new(None),
        })
    }

    /// Write to Disk Mode: `<dir>/agent-access-YYYYMMDD.jsonl` rotates daily (the directory is the
    /// working directory `audit/`).
    #[must_use]
    pub fn at_dir(dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            ring: Mutex::new(VecDeque::new()),
            dir: Some(dir),
            day_file: Mutex::new(None),
        })
    }

    /// Record an audit event (masking → memory ring → tracing → file). All operations are performed
    /// in best-effort mode. Audit failures do not interrupt normal operations; only `tracing::warn`
    /// messages are generated.
    pub fn record(
        &self,
        action: &str,
        agent: Option<&str>,
        credential_id: Option<&str>,
        dut: Option<&str>,
        result: AuditOutcome,
        detail: Option<&str>,
    ) {
        let event = AuditEvent {
            ts_unix_ms: unix_ms(),
            action: action.to_string(),
            agent: agent.map(str::to_string),
            credential_id: credential_id.map(str::to_string),
            dut: dut.map(str::to_string),
            result,
            detail: detail.map(redact_detail),
        };
        {
            let mut ring = crate::util::lock_unpoisoned(&self.ring);
            if ring.len() >= RING_CAP {
                ring.pop_front();
            }
            ring.push_back(event.clone());
        }
        tracing::info!(
            target: "audit",
            action = %event.action,
            agent = event.agent.as_deref().unwrap_or(""),
            dut = event.dut.as_deref().unwrap_or(""),
            result = event.result.as_str(),
            "audit"
        );
        self.append_file(&event);
    }

    fn append_file(&self, event: &AuditEvent) {
        let Some(dir) = &self.dir else { return };
        let day = day_key(event.ts_unix_ms);
        let line = format!("{}\n", event.to_jsonl());
        let mut guard = crate::util::lock_unpoisoned(&self.day_file);
        let needs_open = guard.as_ref().is_none_or(|state| state.day != day);
        if needs_open {
            match open_day_file(dir, &day) {
                Ok(state) => *guard = Some(state),
                Err(err) => {
                    tracing::warn!(target: "audit", "{err}");
                    return;
                }
            }
        }
        let Some(state) = guard.as_mut() else { return };
        if state.stopped {
            return;
        }
        let line_bytes = line.len() as u64;
        if state.bytes > 0
            && state.bytes.saturating_add(line_bytes) > MAX_AUDIT_FILE_BYTES
            && let Err(err) = state.rotate(dir)
        {
            tracing::warn!(target: "audit", "{err}");
            return;
        }
        if state.bytes.saturating_add(line_bytes) > MAX_AUDIT_FILE_BYTES {
            // A single event larger than the cap would defeat the limit.
            state.stopped = true;
            tracing::warn!(target: "audit", "Audit entry exceeds the file limit. Stopping audit persistence for today");
            return;
        }
        if let Some(file) = state.file.as_mut() {
            if let Err(err) = file.write_all(line.as_bytes()) {
                tracing::warn!(target: "audit", "Failed to write audit record: {err}");
                return;
            }
            state.bytes += line_bytes;
        }
    }

    /// The memory ring has recently processed `limit` items (sorted in ascending order, based on
    /// the data source from the GUI panel).
    pub fn recent(&self, limit: usize) -> Vec<AuditEvent> {
        let ring = crate::util::lock_unpoisoned(&self.ring);
        ring.iter().rev().take(limit).rev().cloned().collect()
    }
}

fn day_key(ts_unix_ms: u64) -> String {
    // Number of Days → YYYYMMDD (Based on the "from days" algorithm; no dependency on chrono)
    let days = i64::try_from(ts_unix_ms / 86_400_000).unwrap_or(i64::MAX);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_plain_and_json_and_bearer_forms() {
        assert_eq!(redact_detail("token=abc123"), "token=[REDACTED]");
        assert_eq!(redact_detail("token: abc123"), "token: [REDACTED]");
        // JSON format: Quoted keys should be placed before the separator; quoted values remain
        // unchanged, and the content is replaced accordingly.
        assert_eq!(
            redact_detail(r#"{"approvalToken":"apr-xyz","dut":"d1"}"#),
            r#"{"approvalToken":"[REDACTED]","dut":"d1"}"#
        );
        // Dual hits in authorization and bearer fields (deep defense mechanism)
        assert_eq!(
            redact_detail("Authorization: Bearer sk-live-999"),
            "Authorization: [REDACTED] [REDACTED]"
        );
        // Case-insensitivity
        assert_eq!(redact_detail("TOKEN = leak"), "TOKEN = [REDACTED]");
        // lease_id allows retention (handle ≠ permissions)
        assert_eq!(
            redact_detail("lease=lease-abc token=zzz"),
            "lease=lease-abc token=[REDACTED]"
        );
        // Word boundaries: Combinations that do not involve this key should not be mistakenly
        // damaged.
        assert_eq!(
            redact_detail("credential_id=cred-1"),
            "credential_id=cred-1"
        );
        // There is no separator at the end.
        assert_eq!(redact_detail("use token p0"), "use token [REDACTED]");
    }

    #[test]
    fn record_appends_jsonl_without_secrets() {
        let dir =
            std::env::temp_dir().join(format!("duthub-audit-{}-{}", std::process::id(), unix_ms()));
        let sink = AuditSink::at_dir(dir.clone());
        sink.record(
            "auth.fail",
            None,
            None,
            None,
            AuditOutcome::Denied,
            Some("bearer token=super-secret"),
        );
        sink.record(
            "lease.acquire",
            Some("claude"),
            Some("cred-claude"),
            Some("dut-a"),
            AuditOutcome::Ok,
            Some("lease=lease-1"),
        );
        let day = day_key(unix_ms());
        let path = dir.join(format!("agent-access-{day}.jsonl"));
        let raw = std::fs::read_to_string(&path).expect("The jsonl should be written to disk.");
        assert!(
            !raw.contains("super-secret"),
            "The token must never be written to disk: {raw}"
        );
        assert!(
            raw.contains("lease-1"),
            "lease_id allows recording of records"
        );
        let mut lines = raw.lines();
        let first: AuditEvent = serde_json::from_str(lines.next().unwrap())
            .expect("The first line should contain valid JSONL format data.");
        assert_eq!(first.action, "auth.fail");
        assert_eq!(first.result, AuditOutcome::Denied);
        // Double hit of bearer + token: The entire value is erased (the first hit erases the
        // subsequent ones, with overlapping skipped)—The criterion for determination is that
        // "super-secret does not appear in any bytes written to disk."
        assert_eq!(first.detail.as_deref(), Some("bearer [REDACTED]"));
        let second: AuditEvent = serde_json::from_str(lines.next().unwrap())
            .expect("The next line should contain valid JSONL data.");
        assert_eq!(second.agent.as_deref(), Some("claude"));
        assert_eq!(second.credential_id.as_deref(), Some("cred-claude"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_ring_keeps_recent_in_order_and_caps() {
        let sink = AuditSink::memory();
        for i in 0..(RING_CAP + 50) {
            sink.record(
                "write.deny",
                Some("a"),
                None,
                Some(&format!("dut-{i}")),
                AuditOutcome::Denied,
                None,
            );
        }
        let recent = sink.recent(3);
        assert_eq!(recent.len(), 3);
        // Recent three items: dut-RING_CAP+47/48/49 (sorted in ascending order by time)
        assert_eq!(
            recent[2].dut.as_deref(),
            Some(format!("dut-{}", RING_CAP + 49).as_str())
        );
        assert_eq!(
            recent[0].dut.as_deref(),
            Some(format!("dut-{}", RING_CAP + 47).as_str())
        );
        // The number of rings cannot grow indefinitely.
        assert_eq!(sink.recent(usize::MAX).len(), RING_CAP);
    }

    #[test]
    fn day_key_matches_known_epoch() {
        // 2026-11-17 00:00:00 UTC = 1794873600s
        assert_eq!(day_key(1_794_873_600_000), "20261117");
        // 1970-01-01
        assert_eq!(day_key(0), "19700101");
    }
}
