//! Asynchronous JSONL transcript writer for Agent sessions.
//!
//! The ACP worker loop must never block on disk I/O while it drains events, so
//! every entry is handed to a dedicated writer thread. The thread serialises
//! entries in order and performs `write_all` + `flush` per line, preserving the
//! real-time durability of the previous synchronous [`super::session::TranscriptLog`].
//!
//! The 8 MiB file cap and write-failure notes are produced by the writer thread
//! itself and pushed back into the shared [`SessionView`] (once each), so the
//! caller does not need to inspect writer state.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::session::{FILE_TRANSCRIPT_LIMIT, SessionView, TranscriptEntry, TranscriptKind};

/// How long `finish`/`Drop` waits for the writer thread to drain its queue.
const WRITER_JOIN_TIMEOUT: Duration = Duration::from_secs(3);

enum WriterMessage {
    Entry(TranscriptEntry),
    Finish,
}

/// Handle to the background transcript writer.
pub struct TranscriptWriter {
    tx: Option<Sender<WriterMessage>>,
    join: Option<JoinHandle<()>>,
    view: Arc<SessionView>,
    dead_noted: Mutex<bool>,
}

impl std::fmt::Debug for TranscriptWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TranscriptWriter")
            .field("open", &self.tx.is_some())
            .finish_non_exhaustive()
    }
}

impl TranscriptWriter {
    /// Open (append) the transcript and start the writer thread.
    ///
    /// I/O failures never block the session: they are reported once through the
    /// shared view while the in-memory transcript keeps working.
    pub fn open(path: &Path, view: Arc<SessionView>) -> Self {
        let (tx, rx) = mpsc::channel();
        let writer_view = view.clone();
        let writer_path = path.to_path_buf();
        let join = std::thread::Builder::new()
            .name("duthub-transcript".into())
            .spawn(move || write_loop(&writer_path, &writer_view, rx))
            .ok();
        Self {
            tx: Some(tx),
            join,
            view,
            dead_noted: Mutex::new(false),
        }
    }

    /// Queue one entry; this never performs disk I/O on the caller thread.
    pub fn push(&self, entry: TranscriptEntry) {
        let Some(tx) = self.tx.as_ref() else {
            self.note_writer_gone();
            return;
        };
        if tx.send(WriterMessage::Entry(entry)).is_err() {
            self.note_writer_gone();
        }
    }

    /// Flush the queue and join the writer thread (bounded).
    pub fn finish(mut self) {
        self.close();
    }

    fn note_writer_gone(&self) {
        let mut noted = self.dead_noted.lock().unwrap();
        if *noted {
            return;
        }
        *noted = true;
        self.view.push_entry(TranscriptEntry::system(
            "transcript 写线程已停止，后续只保留内存记录",
        ));
    }

    fn close(&mut self) {
        if let Some(tx) = self.tx.take() {
            // A closed receiver still drains buffered messages, so the final
            // line of this session reaches the file before the thread exits.
            let _ = tx.send(WriterMessage::Finish);
        }
        let Some(join) = self.join.take() else {
            return;
        };
        let deadline = Instant::now() + WRITER_JOIN_TIMEOUT;
        while !join.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if join.is_finished() {
            let _ = join.join();
        }
    }
}

impl Drop for TranscriptWriter {
    fn drop(&mut self) {
        // Covers early-return error paths in the worker loop.
        self.close();
    }
}

fn write_loop(path: &Path, view: &SessionView, rx: Receiver<WriterMessage>) {
    let mut state = WriterFile::open(path);
    for message in rx {
        match message {
            WriterMessage::Entry(entry) => state.append(&entry, view),
            WriterMessage::Finish => break,
        }
    }
    state.flush();
}

struct WriterFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    capped: bool,
    cap_noted: bool,
    failure_noted: bool,
    open_error: Option<String>,
}

impl WriterFile {
    fn open(path: &Path) -> Self {
        let (file, open_error) = match OpenOptions::new().create(true).append(true).open(path) {
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
            cap_noted: written >= FILE_TRANSCRIPT_LIMIT,
            failure_noted: false,
            open_error,
        }
    }

    fn append(&mut self, entry: &TranscriptEntry, view: &SessionView) {
        if self.capped {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            let reason = self
                .open_error
                .clone()
                .unwrap_or_else(|| "transcript 文件未打开".into());
            self.note_failure(view, &reason);
            return;
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
            if !self.cap_noted {
                self.cap_noted = true;
                view.push_entry(TranscriptEntry::new(
                    TranscriptKind::System,
                    "transcript 文件已达 8 MiB 上限，后续不再写入文件",
                ));
            }
            return;
        }
        let result = file
            .write_all(bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush());
        match result {
            Ok(()) => self.written += bytes.len() as u64 + 1,
            Err(error) => {
                self.file = None;
                self.note_failure(view, &error.to_string());
            }
        }
    }

    fn note_failure(&mut self, view: &SessionView, reason: &str) {
        if self.failure_noted {
            return;
        }
        self.failure_noted = true;
        view.push_entry(TranscriptEntry::new(
            TranscriptKind::System,
            format!(
                "transcript 文件写入失败，后续只保留内存记录: {reason}（{}）",
                self.path.display()
            ),
        ));
    }

    fn flush(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "duthub-transcript-writer-{tag}-{}.jsonl",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn writes_jsonl_entries_without_blocking_the_caller() {
        let path = temp_file("jsonl");
        let view = Arc::new(SessionView::new());
        let writer = TranscriptWriter::open(&path, view.clone());
        writer.push(TranscriptEntry::new(TranscriptKind::User, "hello"));
        writer.push(TranscriptEntry::new(TranscriptKind::Agent, "world"));
        writer.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "user");
        assert_eq!(lines[0]["text"], "hello");
        assert_eq!(lines[1]["kind"], "agent");
        assert_eq!(lines[1]["text"], "world");
        // The writer only mirrors to disk; the caller owns memory ordering.
        assert_eq!(view.transcript_snapshot().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cap_note_is_written_back_once_by_the_writer_thread() {
        let path = temp_file("cap");
        let view = Arc::new(SessionView::new());
        let writer = TranscriptWriter::open(&path, view.clone());
        let payload = "x".repeat(64 * 1024);
        for _ in 0..130 {
            writer.push(TranscriptEntry::new(TranscriptKind::Agent, &payload));
        }
        writer.finish();
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.len() <= FILE_TRANSCRIPT_LIMIT);
        let notes = view
            .transcript_snapshot()
            .iter()
            .filter(|entry| entry.text.contains("8 MiB"))
            .count();
        assert_eq!(notes, 1, "上限注记只能由写线程回写一次");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_failure_is_noted_once_on_first_entry() {
        let path = temp_file("open-fail")
            .join("missing")
            .join("transcript.jsonl");
        let view = Arc::new(SessionView::new());
        let writer = TranscriptWriter::open(&path, view.clone());
        writer.push(TranscriptEntry::new(TranscriptKind::User, "one"));
        writer.push(TranscriptEntry::new(TranscriptKind::User, "two"));
        writer.finish();
        let notes = view
            .transcript_snapshot()
            .iter()
            .filter(|entry| entry.text.contains("transcript 文件写入失败"))
            .count();
        assert_eq!(notes, 1);
    }
}
