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

/// 每会话缓冲容量（EventEnvelope 条数）。
pub const SESSION_BUFFER_CAP: usize = 2000;

pub struct EventBus {
    tx: broadcast::Sender<EventEnvelope>,
    buffers: Mutex<HashMap<String, VecDeque<EventEnvelope>>>,
    next_seq: AtomicU64,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl EventBus {
    /// `capacity` 为 broadcast 通道容量（慢消费者的滞后上限来源）。
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

    /// 生成信封（全局单调 seq），写入会话缓冲并广播。
    pub fn publish(&self, session_id: &str, dut_id: &str, event: UiEvent) -> EventEnvelope {
        let mut buffers = self.buffers.lock().unwrap();
        let envelope = EventEnvelope {
            seq: self.next_seq(),
            ts_unix_ms: now_unix_ms(),
            dut_id: Some(dut_id.to_string()),
            session_id: Some(session_id.to_string()),
            event,
        };
        let buffer = buffers.entry(session_id.to_string()).or_default();
        if buffer.len() >= SESSION_BUFFER_CAP {
            buffer.pop_front();
        }
        buffer.push_back(envelope.clone());
        // 无订阅者/通道满均不影响缓冲：send 结果被忽略（Err 仅在关闭或满时出现）
        let _ = self.tx.send(envelope.clone());
        envelope
    }

    /// 会话结束/清理时丢弃缓冲。
    pub fn drop_session(&self, session_id: &str) {
        self.buffers.lock().unwrap().remove(session_id);
    }

    /// 按会话补读：返回 seq > `since_seq` 的前 `limit` 条。
    pub fn read_log(&self, session_id: &str, since_seq: u64, limit: usize) -> Vec<EventEnvelope> {
        let buffers = self.buffers.lock().unwrap();
        buffers
            .get(session_id)
            .map(|buffer| {
                buffer
                    .iter()
                    .filter(|e| e.seq > since_seq)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn read_serial_page(
        &self,
        session_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> (u64, Vec<EventEnvelope>) {
        let buffers = self.buffers.lock().unwrap();
        let buffer = buffers.get(session_id);
        let tail = buffer
            .and_then(|buffer| buffer.back())
            .map_or(0, |event| event.seq);
        let since = if since_seq > tail { 0 } else { since_seq };
        let events: Vec<_> = buffer
            .into_iter()
            .flatten()
            .filter(|event| event.seq > since && matches!(event.event, UiEvent::SerialChunk { .. }))
            .take(limit)
            .cloned()
            .collect();
        (events.last().map_or(since, |event| event.seq), events)
    }

    /// 该会话缓冲中的最新 seq（无会话则 0）。
    pub fn last_seq(&self, session_id: &str) -> u64 {
        let buffers = self.buffers.lock().unwrap();
        buffers
            .get(session_id)
            .and_then(|b| b.back().map(|e| e.seq))
            .unwrap_or(0)
    }
}

/// 快捷构造一条 SerialChunk 事件（测试与 demo 共用）。
pub fn chunk(text: impl Into<String>, style: ChunkStyle) -> UiEvent {
    UiEvent::SerialChunk {
        text: text.into(),
        style,
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
