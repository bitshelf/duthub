//! 会话驱动抽象 + Demo 驱动（M1.1）。
//!
//! Demo 驱动回放 `tests/fixture-logs/` 样本（正常启动 + 内核 panic），随后进入
//! 交互式模拟 shell（移植原型 `response()` 行为），无硬件即可驱动全链路。
//! 真实驱动（M1.2）实现同一命令通道，走 tokio-serial + alacritty_terminal 仿真
//! （行重组/本地回显已废弃：回显与提示符由设备 shell 经仿真器渲染）。

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use std::time::Duration;

use tokio::sync::Notify;

use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::ipc::{ChunkStyle, EventEnvelope, UiEvent};
use duthub_core::lease::{LeaseAuthority, WriteCtx};

use crate::bus::EventBus;
use crate::logs::SessionLogWriter;
use crate::session::LineEnding;

/// 会话命令（单个 task 独占串口句柄，读写都进该 task，docs/ARCHITECTURE.md）。
pub enum SessionCommand {
    /// 用户输入的一行（已含或不含换行均可）。`lease` 为入口 admit 捕获的
    /// 租约上下文（None = 无租约要求路径：auth 关的 Agent 过渡写 / System）。
    Write {
        text: String,
        lease: Option<WriteCtx>,
    },
    /// 一次完整的多行心跳探测，保留原始脚本用于演示驱动。
    WriteHeartbeatBatch {
        text: String,
        batch: std::sync::Arc<crate::heartbeat::HeartbeatBatch>,
        lease: Option<WriteCtx>,
    },
    /// 原始字节直写（PuTTY 键盘路径：typed_char/Enter \r/Backspace 0x7f），
    /// 不做行尾追加、不发 [TX] 回显
    WriteBytes {
        bytes: Vec<u8>,
        lease: Option<WriteCtx>,
    },
    /// 重设波特率（真实驱动：断开重连协调）
    SetBaud(u32),
    /// 终端网格重设尺寸（窗口 resize → term.resize(cols, rows)）
    Resize { cols: usize, rows: usize },
    /// 重置本地终端仿真器。
    ResetTerminal,
    /// 会话级行尾策略（发送路径生效，见 LineEnding；仅 write_line 路径）
    SetEnding(LineEnding),
    /// 分段标记（日志分割）：写 `[mark] {label}` 行（Mark 样式），文本与索引同步
    Mark(String),
    /// 请求关闭
    Close,
}

/// 发射器：原始字节+文本行 → 日志管道；事件 → 总线 + 会话缓冲。
pub struct SessionCtx {
    pub dut_id: String,
    pub session_id: String,
    bus: Arc<EventBus>,
    logs: Option<Arc<Mutex<SessionLogWriter>>>,
    reconnect_notify: Option<std::sync::Arc<Notify>>,
    /// 租约复核（[E] actor 复核用；None = 无租约要求路径）
    lease_authority: Option<Arc<dyn LeaseAuthority>>,
    /// 审计（actor 拒绝写入记 write.deny）
    audit: Option<Arc<AuditSink>>,
}

