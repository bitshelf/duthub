//! 有界事件总线 + 按会话环形缓冲（M1.4）。
//!
//! - `publish`：事件写入会话环形缓冲，并广播给所有订阅者。
//! - 订阅者消费过慢会收到 `Lagged(n)`（tokio broadcast 语义），此时调用
//!   `read_log(session_id, since_seq, limit)` 从缓冲补齐，不丢内核 panic 数据
//!   （docs/API_CONTRACT.md 事件节）。
//! - `seq` 全局单调，由总线分配。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::broadcast;

use duthub_core::ipc::{ChunkStyle, EventEnvelope, UiEvent};

/// Per-session ring buffer capacity for non-screen events (chunks, notes, disconnects).
pub const SESSION_BUFFER_CAP: usize = 2000;

#[derive(Default)]
struct SessionBuffer {
    events: VecDeque<EventEnvelope>,
    /// Only the latest `UiEvent::SerialScreen` snapshot is retained so high-rate
    /// screen ticks never evict serial log lines from `events`.
    latest_screen: Option<EventEnvelope>,
    last_seq: u64,
}

impl SessionBuffer {
    fn push(&mut self, envelope: EventEnvelope) {
        self.last_seq = envelope.seq;
        if matches!(envelope.event, UiEvent::SerialScreen { .. }) {
            self.latest_screen = Some(envelope);
            return;
        }
        if self.events.len() >= SESSION_BUFFER_CAP {
            self.events.pop_front();
        }
        self.events.push_back(envelope);
    }

    fn read_since(&self, since_seq: u64, limit: usize) -> Vec<EventEnvelope> {
        let mut out: Vec<EventEnvelope> = self
            .events
            .iter()
            .filter(|e| e.seq > since_seq)
            .take(limit)
            .cloned()
            .collect();
        if let Some(screen) = self.latest_screen.as_ref().filter(|e| e.seq > since_seq)
            && out.len() < limit
        {
            let pos = out.partition_point(|e| e.seq < screen.seq);
            out.insert(pos, screen.clone());
        }
        out
    }

    fn read_tail(&self, limit: usize) -> Vec<EventEnvelope> {
        if limit == 0 {
            return Vec::new();
        }
        let mut out: Vec<EventEnvelope> = self
            .events
            .iter()
            .skip(self.events.len().saturating_sub(limit))
            .cloned()
            .collect();
        if let Some(screen) = self.latest_screen.as_ref() {
            let pos = out.partition_point(|e| e.seq < screen.seq);
            out.insert(pos, screen.clone());
            if out.len() > limit {
                out.remove(0);
            }
        }
        out
    }
}

