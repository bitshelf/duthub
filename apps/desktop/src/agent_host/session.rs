//! 会话视图与 transcript 上限（W2）。
//!
//! 两条上限**同时**实现：
//! - 内存 transcript 最多 [`MEMORY_TRANSCRIPT_LIMIT`] 条，超出丢最旧；
//! - 文件 transcript 到 [`FILE_TRANSCRIPT_LIMIT`] 后停写，并在内存记一条 System
//!   注记（只记一次）。
//!
//! [`SessionView`] 用 `Arc` 共享；UI 每帧只 clone `Arc`，不重建/克隆整个 Vec。

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use duthub_sandbox::SandboxStatus;

pub const MEMORY_TRANSCRIPT_LIMIT: usize = 2000;
pub const FILE_TRANSCRIPT_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Stopped,
    Starting,
    Ready,
    Busy,
    Failed(String),
    Stopping,
}

impl SessionStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, SessionStatus::Stopped | SessionStatus::Failed(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptKind {
    User,
    Agent,
    Thought,
    Tool,
    System,
    Error,
}

impl TranscriptKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TranscriptKind::User => "user",
            TranscriptKind::Agent => "agent",
            TranscriptKind::Thought => "thought",
            TranscriptKind::Tool => "tool",
            TranscriptKind::System => "system",
            TranscriptKind::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub kind: TranscriptKind,
    pub text: String,
    pub at_unix_ms: u64,
}

impl TranscriptEntry {
    pub fn new(kind: TranscriptKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            at_unix_ms: now_unix_ms(),
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::new(TranscriptKind::System, text)
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self::new(TranscriptKind::Error, text)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug)]
pub struct SessionView {
    pub status: Mutex<SessionStatus>,
    pub session_id: Mutex<Option<String>>,
    pub port: Mutex<Option<u16>>,
    pub transcript: Arc<Mutex<std::collections::VecDeque<TranscriptEntry>>>,
    pub stderr_tail: Mutex<String>,
    /// Runtime credential id issued for this session's loopback listener.
    credential_id: Mutex<Option<String>>,
    /// MXC sandbox capability facts recorded by the worker (ADR-12).
    os_isolation: Mutex<Option<SandboxStatus>>,
    /// Total entries ever pushed (including entries later dropped by the
    /// memory cap), used by transcript diagnostics and tests.
    total_entries: AtomicU64,
}

impl Default for SessionView {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionView {
    pub fn new() -> Self {
        Self {
            status: Mutex::new(SessionStatus::Stopped),
            session_id: Mutex::new(None),
            port: Mutex::new(None),
            transcript: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            stderr_tail: Mutex::new(String::new()),
            credential_id: Mutex::new(None),
            os_isolation: Mutex::new(None),
            total_entries: AtomicU64::new(0),
        }
    }

    pub fn status(&self) -> SessionStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn set_status(&self, status: SessionStatus) {
        *self.status.lock().unwrap() = status;
    }

    /// 仅当当前状态等于 `expected` 时替换；UI 与工作线程竞争下的 CAS（F3）。
    pub fn try_transition(&self, expected: &SessionStatus, next: SessionStatus) -> bool {
        let mut status = self.status.lock().unwrap();
        if *status != *expected {
            return false;
        }
        *status = next;
        true
    }

    /// 仅当当前状态满足 `predicate` 时替换。用于「任意非终止态 → 终止态」
    /// 与「非终止态 → Stopping」；已是终止态时永不覆盖。
    pub fn try_transition_if(
        &self,
        predicate: impl FnOnce(&SessionStatus) -> bool,
        next: SessionStatus,
    ) -> bool {
        let mut status = self.status.lock().unwrap();
        if !predicate(&status) {
            return false;
        }
        *status = next;
        true
    }

    /// 工作线程失败兜底：只把非终止态改为 Failed，不覆盖已终止状态。
    /// 返回是否发生了转换（供事件回调去重）。
    pub fn fail(&self, message: impl Into<String>) -> bool {
        let message = message.into();
        self.try_transition_if(
            |status| !status.is_terminal(),
            SessionStatus::Failed(message),
        )
    }

    /// 正常退出：只把非终止态改为 Stopped；已 Failed 的会话保持 Failed。
    pub fn stop_normally(&self) -> bool {
        self.try_transition_if(|status| !status.is_terminal(), SessionStatus::Stopped)
    }

