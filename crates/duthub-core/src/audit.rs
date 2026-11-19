//! 审计事件与 JSONL 落盘（`docs/SECURITY.md`）。
//!
//! - 事件命名空间：`auth.ok/auth.fail`、`grant.deny`、`lease.acquire/renew/release/
//!   expire/takeover/queue/cancel/protected.enter/protected.exit`、
//!   `write.admit/write.deny`、`control.execute/control.deny`、`approval.issue/consume`。
//! - 双通道：内存环（容量 1000，GUI 面板读取）+ 按日轮换 JSONL 文件
//!   `<工作目录>/audit/agent-access-YYYYMMDD.jsonl`（权威落盘）。
//! - **脱敏红线**：token/approval token/authorization 一律不落审计（含 detail
//!   自由文本，写入前经 [`redact_detail`] 强制擦除）；lease_id 是高熵句柄
//!   （SPEC §6.3：句柄 ≠ 权限）允许记录。
//! - **磁盘红线**：单日文件超过 [`MAX_AUDIT_FILE_BYTES`] 时轮转为新文件；
//!   轮转到 [`MAX_AUDIT_ROTATIONS`] 后停止当日落盘（内存环照常工作），
//!   避免长期运行的进程把审计目录写到磁盘耗尽。

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 内存环容量（GUI 面板展示最近 200 条，环留余量）。
const RING_CAP: usize = 1000;

/// 单个审计 JSONL 文件的字节上限（约 8 MiB）。
pub const MAX_AUDIT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// 单日最多轮转次数；达到上限后停止当日落盘（内存环不受影响）。
pub const MAX_AUDIT_ROTATIONS: u32 = 4;

/// 审计结果类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Ok,
    Denied,
    Error,
}

impl AuditOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditOutcome::Ok => "ok",
            AuditOutcome::Denied => "denied",
            AuditOutcome::Error => "error",
        }
    }
}

/// 单条审计事件（JSONL 行形状，camelCase 与 settings 家族一致）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    /// unix 毫秒时间戳
    pub ts_unix_ms: u64,
    /// 事件名（见模块注释命名空间）
    pub action: String,
    /// Agent 展示名（stdio 本地管理员为 `stdio-admin`，GUI 为 None）
    pub agent: Option<String>,
    /// 凭据稳定 id（非 token）
    pub credential_id: Option<String>,
    /// 目标 DUT（认证类事件为 None）
    pub dut: Option<String>,
    pub result: AuditOutcome,
    /// 自由文本摘要（写入前经脱敏）
    pub detail: Option<String>,
}

impl AuditEvent {
    pub fn to_jsonl(&self) -> String {
        // 单行压缩：jsonl 每行一个对象
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 脱敏自由文本：擦除敏感键后跟的值部分（大小写不敏感）。
///
/// 键名先做归一化（小写、去掉 `_`/`-`），再看是否精确等于
/// `authorization`/`bearer`，或以 `token`/`key`/`secret`/`password` 结尾。
/// 这样 `access_token`、`api-key`、`clientSecret`、`approvalToken` 等变体
/// 都会命中；`lease_id`、`credential_id` 等非敏感键保持原样（句柄 ≠ 权限）。
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
            continue; // 与上一段重叠（同值多键命中）
        }
        out.push_str(&text[cursor..s]);
        out.push_str("[REDACTED]");
        cursor = e;
    }
    out.push_str(&text[cursor..]);
    out
}

/// 扫描文本中的键候选：连续的 ASCII 字母数字、`_`、`-`。
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

/// 键名归一化后判定是否敏感（`Authorization`/`Bearer` 或后缀匹配）。
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

/// 给定键结束位置，定位"分隔符 + 引号 + 值"的值区段（字节索引，按 UTF-8 边界）。
/// 分隔符形态：`key=value` / `key: value` / JSON `"key":"value"`（闭引号在分隔符前）/
/// `key value`（Bearer 头风格，空格即分隔）。
fn value_range(text: &str, key_end: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = key_end;
    let mut saw_space = false;
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        saw_space = true;
        i += 1;
    }
    if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
        // JSON 键的闭引号：其后必须是 = 或 :
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || (bytes[i] != b'=' && bytes[i] != b':') {
            return None;
        }
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
    } else if i < bytes.len() && (bytes[i] == b'=' || bytes[i] == b':') {
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
    } else if !saw_space {
        return None;
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

/// 审计接收端：内存环 + 按日 JSONL 文件（`dir` 为 None 时仅内存环，测试用）。
pub struct AuditSink {
    ring: Mutex<VecDeque<AuditEvent>>,
    dir: Option<PathBuf>,
    /// 已打开的当日文件句柄缓存（键 = YYYYMMDD）；跨日或超限时轮换重开。
    day_file: Mutex<Option<DayFile>>,
}

/// 当日审计文件状态（字节计数驱动轮转，句柄为 `None` 表示已停写）。
struct DayFile {
    day: String,
    file: Option<std::fs::File>,
    bytes: u64,
    rotations: u32,
    /// 达到轮转上限或写失败后停止当日落盘，避免无限增长。
    stopped: bool,
}

impl DayFile {
    /// 关闭当前句柄，把当日文件改名为 `agent-access-YYYYMMDD.rN.jsonl`，
    /// 再新建空文件继续写。失败时停止当日落盘（不越过体积上限）。
    fn rotate(&mut self, dir: &Path) -> Result<(), String> {
        if self.rotations >= MAX_AUDIT_ROTATIONS {
            self.stopped = true;
            return Err(format!(
                "审计文件达到轮转上限 {MAX_AUDIT_ROTATIONS}，停止当日落盘"
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
        // Windows 要求先关闭句柄再改名。
        if let Err(err) = std::fs::rename(&base, &rotated) {
            self.stopped = true;
            return Err(format!(
                "审计文件轮转失败 {} -> {}: {err}",
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
                Err(format!("轮转后审计文件打开失败 {}: {err}", base.display()))
            }
        }
    }
}

/// 打开当日文件并统计已存在的轮转分片数（进程重启后仍能延续上限）。
fn open_day_file(dir: &Path, day: &str) -> Result<DayFile, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("审计目录创建失败: {err}"))?;
    let path = dir.join(format!("agent-access-{day}.jsonl"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("审计文件打开失败 {}: {err}", path.display()))?;
    let bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    Ok(DayFile {
        day: day.to_string(),
        file: Some(file),
        bytes,
        rotations: count_rotations(dir, day),
        stopped: false,
    })
}

/// 统计 `agent-access-YYYYMMDD.rN.jsonl` 分片数量。
fn count_rotations(dir: &Path, day: &str) -> u32 {
    let prefix = format!("agent-access-{day}.r");
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with(&prefix) && name.ends_with(".jsonl")
                })
                .count() as u32
        })
        .unwrap_or(0)
}