pub struct EventBus {
    tx: broadcast::Sender<EventEnvelope>,
    buffers: Mutex<HashMap<String, SessionBuffer>>,
    next_seq: AtomicU64,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl EventBus {
    /// Create a bounded broadcast bus; slow subscribers receive `Lagged(n)` and
    /// recover from the per-session backlog via [`Self::read_log`].
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self {
            tx,
            buffers: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Mint a monotonically sequenced envelope, retain it in the session backlog,
    /// and broadcast it to live subscribers.
    pub fn publish(&self, session_id: &str, dut_id: &str, event: UiEvent) -> EventEnvelope {
        let mut buffers = self.buffers.lock().unwrap();
        let envelope = EventEnvelope {
            seq: self.next_seq(),
            ts_unix_ms: duthub_core::util::unix_ms(),
            dut_id: Some(dut_id.to_string()),
            session_id: Some(session_id.to_string()),
            event,
        };
        if let Some(buffer) = buffers.get_mut(session_id) {
            buffer.push(envelope.clone());
        } else {
            let mut buffer = SessionBuffer::default();
            buffer.push(envelope.clone());
            buffers.insert(session_id.to_string(), buffer);
        }
        let _ = self.tx.send(envelope.clone());
        envelope
    }

    /// Drop the backlog for a closed session.
    pub fn drop_session(&self, session_id: &str) {
        self.buffers.lock().unwrap().remove(session_id);
    }

    /// Expire a closed buffer only while its final sequence still matches.
    pub(crate) fn expire_closed_session(&self, session_id: &str, final_seq: u64) {
        let mut buffers = self.buffers.lock().unwrap();
        if buffers
            .get(session_id)
            .is_some_and(|buffer| buffer.last_seq == final_seq)
        {
            buffers.remove(session_id);
        }
    }

    /// Return up to `limit` buffered envelopes with `seq > since_seq`.
    pub fn read_log(&self, session_id: &str, since_seq: u64, limit: usize) -> Vec<EventEnvelope> {
        let buffers = self.buffers.lock().unwrap();
        buffers
            .get(session_id)
            .map(|buffer| buffer.read_since(since_seq, limit))
            .unwrap_or_default()
    }

    /// Clone only the newest envelopes, in ascending sequence order.
    pub(crate) fn read_log_tail(&self, session_id: &str, limit: usize) -> Vec<EventEnvelope> {
        let buffers = self.buffers.lock().unwrap();
        buffers
            .get(session_id)
            .map_or_else(Vec::new, |buffer| buffer.read_tail(limit))
    }

    pub fn read_serial_page(
        &self,
        session_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> (u64, Vec<EventEnvelope>) {
        let buffers = self.buffers.lock().unwrap();
        let buffer = buffers.get(session_id);
        let tail = buffer.map_or(0, |buffer| buffer.last_seq);
        let since = if since_seq > tail { 0 } else { since_seq };
        let events: Vec<_> = buffer
            .into_iter()
            .flat_map(|buffer| buffer.events.iter())
            .filter(|event| event.seq > since && matches!(event.event, UiEvent::SerialChunk { .. }))
            .take(limit)
            .cloned()
            .collect();
        (events.last().map_or(since, |event| event.seq), events)
    }

    /// Latest sequence number seen in the session backlog (`0` if absent).
    pub fn last_seq(&self, session_id: &str) -> u64 {
        let buffers = self.buffers.lock().unwrap();
        buffers.get(session_id).map_or(0, |buffer| buffer.last_seq)
    }
}

/// Convenience constructor for a `UiEvent::SerialChunk`.
pub fn chunk(text: impl Into<String>, style: ChunkStyle) -> UiEvent {
    UiEvent::SerialChunk {
        text: text.into(),
        style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn seq_is_monotonic_across_publish() {
        let bus = EventBus::new(64);
        let e1 = bus.publish("s1", "d1", chunk("a", ChunkStyle::Normal));
        let e2 = bus.publish("s1", "d1", chunk("b", ChunkStyle::Normal));
        assert_eq!(e1.seq + 1, e2.seq);
    }

    #[test]
    fn serial_screen_is_shared_by_the_backlog_and_broadcast() {
        let bus = EventBus::default();
        let mut receiver = bus.subscribe();
        let screen = Arc::new(duthub_core::ipc::ScreenSnapshot::default());
        bus.publish(
            "s1",
            "d1",
            UiEvent::SerialScreen {
                screen: screen.clone(),
            },
        );

        let backlog = bus.read_log("s1", 0, 1).pop().unwrap();
        let received = receiver.try_recv().unwrap();
        for envelope in [backlog, received] {
            let UiEvent::SerialScreen { screen: shared } = envelope.event else {
                panic!("expected a screen event");
            };
            assert!(Arc::ptr_eq(&screen, &shared));
        }
    }

    #[test]
    fn buffer_cap_evicts_oldest() {
        let bus = EventBus::new(4);
        for i in 0..(SESSION_BUFFER_CAP as u32 + 500) {
            bus.publish("s1", "d1", chunk(format!("line-{i}"), ChunkStyle::Normal));
        }
        let log = bus.read_log("s1", 0, usize::MAX);
        assert_eq!(log.len(), SESSION_BUFFER_CAP);
        let first_text = match &log[0].event {
            UiEvent::SerialChunk { text, .. } => text.clone(),
            _ => panic!("应为 SerialChunk"),
        };
        // 最老的 500 条已被淘汰：缓冲首行应是 line-500
        assert!(first_text.contains("line-500"));
    }

    #[test]
    fn closed_generation_cannot_expire_replacement_evidence() {
        let bus = EventBus::new(4);
        let old = bus.publish("sess-dut", "dut", chunk("old", ChunkStyle::Normal));
        bus.drop_session("sess-dut");
        let current = bus.publish("sess-dut", "dut", chunk("new", ChunkStyle::Normal));
        bus.expire_closed_session("sess-dut", old.seq);
        assert_eq!(bus.last_seq("sess-dut"), current.seq);
        bus.expire_closed_session("sess-dut", current.seq);
        assert_eq!(bus.last_seq("sess-dut"), 0);
    }

    #[test]
    fn log_tail_preserves_the_latest_sequence_order() {
        let bus = EventBus::new(4);
        let first = bus.publish("s1", "d1", chunk("old", ChunkStyle::Normal));
        bus.publish("s2", "d2", chunk("other session", ChunkStyle::Normal));
        let next = bus.publish("s1", "d1", chunk("next", ChunkStyle::Normal));
        let last = bus.publish("s1", "d1", chunk("last", ChunkStyle::Normal));
        let seqs: Vec<_> = bus
            .read_log_tail("s1", 2)
            .into_iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, [next.seq, last.seq]);
        assert_eq!(bus.read_log_tail("s1", usize::MAX)[0].seq, first.seq);
        assert!(bus.read_log_tail("s1", 0).is_empty());
        assert!(bus.read_log_tail("missing", 2).is_empty());
    }

    #[test]
    fn read_log_filters_since_seq() {
        let bus = EventBus::new(64);
        for i in 0..10 {
            bus.publish("s1", "d1", chunk(format!("l{i}"), ChunkStyle::Normal));
        }
        let last = bus.last_seq("s1");
        let tail = bus.read_log("s1", last - 3, usize::MAX);
        assert_eq!(tail.len(), 3);
    }

    #[test]
    fn slow_subscriber_gets_lagged_then_catches_up() {
        let bus = Arc::new(EventBus::new(4));
        let mut rx = bus.subscribe();
        // 快速生产 20 条，订阅者不消费 → broadcast 滞后
        for i in 0..20 {
            bus.publish("s1", "d1", chunk(format!("l{i}"), ChunkStyle::Normal));
        }
        let mut lagged = 0u64;
        let mut received = 0u32;
        loop {
            match rx.try_recv() {
                Ok(env) => {
                    received += 1;
                    let _ = env;
                }
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    lagged = n;
                    break;
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => panic!("总线不应关闭"),
            }
        }
        assert!(lagged >= 1, "慢消费者必须收到 Lagged");
        assert_eq!(received, 0, "Lagged 语义下旧事件不可靠，应走补读");
        // 滞后期间缓冲仍在 → 从缓冲补齐全部 20 条
        let catch_up = bus.read_log("s1", 0, usize::MAX);
        assert_eq!(catch_up.len(), 20);
        assert_eq!(
            catch_up[0].seq, 1,
            "缓冲补读从最初一条开始，不丢 panic 数据"
        );
    }

    #[test]
    fn serial_page_limits_chunks_and_recovers_a_cursor_past_tail() {
        let bus = EventBus::new(16);
        let first = bus.publish("s1", "dut", chunk("first", ChunkStyle::Normal));
        bus.publish("s2", "other", chunk("other DUT", ChunkStyle::Normal));
        bus.publish(
            "s1",
            "dut",
            UiEvent::SerialDisconnected {
                reason: "fixture".into(),
            },
        );
        let second = bus.publish("s1", "dut", chunk("second", ChunkStyle::Normal));
        let (cursor, chunks) = bus.read_serial_page("s1", 0, 1);
        assert_eq!(cursor, first.seq);
        assert_eq!(chunks.len(), 1);
        let (cursor, chunks) = bus.read_serial_page("s1", cursor, 1);
        assert_eq!(cursor, second.seq);
        assert_eq!(chunks.len(), 1);
        let (cursor, chunks) = bus.read_serial_page("s1", second.seq + 100, 1);
        assert_eq!(cursor, first.seq);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn drop_session_clears_buffer() {
        let bus = EventBus::new(8);
        bus.publish("s1", "d1", chunk("x", ChunkStyle::Normal));
        bus.drop_session("s1");
        assert!(bus.read_log("s1", 0, usize::MAX).is_empty());
        assert_eq!(bus.last_seq("s1"), 0);
    }
}