    /// Move a session that stopped after `Ready` back to `Starting` for an
    /// automatic restart (W2b).
    ///
    /// Only the `Stopped -> Starting` transition is performed; a `Failed`
    /// session (or one already being stopped) is never revived by this method.
    /// Returns whether the session can be restarted.
    pub fn reset_for_restart(&self) -> bool {
        self.try_transition(&SessionStatus::Stopped, SessionStatus::Starting)
    }

    /// MXC sandbox facts recorded for this session, if the worker has probed them.
    pub fn os_isolation(&self) -> Option<SandboxStatus> {
        self.os_isolation.lock().unwrap().clone()
    }

    pub fn set_os_isolation(&self, status: SandboxStatus) {
        *self.os_isolation.lock().unwrap() = Some(status);
    }

    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    pub fn set_session_id(&self, session_id: impl Into<String>) {
        *self.session_id.lock().unwrap() = Some(session_id.into());
    }

    pub fn port(&self) -> Option<u16> {
        *self.port.lock().unwrap()
    }

    pub fn set_port(&self, port: u16) {
        *self.port.lock().unwrap() = Some(port);
    }

    pub fn credential_id(&self) -> Option<String> {
        self.credential_id.lock().unwrap().clone()
    }

    pub fn set_credential_id(&self, credential_id: impl Into<String>) {
        *self.credential_id.lock().unwrap() = Some(credential_id.into());
    }

    /// Total entries ever pushed, including ones already dropped from the
    /// bounded in-memory transcript.
    pub fn total_entries(&self) -> u64 {
        self.total_entries.load(Ordering::Relaxed)
    }

    /// Append previously persisted transcript entries (e.g. after reloading a
    /// session) using the same memory cap as live entries.
    pub fn seed_history(&self, entries: impl IntoIterator<Item = TranscriptEntry>) {
        for entry in entries {
            self.push_entry(entry);
        }
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().unwrap().clone()
    }

    pub fn set_stderr_tail(&self, text: impl Into<String>) {
        *self.stderr_tail.lock().unwrap() = text.into();
    }

    /// 内存 transcript 快照（测试/导出；渲染层应直接锁 [`SessionView::transcript`]）。
    pub fn transcript_snapshot(&self) -> Vec<TranscriptEntry> {
        self.transcript.lock().unwrap().iter().cloned().collect()
    }

    /// 追加一条内存记录；超出 2000 条时丢最旧。
    pub fn push_entry(&self, entry: TranscriptEntry) {
        let mut entries = self.transcript.lock().unwrap();
        if entries.len() >= MEMORY_TRANSCRIPT_LIMIT {
            entries.pop_front();
        }
        entries.push_back(entry);
        self.total_entries.fetch_add(1, Ordering::Relaxed);
    }

