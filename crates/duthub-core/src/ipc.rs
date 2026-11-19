//! UI ↔ 后端的 typed command/event 契约（自 `docs/API_CONTRACT.md` 收敛）。
//!
//! - 命令经 `AppService::handle` 同步分发，返回按序事件信封。
//! - `AppService::poll_events` 提供 Demo 事件源（M1.1 将替换为真实串口后端的事件流）。
//! - 事件信封携带单调 `seq` 与可选 `dut_id`/`session_id`（API_CONTRACT 事件语义）。
//! - 序列化形态是稳定契约：新增字段必须 default / 容忍未知字段。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::backend::DutId;

/// UI 发出的命令（M0 只含契约探针；M1 起按 API_CONTRACT 扩展串口命令）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum AppCommand {
    Ping,
    /// 请求后端版本信息（自检用，映射到 `UiEvent::VersionInfo`）。
    GetVersion,
    /// 向会话提交一行输入（Demo 回显；真实写入在 M1 接 SerialBackend）。
    SubmitLine {
        dut_id: String,
        session_id: String,
        text: String,
    },
}

/// 终端文本的展示中性标注；UI 负责映射到主题色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStyle {
    Normal,
    Dim,
    Good,
    Prompt,
    /// 内核错误行（panic/oops/failed）——UI 用红色突出
    Error,
    /// 分段标记行（`[mark] …`，日志分割）——UI 用分隔样式渲染
    Mark,
}

/// 终端网格行快照（PuTTY 模式仿真）：文本 + 前景/背景色。
/// `fg`/`bg` 为 0xRRGGBB；`None` = 无特殊色（UI 用主题默认前景/透明背景）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenRow {
    pub text: String,
    pub fg: Option<u32>,
    pub bg: Option<u32>,
}

/// 终端屏幕快照：real 会话的 Term 网格 → UI 渲染（每次网格变化整体替换）。
/// 光标以行索引 + 行内字符偏移给出（`None` = 光标隐藏）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSnapshot {
    pub rows: Vec<ScreenRow>,
    pub cursor_row: Option<u32>,
    pub cursor_char: Option<u32>,
    #[serde(default)]
    pub application_cursor: bool,
    #[serde(default)]
    pub bracketed_paste: bool,
}

/// 传给 UI 的事件体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UiEvent {
    SerialChunk {
        text: String,
        style: ChunkStyle,
    },
    /// real 会话终端网格快照（仿真器渲染源；不进 .log/serial_read 文本流）
    SerialScreen {
        screen: ScreenSnapshot,
    },
    SerialDisconnected {
        reason: String,
    },
    DutUpdated {
        dut: DutId,
    },
    VersionInfo {
        version: String,
        schema: u32,
    },
    Pong {
        seq_seen: u64,
    },
}

/// 事件信封：单调序号 + 会话归属 + 时间戳。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub seq: u64,
    pub ts_unix_ms: u64,
    #[serde(default)]
    pub dut_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub event: UiEvent,
}

/// 应用服务（M0 骨架）：命令分发与事件流的唯一入口，后续里程碑在此接入设备层。
#[derive(Debug, Default)]
pub struct AppService {
    next_seq: u64,
    demo_queue: VecDeque<(Option<String>, Option<String>, UiEvent)>,
}

impl AppService {
    pub fn new() -> Self {
        let mut svc = Self::default();
        svc.push_demo(
            None,
            None,
            UiEvent::SerialChunk {
                text: "duthub Console · RK3576".into(),
                style: ChunkStyle::Dim,
            },
        );
        svc.push_demo(
            None,
            None,
            UiEvent::SerialChunk {
                text: "Host wayland-pc · /dev/ttyUSB0 · 115200 baud · 8N1".into(),
                style: ChunkStyle::Dim,
            },
        );
        svc.push_demo(
            None,
            None,
            UiEvent::SerialChunk {
                text: "Linux version 6.1.118 (demo build)".into(),
                style: ChunkStyle::Normal,
            },
        );
        svc.push_demo(
            None,
            None,
            UiEvent::SerialChunk {
                text: "systemd[1]: Started Serial Getty.".into(),
                style: ChunkStyle::Normal,
            },
        );
        svc.push_demo(
            None,
            None,
            UiEvent::SerialChunk {
                text: "Network interface configured.".into(),
                style: ChunkStyle::Good,
            },
        );
        svc.push_demo(
            Some("rk3576-a".into()),
            Some("sess-rk3576-a".into()),
            UiEvent::SerialChunk {
                text: "RK3576 login: root".into(),
                style: ChunkStyle::Normal,
            },
        );
        svc.push_demo(
            Some("rk3576-a".into()),
            Some("sess-rk3576-a".into()),
            UiEvent::SerialChunk {
                text: "root@rk3576:~# ".into(),
                style: ChunkStyle::Prompt,
            },
        );
        svc
    }