impl SessionCtx {
    pub async fn begin_actor_io(&self) -> Option<tokio::sync::RwLockReadGuard<'_, ()>> {
        match self.lease_authority.as_ref() {
            Some(authority) => Some(authority.serial_io().read().await),
            None => None,
        }
    }
    pub fn new(dut_id: &str, session_id: &str, bus: Arc<EventBus>) -> Self {
        Self {
            dut_id: dut_id.to_string(),
            session_id: session_id.to_string(),
            bus,
            logs: None,
            reconnect_notify: None,
            lease_authority: None,
            audit: None,
        }
    }

    /// 挂接日志管道（原始字节副本 + utf8-lossy 文本，M1.5）。
    pub fn with_logs(mut self, logs: Arc<Mutex<SessionLogWriter>>) -> Self {
        self.logs = Some(logs);
        self
    }

    /// 挂接手动重连唤醒（H1.4：退避等待期间可被提前唤醒）。
    pub fn with_reconnect_notify(mut self, notify: std::sync::Arc<Notify>) -> Self {
        self.reconnect_notify = Some(notify);
        self
    }

    /// 挂接租约复核（[E] actor 复核：SPEC §6.5.1 唯一 Actor 写前验证代数）。
    pub fn with_lease_authority(mut self, authority: Option<Arc<dyn LeaseAuthority>>) -> Self {
        self.lease_authority = authority;
        self
    }

    /// 挂接审计接收端。
    pub fn with_audit(mut self, audit: Option<Arc<AuditSink>>) -> Self {
        self.audit = audit;
        self
    }

    /// [E] actor 复核：命令携带租约上下文时，写串口前最后一刻验证。
    /// 失败 → 注记行（Error 样式）+ 审计 write.deny + 返回 false（调用方丢弃写入）。
    pub fn admit_actor_write(&self, lease: &Option<WriteCtx>) -> bool {
        let Some(ctx) = lease else {
            return true; // 无租约要求路径（既有测试 / 过渡放行）
        };
        let Some(authority) = &self.lease_authority else {
            return true;
        };
        match authority.validate_ctx(&self.dut_id, ctx) {
            Ok(()) => true,
            Err(err) => {
                let note = format!("[lease] 过期写入已拦截：{}", err.message());
                self.emit_text(&note, ChunkStyle::Error);
                if let Some(audit) = &self.audit {
                    audit.record(
                        "write.deny",
                        None,
                        None,
                        Some(&self.dut_id),
                        AuditOutcome::Denied,
                        Some(&format!(
                            "actor 复核拒绝 lease={} gen={}：{}",
                            ctx.lease_id,
                            ctx.generation,
                            err.message()
                        )),
                    );
                }
                false
            }
        }
    }

    fn write_log(&self, line: &str) {
        if let Some(logs) = &self.logs
            && let Ok(mut writer) = logs.lock()
        {
            let _ = writer.note(line);
        }
    }

    pub fn emit_text(&self, text: &str, style: ChunkStyle) -> EventEnvelope {
        self.write_log(text);
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(text, style),
        )
    }

    pub fn emit_disconnected(&self, reason: &str) -> EventEnvelope {
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            UiEvent::SerialDisconnected {
                reason: reason.to_string(),
            },
        )
    }

    /// 发送回显（H1.1，demo 驱动专用）：TX 标记行进终端流（Good 配色区分收发）；
    /// .bin 原始日志记真实线上字节（含行尾），.log 文本记标记行。
    pub fn emit_tx(&self, wire_payload: &str, display_text: &str) {
        let display = single_line_tx(display_text);
        if let Some(logs) = &self.logs
            && let Ok(mut writer) = logs.lock()
        {
            let _ = writer.append(wire_payload.as_bytes(), &format!("[TX] {display}"));
        }
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(format!("[TX] {display}"), ChunkStyle::Good),
        );
    }

    /// TX 仅审计（real 驱动 write_line 路径）：.bin 记真实线上字节、.log 记
    /// `[TX]` 标记行，但不再进终端流/总线（PuTTY 行为：无本地回显，屏幕内容
    /// 由设备 shell 经仿真器渲染）。
    pub fn log_tx(&self, wire_payload: &[u8], display_text: &str) {
        let display = single_line_tx(display_text);
        if let Some(logs) = &self.logs
            && let Ok(mut writer) = logs.lock()
        {
            let _ = writer.append(wire_payload, &format!("[TX] {display}"));
        }
    }

    /// RX 原始字节块：.bin 记真字节（不转码），.log 记 utf8-lossy 文本，总线发
    /// SerialChunk（MCP serial_read / 录制 / 审计的文本来源；终端 UI 渲染走
    /// SerialScreen 网格快照，不重复显示该文本）。
    pub fn emit_raw(&self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).to_string();
        if let Some(logs) = &self.logs
            && let Ok(mut writer) = logs.lock()
        {
            let _ = writer.append(bytes, &text);
        }
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(text, ChunkStyle::Normal),
        );
    }

    /// 键入字节审计（real 驱动 WriteBytes 路径）：只进 .bin 原始流；.log 与终端流
    /// 由随后的设备回显承担（避免逐键 [TX] 刷屏）。
    pub fn log_keystrokes(&self, bytes: &[u8]) {
        if let Some(logs) = &self.logs
            && let Ok(mut writer) = logs.lock()
        {
            let _ = writer.append_raw(bytes);
        }
    }

    /// 终端网格快照 → 总线（real 驱动专用；UI 整体替换渲染源）。
    pub fn emit_screen(&self, screen: &duthub_core::ipc::ScreenSnapshot) {
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            UiEvent::SerialScreen {
                screen: screen.clone(),
            },
        );
    }

    /// 分段标记（日志分割）：`[mark] {label}` 行进终端流（Mark 样式），
    /// 文本与原始日志同步写入（标记是本机注记，非线上字节）。
    pub fn emit_mark(&self, label: &str) {
        let line = format!("[mark] {label}");
        let split_error = self.logs.as_ref().and_then(|logs| {
            let mut writer = logs.lock().ok()?;
            writer.mark(&line).err()
        });
        self.bus.publish(
            &self.session_id,
            &self.dut_id,
            crate::bus::chunk(line, ChunkStyle::Mark),
        );
        if let Some(err) = split_error {
            self.emit_text(&format!("[serial] 日志分段失败: {err}"), ChunkStyle::Error);
        }
    }

    /// 断线重连退避等待（H1.4）：定时到期或手动重连通知（先到先返）。
    pub async fn reconnect_wait(&self, backoff: Duration) {
        match &self.reconnect_notify {
            Some(notify) => {
                let notified = notify.notified();
                tokio::pin!(notified);
                tokio::select! {
                    _ = &mut notified => {}
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
            None => tokio::time::sleep(backoff).await,
        }
    }
}

/// [TX] 注记单行化（P2-034）：display_text 中的换行转义为字面 `\r` / `\n`
/// 两个字符，保证 .log 与终端流不出现无前缀续行——按行解析 .log 的下游工具
/// 不会把多行写入的后续行误读为设备输出。wire 原始字节不经此处理（.bin 保留
/// 真实线上字节）。
fn single_line_tx(text: &str) -> String {
    text.replace('\r', "\\r").replace('\n', "\\n")
}

/// 内核行分类：错误行（panic/oops/BUG/failed）标红、prompt 标亮，其余常规。
pub fn classify_line(text: &str) -> ChunkStyle {
    let lower = text.to_ascii_lowercase();
    if lower.contains("panic")
        || lower.contains("oops")
        || lower.contains("bug:")
        || lower.contains("call trace")
        || lower.contains("error")
        || lower.contains("failed")
    {
        return ChunkStyle::Error;
    }
    let trimmed_end = text.trim_end();
    if trimmed_end.ends_with('#') || trimmed_end.ends_with('$') || trimmed_end.ends_with("login:") {
        return ChunkStyle::Prompt;
    }
    ChunkStyle::Normal
}

// NOTE：手写 LineAssembler（\n 分行 + 半行冲刷）已随终端仿真重构移除 ——
// real 会话字节流直接进 alacritty_terminal 仿真器，回显/提示符/光标由设备
// shell 负责（PuTTY 行为），文本流按读取块原样进日志管道。

// ---------------------------------------------------------------------------
// Demo 驱动
// ---------------------------------------------------------------------------

const BOOT_FIXTURE: &str = include_str!("../../../tests/fixture-logs/rk3576-boot.log");
const PANIC_FIXTURE: &str = include_str!("../../../tests/fixture-logs/kernel-panic.log");

/// fixture 文本内容（panic 样本供测试与“模拟 panic”命令使用）。
pub fn boot_fixture() -> Vec<String> {
    BOOT_FIXTURE.lines().map(str::to_string).collect()
}

pub fn panic_fixture() -> Vec<String> {
    PANIC_FIXTURE.lines().map(str::to_string).collect()
}

/// Demo 驱动：回放启动日志 → 交互 shell → Close/取消退出。
pub async fn run_demo_session(
    ctx: SessionCtx,
    mut cmd_rx: mpsc::Receiver<SessionCommand>,
    line_delay_ms: u64,
) {
    let mut ending = LineEnding::default();
    for line in boot_fixture() {
        ctx.emit_text(&line, classify_line(&line));
        tokio::time::sleep(std::time::Duration::from_millis(line_delay_ms)).await;
    }
    ctx.emit_text("RK3576 login: root", ChunkStyle::Normal);
    ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCommand::Write { text, lease } => {
                // [E] actor 复核（SPEC §6.5.1）：写前最后一刻验证租约代数；
                // 过期/被接管 → 丢弃写入（注记 + 审计），不触串口
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                // 发送回显与真实驱动一致：Good 配色 + [TX] 标记（演示 shell 的
                // 提示符行随后模拟设备侧本地回显）
                ctx.emit_tx(&ending.apply(&text), &text);
                let command = text.trim();
                if command.is_empty() {
                    ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
                    continue;
                }
                ctx.emit_text(&format!("root@rk3576:~# {command}"), ChunkStyle::Prompt);
                emit_demo_response(&ctx, command);
                ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
            }
            SessionCommand::WriteHeartbeatBatch { text, batch, lease } => {
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                ctx.emit_tx(&ending.apply(&text), &text);
                ctx.emit_text(&batch.begin_marker(), ChunkStyle::Normal);
                for command in batch
                    .script
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                {
                    emit_demo_response(&ctx, command);
                }
                ctx.emit_text(&batch.end_marker(), ChunkStyle::Normal);
            }
            SessionCommand::SetBaud(_) => {
                ctx.emit_text("(demo) 波特率变更已模拟", ChunkStyle::Dim);
            }
            SessionCommand::Mark(label) => {
                ctx.emit_mark(&label);
            }
            SessionCommand::SetEnding(new_ending) => {
                ending = new_ending;
                ctx.emit_text(
                    &format!("(demo) 行尾策略 → {}", new_ending.label()),
                    ChunkStyle::Dim,
                );
            }
            SessionCommand::WriteBytes { bytes, lease } => {
                // [E] actor 复核（同 Write 分支）
                if !ctx.admit_actor_write(&lease) {
                    continue;
                }
                // demo 无仿真器：按键字节按 utf8-lossy 走通用回显路径（行为可见即可）
                let command = String::from_utf8_lossy(&bytes).trim().to_string();
                if !command.is_empty() {
                    ctx.emit_text(&format!("Executed (demo): {command}"), ChunkStyle::Normal);
                }
                ctx.emit_text("root@rk3576:~# ", ChunkStyle::Prompt);
            }
            SessionCommand::Resize { .. } | SessionCommand::ResetTerminal => {
                // demo 无终端网格，忽略
            }
            SessionCommand::Close => break,
        }
    }
    ctx.emit_disconnected("session closed");
}