    /// 追加内存 + 文件记录；文件到 8 MiB 后停写并只补一条 System 注记。
    pub fn push_with_log(&self, entry: TranscriptEntry, log: &mut TranscriptLog) {
        match log.append(&entry) {
            AppendOutcome::Written | AppendOutcome::Capped => {}
            AppendOutcome::Failed(error) if !log.failure_noted => {
                log.failure_noted = true;
                self.push_entry(TranscriptEntry::system(format!(
                    "transcript 文件写入失败，后续只保留内存记录: {error}"
                )));
            }
            AppendOutcome::Failed(_) => {}
        }
        self.push_entry(entry);
        if log.cap_noted {
            log.cap_noted = false;
            self.push_entry(TranscriptEntry::system(
                "transcript 文件已达 8 MiB 上限，后续不再写入文件",
            ));
        }
    }
}

#[derive(Debug)]
enum AppendOutcome {
    Written,
    Capped,
    Failed(String),
}

/// 逐行 JSONL transcript 写入器（仅工作线程使用）。
#[derive(Debug)]
pub struct TranscriptLog {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    capped: bool,
    cap_noted: bool,
    failure_noted: bool,
    open_error: Option<String>,
}

impl TranscriptLog {
    /// 打开（追加）transcript；I/O 失败降级为只记内存，不阻塞会话。
    pub fn open(path: &Path) -> Self {
        let (file, failure_note) = match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => (Some(file), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let written = file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map_or(0, |metadata| metadata.len());
        Self {
            path: path.to_path_buf(),
            file,
            written,
            capped: written >= FILE_TRANSCRIPT_LIMIT,
            // 已到上限的文件在第一次 push 时补一条 System 注记。
            cap_noted: written >= FILE_TRANSCRIPT_LIMIT,
            // F2：打开成功不代表后续写入不会失败；首次写入失败才置 true，
            // 否则失败注记永远不会出现、文件副本会静默停止增长。
            failure_noted: false,
            open_error: failure_note,
        }
    }

    /// 测试专用：用任意 `File`（如只读句柄）构造写入器，模拟打开成功后的写失败。
    #[cfg(test)]
    fn with_file_for_test(path: PathBuf, file: File, written: u64) -> Self {
        Self {
            path,
            file: Some(file),
            written,
            capped: false,
            cap_noted: false,
            failure_noted: false,
            open_error: None,
        }
    }

    fn append(&mut self, entry: &TranscriptEntry) -> AppendOutcome {
        if self.capped {
            return AppendOutcome::Capped;
        }
        let Some(file) = self.file.as_mut() else {
            return AppendOutcome::Failed(
                self.open_error
                    .clone()
                    .unwrap_or_else(|| "transcript 文件未打开".into()),
            );
        };
        let line = serde_json::json!({
            "kind": entry.kind.as_str(),
            "text": entry.text,
            "atUnixMs": entry.at_unix_ms,
        })
        .to_string();
        let bytes = line.as_bytes();
        if self.written + bytes.len() as u64 + 1 > FILE_TRANSCRIPT_LIMIT {
            self.capped = true;
            self.cap_noted = true;
            return AppendOutcome::Capped;
        }
        let result = file
            .write_all(bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush());
        match result {
            Ok(()) => {
                self.written += bytes.len() as u64 + 1;
                AppendOutcome::Written
            }
            Err(error) => {
                self.file = None;
                AppendOutcome::Failed(error.to_string())
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_capped(&self) -> bool {
        self.capped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "duthub-transcript-{tag}-{}.jsonl",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn memory_transcript_drops_oldest_at_limit() {
        let view = SessionView::new();
        for index in 0..=MEMORY_TRANSCRIPT_LIMIT {
            view.push_entry(TranscriptEntry::system(format!("entry-{index}")));
        }
        let entries = view.transcript_snapshot();
        assert_eq!(entries.len(), MEMORY_TRANSCRIPT_LIMIT);
        assert_eq!(entries.first().unwrap().text, "entry-1");
        assert_eq!(
            entries.last().unwrap().text,
            format!("entry-{MEMORY_TRANSCRIPT_LIMIT}")
        );
    }

    #[test]
    fn file_transcript_stops_at_cap_and_notes_once() {
        let path = temp_file("cap");
        let mut log = TranscriptLog::open(&path);
        let view = SessionView::new();
        let payload = "x".repeat(64 * 1024);
        for _ in 0..130 {
            view.push_with_log(
                TranscriptEntry::new(TranscriptKind::Agent, &payload),
                &mut log,
            );
        }
        assert!(log.is_capped());
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.len() <= FILE_TRANSCRIPT_LIMIT);
        let notes = view
            .transcript_snapshot()
            .iter()
            .filter(|entry| entry.text.contains("8 MiB"))
            .count();
        assert_eq!(notes, 1, "上限注记只能出现一次");

        // 重开一个已到上限的文件：第一次 push 也要且只补一条注记。
        drop(log);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(FILE_TRANSCRIPT_LIMIT)
            .unwrap();
        let mut reopened = TranscriptLog::open(&path);
        let reopened_view = SessionView::new();
        reopened_view.push_with_log(
            TranscriptEntry::new(TranscriptKind::User, "after-cap"),
            &mut reopened,
        );
        let reopened_notes = reopened_view
            .transcript_snapshot()
            .iter()
            .filter(|entry| entry.text.contains("8 MiB"))
            .count();
        assert_eq!(reopened_notes, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_transcript_writes_jsonl() {
        let path = temp_file("jsonl");
        let mut log = TranscriptLog::open(&path);
        let view = SessionView::new();
        view.push_with_log(
            TranscriptEntry::new(TranscriptKind::Agent, "hello"),
            &mut log,
        );
        drop(log);
        let text = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["kind"], "agent");
        assert_eq!(line["text"], "hello");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn total_entries_counts_dropped_entries_and_seed_history_respects_cap() {
        let view = SessionView::new();
        view.set_credential_id("cred-1");
        assert_eq!(view.credential_id().as_deref(), Some("cred-1"));
        for index in 0..=MEMORY_TRANSCRIPT_LIMIT {
            view.push_entry(TranscriptEntry::system(index.to_string()));
        }
        assert_eq!(
            view.total_entries(),
            MEMORY_TRANSCRIPT_LIMIT as u64 + 1,
            "total_entries 必须统计被内存上限丢弃的记录"
        );
        assert_eq!(view.transcript_snapshot().len(), MEMORY_TRANSCRIPT_LIMIT);

        let seeded = SessionView::new();
        seeded.seed_history((0..3).map(|index| TranscriptEntry::system(index.to_string())));
        assert_eq!(seeded.transcript_snapshot().len(), 3);
        assert_eq!(seeded.total_entries(), 3);
    }

    #[test]
    fn transition_is_compare_and_swap_and_terminal_is_sticky() {
        let view = SessionView::new();
        view.set_status(SessionStatus::Ready);
        assert!(view.try_transition(&SessionStatus::Ready, SessionStatus::Busy));
        assert!(!view.try_transition(&SessionStatus::Ready, SessionStatus::Busy));
        assert!(view.fail("boom"));
        assert!(!view.fail("again"), "终止态不得被再次覆盖");
        assert!(!view.stop_normally(), "Failed 不得被 Stopped 覆盖");
        assert!(matches!(view.status(), SessionStatus::Failed(message) if message == "boom"));

        assert!(!view.try_transition_if(|s| !s.is_terminal(), SessionStatus::Stopping));
        assert!(matches!(view.status(), SessionStatus::Failed(_)));
    }

    #[test]
    fn reset_for_restart_only_revives_stopped_sessions() {
        let view = SessionView::new();
        view.set_status(SessionStatus::Ready);
        assert!(!view.reset_for_restart(), "Ready 不是可重启的终止态");
        view.set_status(SessionStatus::Stopped);
        assert!(view.reset_for_restart());
        assert_eq!(view.status(), SessionStatus::Starting);
        assert!(!view.reset_for_restart(), "Starting 不得重复重置");
        view.set_status(SessionStatus::Failed("boom".into()));
        assert!(!view.reset_for_restart(), "Failed 必须保持终止");
        view.set_status(SessionStatus::Stopping);
        assert!(!view.reset_for_restart(), "Stopping 不得被改成 Starting");

        let status = SandboxStatus {
            backend: duthub_sandbox::SandboxBackendKind::ProcessContainer,
            tier: Some("base-container".into()),
            supported: true,
            read_deny: true,
            host_loopback: true,
            ui_isolation: true,
            ..SandboxStatus::default()
        };
        view.set_os_isolation(status.clone());
        assert_eq!(view.os_isolation(), Some(status));
    }

    #[test]
    fn write_failure_after_successful_open_notes_once() {
        let path = temp_file("write-fail");
        std::fs::write(&path, b"").unwrap();
        // 只读句柄：open 成功（不触发 F2 的旧 bug），但 write_all 必然失败。
        let read_only = File::open(&path).unwrap();
        let mut log = TranscriptLog::with_file_for_test(path.clone(), read_only, 0);
        assert!(!log.failure_noted);
        let view = SessionView::new();

        view.push_with_log(
            TranscriptEntry::new(TranscriptKind::Agent, "first"),
            &mut log,
        );
        assert!(log.failure_noted, "首次写失败必须置位并产生注记");
        let notes = view
            .transcript_snapshot()
            .iter()
            .filter(|entry| entry.text.contains("transcript 文件写入失败"))
            .count();
        assert_eq!(notes, 1, "失败注记只能出现一次");

        view.push_with_log(
            TranscriptEntry::new(TranscriptKind::Agent, "second"),
            &mut log,
        );
        let notes = view
            .transcript_snapshot()
            .iter()
            .filter(|entry| entry.text.contains("transcript 文件写入失败"))
            .count();
        assert_eq!(notes, 1, "后续失败不得重复注记（仅内存记录）");
        assert_eq!(view.transcript_snapshot().len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_failure_notes_once_on_first_push() {
        let path = temp_file("open-fail")
            .join("missing")
            .join("transcript.jsonl");
        let mut log = TranscriptLog::open(&path);
        let view = SessionView::new();
        view.push_with_log(TranscriptEntry::new(TranscriptKind::User, "one"), &mut log);
        view.push_with_log(TranscriptEntry::new(TranscriptKind::User, "two"), &mut log);
        let notes = view
            .transcript_snapshot()
            .iter()
            .filter(|entry| entry.text.contains("transcript 文件写入失败"))
            .count();
        assert_eq!(notes, 1);
    }
}