    fn push_demo(&mut self, dut: Option<String>, session: Option<String>, event: UiEvent) {
        self.demo_queue.push_back((dut, session, event));
    }

    fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    fn envelope(
        &mut self,
        dut: Option<String>,
        session: Option<String>,
        event: UiEvent,
    ) -> EventEnvelope {
        EventEnvelope {
            seq: self.next_seq(),
            ts_unix_ms: now_unix_ms(),
            dut_id: dut,
            session_id: session,
            event,
        }
    }

    /// 抽取 Demo 事件流（最多 `max` 条），seq 与命令事件共享同一单调序列。
    pub fn poll_events(&mut self, max: usize) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        while out.len() < max {
            let Some((dut, session, event)) = self.demo_queue.pop_front() else {
                break;
            };
            out.push(self.envelope(dut, session, event));
        }
        out
    }

    /// 分发一条命令，返回按序产生的事件（可为空）。
    pub fn handle(&mut self, command: AppCommand) -> Vec<EventEnvelope> {
        match command {
            AppCommand::Ping => vec![self.envelope(
                None,
                None,
                UiEvent::Pong {
                    seq_seen: self.next_seq,
                },
            )],
            AppCommand::GetVersion => vec![self.envelope(
                None,
                None,
                UiEvent::VersionInfo {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    schema: 1,
                },
            )],
            AppCommand::SubmitLine {
                dut_id,
                session_id,
                text,
            } => {
                let prompt = format!("root@rk3576:~# {text}");
                let mut out = vec![self.envelope(
                    Some(dut_id),
                    Some(session_id),
                    UiEvent::SerialChunk {
                        text: prompt,
                        style: ChunkStyle::Prompt,
                    },
                )];
                out.push(self.envelope(
                    None,
                    None,
                    UiEvent::SerialChunk {
                        text: format!("Executed (demo): {text}"),
                        style: ChunkStyle::Normal,
                    },
                ));
                out
            }
        }
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

    #[test]
    fn version_command_envelope_is_monotonic() {
        let mut svc = AppService::new();
        let ping = svc.handle(AppCommand::Ping);
        let ver = svc.handle(AppCommand::GetVersion);
        assert_eq!(ping[0].seq, 1);
        assert_eq!(ver[0].seq, 2);
        assert!(
            matches!(&ver[0].event, UiEvent::VersionInfo { version, schema: 1 } if !version.is_empty())
        );
    }

    #[test]
    fn envelope_roundtrip_and_unknown_field_tolerance() {
        let mut svc = AppService::new();
        let env = &svc.handle(AppCommand::GetVersion)[0];
        let json = serde_json::to_string(env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(*env, back);

        let patched = format!(r#"{{"futureField":true,{}"#, &json[1..]);
        let tolerant: EventEnvelope = serde_json::from_str(&patched).unwrap();
        assert_eq!(*env, tolerant);
    }

    #[test]
    fn submit_line_prompt_style() {
        let mut svc = AppService::new();
        let out = svc.handle(AppCommand::SubmitLine {
            dut_id: "rk3576-a".into(),
            session_id: "sess-1".into(),
            text: "uname -a".into(),
        });
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].dut_id.as_deref(), Some("rk3576-a"));
        assert!(matches!(
            &out[0].event,
            UiEvent::SerialChunk {
                style: ChunkStyle::Prompt,
                ..
            }
        ));
    }

    #[test]
    fn demo_stream_is_monotonic_across_poll_and_command() {
        let mut svc = AppService::new();
        svc.handle(AppCommand::GetVersion); // seq 1
        let mut batch = svc.poll_events(usize::MAX);
        let mut last = 1;
        while !batch.is_empty() {
            for env in &batch {
                assert!(env.seq > last, "seq 必须严格递增");
                last = env.seq;
            }
            batch = svc.poll_events(usize::MAX);
        }
        assert!(last >= 7, "demo 脚本应至少产出 7 行");
        let after = svc.handle(AppCommand::Ping);
        assert!(after[0].seq > last, "命令事件接续 demo 序号");
    }
}