impl AuditSink {
    /// 内存环模式（测试 / 无工作目录场景）。
    pub fn memory() -> Arc<Self> {
        Arc::new(Self {
            ring: Mutex::new(VecDeque::new()),
            dir: None,
            day_file: Mutex::new(None),
        })
    }

    /// 落盘模式：`<dir>/agent-access-YYYYMMDD.jsonl` 按日轮换（目录即工作目录 `audit/`）。
    pub fn at_dir(dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            ring: Mutex::new(VecDeque::new()),
            dir: Some(dir),
            day_file: Mutex::new(None),
        })
    }

    /// 记录一条审计事件（脱敏 → 内存环 → tracing → 文件）。全程 best-effort：
    /// 审计失败不阻断业务操作，仅 tracing::warn。
    #[allow(clippy::too_many_arguments)]
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
            let mut ring = self.ring.lock().unwrap();
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
        let mut guard = self.day_file.lock().unwrap();
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
            tracing::warn!(target: "audit", "单条审计事件超过文件上限，停止当日落盘");
            return;
        }
        if let Some(file) = state.file.as_mut() {
            if let Err(err) = file.write_all(line.as_bytes()) {
                tracing::warn!(target: "audit", "审计写入失败: {err}");
                return;
            }
            state.bytes += line_bytes;
        }
    }

    /// 内存环最近 `limit` 条（时间升序，GUI 面板数据源）。
    pub fn recent(&self, limit: usize) -> Vec<AuditEvent> {
        let ring = self.ring.lock().unwrap();
        ring.iter().rev().take(limit).rev().cloned().collect()
    }
}

fn day_key(ts_unix_ms: u64) -> String {
    // 天数 → YYYYMMDD（civil from days 算法，无 chrono 依赖）
    let days = (ts_unix_ms / 86_400_000) as i64;
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
        // JSON 形态：键的闭引号在分隔符之前；值引号保留，内容替换
        assert_eq!(
            redact_detail(r#"{"approvalToken":"apr-xyz","dut":"d1"}"#),
            r#"{"approvalToken":"[REDACTED]","dut":"d1"}"#
        );
        // authorization 与 bearer 双重命中（纵深防御）
        assert_eq!(
            redact_detail("Authorization: Bearer sk-live-999"),
            "Authorization: [REDACTED] [REDACTED]"
        );
        // 大小写不敏感
        assert_eq!(redact_detail("TOKEN = leak"), "TOKEN = [REDACTED]");
        // lease_id 允许保留（句柄 ≠ 权限）
        assert_eq!(
            redact_detail("lease=lease-abc token=zzz"),
            "lease=lease-abc token=[REDACTED]"
        );
        // 词边界：非该键的组合不应误伤
        assert_eq!(
            redact_detail("credential_id=cred-1"),
            "credential_id=cred-1"
        );
        // 末尾无分隔符
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
        let raw = std::fs::read_to_string(&path).expect("jsonl 应落盘");
        assert!(!raw.contains("super-secret"), "token 绝不能落盘: {raw}");
        assert!(raw.contains("lease-1"), "lease_id 允许记录");
        let mut lines = raw.lines();
        let first: AuditEvent =
            serde_json::from_str(lines.next().unwrap()).expect("首行应为合法 JSONL");
        assert_eq!(first.action, "auth.fail");
        assert_eq!(first.result, AuditOutcome::Denied);
        // bearer + token 双重命中：值整体被擦（第一个命中吞掉后续，重叠跳过）——
        // 判定标准是「super-secret 不出现在任何落盘字节里」
        assert_eq!(first.detail.as_deref(), Some("bearer [REDACTED]"));
        let second: AuditEvent =
            serde_json::from_str(lines.next().unwrap()).expect("次行应为合法 JSONL");
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
        // 最近三条：dut-RING_CAP+47/48/49（时间升序）
        assert_eq!(
            recent[2].dut.as_deref(),
            Some(format!("dut-{}", RING_CAP + 49).as_str())
        );
        assert_eq!(
            recent[0].dut.as_deref(),
            Some(format!("dut-{}", RING_CAP + 47).as_str())
        );
        // 环封顶不无限增长
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
