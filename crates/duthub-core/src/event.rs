//! 设备层事件。
//!
//! 事件包含 `dut` 与单调 `seq`；UI 侧收到的必须是同一有界总线的顺序视图，
//! 队列满时上层会收到 `Lagged` 并从日志缓冲补读（M1.4 实现该语义）。

use serde::{Deserialize, Serialize};

use crate::backend::DutId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum AppEvent {
    SerialChunk {
        dut: DutId,
        session_id: String,
        seq: u64,
        bytes: Vec<u8>,
    },
    SerialDisconnected {
        dut: DutId,
        reason: String,
    },
    DutUpdated {
        dut: DutId,
    },
    AgentProgress {
        dut: DutId,
        stage: String,
        status: String,
    },
}