fn emit_demo_response(ctx: &SessionCtx, command: &str) {
    match command {
        "pwd" => {
            ctx.emit_text("/root", ChunkStyle::Normal);
        }
        "whoami" => {
            ctx.emit_text("root", ChunkStyle::Normal);
        }
        "uname -a" => {
            ctx.emit_text(
                "Linux rk3576 6.1.118 #1 SMP PREEMPT aarch64 GNU/Linux",
                ChunkStyle::Normal,
            );
        }
        "ls" => {
            ctx.emit_text(
                "bin  boot  dev  etc  home  lib  media  mnt  opt  proc  root  run  sys  tmp  usr  var",
                ChunkStyle::Normal,
            );
        }
        "uptime" => {
            ctx.emit_text(
                " 00:12:40 up 4 days,  2 users,  load average: 0.05, 0.07, 0.06",
                ChunkStyle::Normal,
            );
        }
        c if c.starts_with("dmesg") => {
            for line in panic_fixture() {
                ctx.emit_text(&line, classify_line(&line));
            }
        }
        "clear" => {
            // UI 端 clear 在 M1.6 由本地处理；demo 只给提示。
            ctx.emit_text("(demo) 屏幕已清空（本地终端处理 clear）", ChunkStyle::Dim);
        }
        "help" => {
            ctx.emit_text(
                "Demo commands: uname -a, whoami, pwd, ls, uptime, dmesg, clear",
                ChunkStyle::Dim,
            );
        }
        "reboot" => {
            ctx.emit_text("Reboot requested (simulation only)", ChunkStyle::Normal);
        }
        other => {
            ctx.emit_text(&format!("Executed (demo): {other}"), ChunkStyle::Normal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_flags_kernel_errors() {
        assert_eq!(
            classify_line("Kernel panic - not syncing: Fatal"),
            ChunkStyle::Error
        );
        assert_eq!(classify_line("[   42.90]Oops: 96000047"), ChunkStyle::Error);
        assert_eq!(
            classify_line("[   3.512] systemd[1]: Started Serial Getty."),
            ChunkStyle::Normal
        );
        assert_eq!(classify_line("root@rk3576:~# "), ChunkStyle::Prompt);
    }

    #[tokio::test]
    async fn demo_session_replays_boot_and_answers_uname() {
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let ctx = SessionCtx::new("dut-demo", "sess-demo", bus.clone());
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        // 等启动日志播完
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        tx.send(SessionCommand::Write {
            text: "uname -a\n".into(),
            lease: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let log: Vec<String> = bus
            .read_log("sess-demo", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let boot_len = boot_fixture().len();
        assert!(
            log.len() >= boot_len + 3,
            "启动行 + 输入回显 + 响应 + 提示符"
        );
        assert!(log.iter().any(|l| l.contains("aarch64 GNU/Linux")));
        assert!(log.iter().any(|l| l.ends_with("root@rk3576:~# uname -a")));
        drop(tx);
        let _ = handle.await;
        // 关闭后最后一条应是 Disconnected，且不再增长
        let after_close = bus.last_seq("sess-demo");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(after_close, bus.last_seq("sess-demo"), "退出后不应再有事件");
    }

    #[tokio::test]
    async fn write_produces_tx_echo_and_setending_takes_effect() {
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let ctx = SessionCtx::new("dut-tx", "sess-tx", bus.clone());
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        tx.send(SessionCommand::Write {
            text: "echo-a".into(),
            lease: None,
        })
        .await
        .unwrap();
        tx.send(SessionCommand::SetEnding(LineEnding::Crlf))
            .await
            .unwrap();
        tx.send(SessionCommand::Write {
            text: "echo-b".into(),
            lease: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let lines: Vec<String> = bus
            .read_log("sess-tx", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|l| l == "[TX] echo-a"),
            "写入必须有 TX 回显行，实际 {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "[TX] echo-b"),
            "SetEnding 后的写入仍要有 TX 回显"
        );
        drop(tx);
        let _ = handle.await;
    }

    /// [E] actor 复核（SPEC §6.5.1）：命令携带过期/不存在的租约上下文 →
    /// 写入在串口前丢弃 + 注记行，绝不产生 TX 回显（INV-04 actor 侧）。
    #[tokio::test]
    async fn actor_recheck_drops_stale_generation_write() {
        use duthub_core::audit::AuditSink;
        use duthub_core::lease::{ControlLeaseManager, WriteCtx};
        let bus = Arc::new(EventBus::new(256));
        let (tx, rx) = mpsc::channel(8);
        let authority: Arc<dyn LeaseAuthority> =
            Arc::new(ControlLeaseManager::new(Some(AuditSink::memory())));
        let audit = AuditSink::memory();
        let ctx = SessionCtx::new("dut-e", "sess-e", bus.clone())
            .with_lease_authority(Some(authority.clone()))
            .with_audit(Some(audit.clone()));
        let handle = tokio::spawn(run_demo_session(ctx, rx, 0));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        // 无人持有该租约（管理器内不存在）：句柄任意 → validate 拒 → 丢弃
        let stale = Some(WriteCtx {
            lease_id: "lease-stale".into(),
            generation: 7,
        });
        tx.send(SessionCommand::Write {
            text: "stale-cmd".into(),
            lease: stale,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let lines: Vec<String> = bus
            .read_log("sess-e", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|l| l.contains("过期写入已拦截")),
            "必须有拦截注记行：{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l == "[TX] stale-cmd"),
            "过期写入不得触串口/产生 TX 回显：{lines:?}"
        );
        // 审计 write.deny
        let denied = audit
            .recent(50)
            .iter()
            .any(|e| e.action == "write.deny" && e.dut.as_deref() == Some("dut-e"));
        assert!(denied, "actor 拒绝必须落审计");
        drop(tx);
        let _ = handle.await;
    }

    #[test]
    fn fixtures_contain_panic_evidence() {
        let panic = panic_fixture().join("\n");
        assert!(panic.contains("Kernel panic - not syncing"));
        assert!(panic.contains("Call trace:"));
        assert!(
            boot_fixture()
                .iter()
                .any(|l| l.contains("Started Serial Getty"))
        );
    }

    /// P2-034b：[TX] 注记单行化——emit_tx（终端流）与 log_tx（.log 审计流）
    /// 把 display_text 的换行转义为字面 `\n` / `\r`，不出现无前缀续行；
    /// wire 原始字节（.bin）保留真实线上字节。
    #[test]
    fn tx_annotation_is_single_line_for_multiline_text() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/demo/tx-single-line")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let bus = Arc::new(EventBus::new(64));
        let logs = Arc::new(std::sync::Mutex::new(
            SessionLogWriter::new(&dir, "sess-txml").unwrap(),
        ));
        let ctx = SessionCtx::new("dut-txml", "sess-txml", bus.clone()).with_logs(logs);
        let expected = "[TX] echo C1\\necho C2";
        // emit_tx：终端流（总线）必须单行
        ctx.emit_tx("echo C1\necho C2\n", "echo C1\necho C2");
        let chunks: Vec<String> = bus
            .read_log("sess-txml", 0, usize::MAX)
            .iter()
            .filter_map(|e| match &e.event {
                UiEvent::SerialChunk { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            chunks.iter().any(|t| t == expected),
            "[TX] 总线注记必须单行（换行转义）：{chunks:?}"
        );
        // log_tx：.log 审计流同样单行
        ctx.log_tx(b"echo C1\recho C2\r\n", "echo C1\r\necho C2");
        let text = std::fs::read_to_string(dir.join("sess-txml.log")).unwrap();
        assert!(
            text.lines().any(|line| line == "[TX] echo C1\\r\\necho C2"),
            ".log 必须有单行 [TX] 注记：{text:?}"
        );
        assert!(
            !text.lines().any(|line| line == "echo C2"),
            ".log 不得出现 [TX] 的无前缀续行：{text:?}"
        );
        // .bin 保留真实线上字节（不经转义）
        let raw = std::fs::read(dir.join("sess-txml.bin")).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&raw),
            "echo C1\necho C2\necho C1\recho C2\r\n"
        );
    }
}
