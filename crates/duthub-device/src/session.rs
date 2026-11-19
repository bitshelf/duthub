//! 会话管理器（M1.3）：注册表、端口互斥、级联取消、重连退避。
//!
//! - 同一 `(host, port)` 同时只能绑定一个有效会话 → `DeviceError::Busy`。
//! - 关闭顺序（docs/ARCHITECTURE.md）：通知会话 task 退出（Agent 侧未来在
//!   `pre_close` 钩子先行）→ 等 task 结束 → 移除句柄 → 释放端口锁。
//! - 重连退避为纯函数（真实驱动用），上限可配。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, mpsc};

use duthub_core::audit::AuditSink;
use duthub_core::lease::LeaseAuthority;
use duthub_core::{DeviceError, DeviceResult};

use crate::bus::EventBus;
pub use crate::demo::SessionCommand;
use crate::demo::{SessionCtx, run_demo_session};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Demo,
    Real,
}

/// 发送路径的行尾策略（H1.2 会话级设置）：无 / \n / \r\n。
///
/// 默认 `Lf`（历史行为：write_line 自动补 \n）。
/// 已以 \n 或 \r 结尾的文本按原文发送，不重复追加。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEnding {
    /// 不追加任何行尾
    Off,
    /// 追加 \n（默认）
    #[default]
    Lf,
    /// 追加 \r\n（Windows 风格设备常用）
    Crlf,
}

impl LineEnding {
    /// UI 展示用短标签。
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Off => "无",
            LineEnding::Lf => "\\n",
            LineEnding::Crlf => "\\r\\n",
        }
    }

    /// 发送路径生效：按策略为不带行尾的文本追加行尾符。
    pub fn apply(self, text: &str) -> String {
        if text.ends_with('\n') || text.ends_with('\r') {
            return text.to_string();
        }
        match self {
            LineEnding::Off => text.to_string(),
            LineEnding::Lf => format!("{text}\n"),
            LineEnding::Crlf => format!("{text}\r\n"),
        }
    }
}

pub struct SessionHandle {
    pub(crate) logs: Arc<Mutex<crate::logs::SessionLogWriter>>,
    /// 会话日志写通道克隆：供电分段（log_control_boundary）同步落盘时用。
    /// None = 写线程未启动（无日志/构建降级），走直写兜底。
    pub(crate) log_tx: Option<std::sync::mpsc::Sender<crate::demo::LogMsg>>,
    pub dut_id: String,
    pub session_id: String,
    pub kind: SessionKind,
    pub host: String,
    pub port: String,
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    pub abort: tokio::task::AbortHandle,
    /// 会话 task 的 JoinHandle（real 会话首开失败时由外层 watcher 等待并回收）
    pub join: Option<tokio::task::JoinHandle<()>>,
    /// 成功打开串口后置 true；task 结束时仍为 false = 首开失败，需要回收注册表
    pub opened_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 手动重连唤醒：退避等待期间收到通知则提前重试。notify_one 在无等待者时
    /// 存一个许可（上限 1）——连接正常时点击"重连"也不丢，下一次断线退避会
    /// 立即返回，语义即"手动请求一次立即重连"。
    pub reconnect_notify: Arc<Notify>,
}

/// 断线重连退避：base * 2^(attempt-1)，封顶 `max`；attempt 从 1 计。
pub fn reconnect_backoff(attempt: u32, base: Duration, max: Duration) -> Duration {
    let exp = attempt.saturating_sub(1).min(16);
    let scaled = base.saturating_mul(1u32 << exp);
    scaled.min(max)
}

/// 会话注册表 + 端口锁（独立结构便于测试）。
#[derive(Default)]
pub struct Registry {
    sessions: HashMap<String, SessionHandle>,
    locked_ports: HashSet<(String, String)>,
    closing: HashMap<String, tokio::task::Id>,
}

impl Registry {
    pub fn get(&self, dut_id: &str) -> Option<&SessionHandle> {
        self.sessions.get(dut_id)
    }

    pub(crate) fn is_reserved(&self, dut_id: &str) -> bool {
        self.sessions.contains_key(dut_id) || self.closing.contains_key(dut_id)
    }

    pub(crate) fn begin_close(&mut self, dut_id: &str) -> Option<SessionHandle> {
        let handle = self.sessions.remove(dut_id)?;
        self.closing.insert(dut_id.to_owned(), handle.abort.id());
        Some(handle)
    }

    pub fn lock_port(&mut self, host: &str, port: &str) -> DeviceResult<()> {
        let key = (host.to_string(), port.to_string());
        if self.locked_ports.contains(&key) {
            return Err(DeviceError::Busy);
        }
        self.locked_ports.insert(key);
        Ok(())
    }

    pub fn unlock_port(&mut self, host: &str, port: &str) {
        self.locked_ports
            .remove(&(host.to_string(), port.to_string()));
    }

    /// Complete teardown only for the matching actor generation.
    pub(crate) fn finish(&mut self, dut_id: &str, actor: tokio::task::Id) -> Option<bool> {
        if self.closing.get(dut_id) == Some(&actor) {
            self.closing.remove(dut_id);
            return Some(true);
        }
        if self.sessions.get(dut_id)?.abort.id() != actor {
            return None;
        }
        self.sessions.remove(dut_id);
        Some(false)
    }

    pub fn insert(&mut self, handle: SessionHandle) {
        self.sessions.insert(handle.dut_id.clone(), handle);
    }

    pub fn remove(&mut self, dut_id: &str) -> Option<SessionHandle> {
        self.sessions.remove(dut_id)
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }
}

/// 会话打开参数（避免 8 参函数）。
pub struct SessionSpec<'a> {
    pub dut_id: &'a str,
    pub session_id: &'a str,
    pub kind: SessionKind,
    pub port: &'a str,
    pub baud: u32,
    pub demo_line_delay_ms: u64,
    /// 终端回滚缓冲行数（串口设置页；real 会话建 Term 时生效）。
    pub scrollback_rows: usize,
    /// 快照携带的最大行数（串口设置页；决定 UI 可回看的历史行数）。
    pub snapshot_max_rows: usize,
    pub logs: Arc<Mutex<crate::logs::SessionLogWriter>>,
    /// real 会话：成功打开串口后置 true（首开失败回收注册表用）
    pub opened_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 串口 actor 在写入前验证命令携带的租约。None 表示该路径不要求租约。
    pub lease_authority: Option<Arc<dyn LeaseAuthority>>,
    /// 审计接收端（actor 拒绝写入时记 write.deny；None = 不落审计）。
    pub audit: Option<Arc<AuditSink>>,
    /// 每 DUT 乱码检测开关（与 `DeviceService` 共享；空 map = 全关）。
    pub garble_enabled: Arc<Mutex<HashMap<String, bool>>>,
}

/// 会话工厂：按 kind 构造驱动 task；返回 (句柄, 命令发送端克隆)。
pub fn spawn_session(
    bus: Arc<EventBus>,
    spec: SessionSpec<'_>,
) -> DeviceResult<(SessionHandle, mpsc::Sender<SessionCommand>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(32);
    let reconnect_notify = Arc::new(Notify::new());
    let ctx = SessionCtx::new(spec.dut_id, spec.session_id, bus)
        .with_logs(spec.logs.clone())
        .with_reconnect_notify(reconnect_notify.clone())
        .with_lease_authority(spec.lease_authority.clone())
        .with_audit(spec.audit.clone())
        .with_garble(spec.garble_enabled.clone());
    let log_tx = ctx.log_sender();
    let task = match spec.kind {
        SessionKind::Demo => tokio::spawn(async move {
            run_demo_session(ctx, cmd_rx, spec.demo_line_delay_ms).await;
        }),
        SessionKind::Real => {
            #[cfg(feature = "real-hardware")]
            {
                let port = spec.port.to_string();
                let baud = spec.baud;
                let scrollback_rows = spec.scrollback_rows;
                let snapshot_max_rows = spec.snapshot_max_rows;
                let flag = spec.opened_flag.clone().unwrap_or_else(|| {
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
                });
                tokio::spawn(crate::real::run_real_session(
                    ctx,
                    cmd_rx,
                    port,
                    baud,
                    flag,
                    scrollback_rows,
                    snapshot_max_rows,
                ))
            }
            #[cfg(not(feature = "real-hardware"))]
            {
                let _ = (ctx, spec.port, spec.baud);
                return Err(DeviceError::Unsupported);
            }
        }
    };
    let abort = task.abort_handle();
    let handle = SessionHandle {
        logs: spec.logs,
        log_tx,
        dut_id: spec.dut_id.to_string(),
        session_id: spec.session_id.to_string(),
        kind: spec.kind,
        host: "local".to_string(),
        port: spec.port.to_string(),
        cmd_tx: cmd_tx.clone(),
        abort,
        join: Some(task),
        opened_flag: spec
            .opened_flag
            .clone()
            .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))),
        reconnect_notify,
    };
    Ok((handle, cmd_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::ipc::UiEvent;
    use duthub_core::lease::WriteIdentity;

    #[test]
    fn line_ending_apply_adds_configured_suffix() {
        assert_eq!(LineEnding::Lf.apply("ls"), "ls\n");
        assert_eq!(LineEnding::Crlf.apply("ls"), "ls\r\n");
        assert_eq!(LineEnding::Off.apply("ls"), "ls");
        // 已带行尾的文本按原文发送，不重复追加
        assert_eq!(LineEnding::Lf.apply("ls\n"), "ls\n");
        assert_eq!(LineEnding::Crlf.apply("ls\r\n"), "ls\r\n");
        assert_eq!(LineEnding::Off.apply("ls\r"), "ls\r");
        // 默认策略为 Lf（保持历史行为）
        assert_eq!(LineEnding::default(), LineEnding::Lf);
    }

    #[test]
    fn line_ending_labels_cover_all_variants() {
        assert_eq!(LineEnding::Off.label(), "无");
        assert_eq!(LineEnding::Lf.label(), "\\n");
        assert_eq!(LineEnding::Crlf.label(), "\\r\\n");
    }

    #[test]
    fn port_lock_is_exclusive() {
        let mut registry = Registry::default();
        registry.lock_port("local", "COM3").unwrap();
        let err = registry.lock_port("local", "COM3").unwrap_err();
        assert!(matches!(err, DeviceError::Busy), "同端口二次绑定必须 Busy");
        registry.unlock_port("local", "COM3");
        registry.lock_port("local", "COM3").unwrap();
    }

    #[test]
    fn different_ports_can_coexist() {
        let mut registry = Registry::default();
        registry.lock_port("local", "COM3").unwrap();
        registry.lock_port("local", "COM5").unwrap();
        registry.lock_port("lab-pc", "COM3").unwrap();
    }

    #[test]
    fn backoff_grows_and_caps() {
        let base = Duration::from_millis(200);
        let max = Duration::from_secs(8);
        assert_eq!(reconnect_backoff(1, base, max), Duration::from_millis(200));
        assert_eq!(reconnect_backoff(2, base, max), Duration::from_millis(400));
        assert_eq!(reconnect_backoff(3, base, max), Duration::from_millis(800));
        assert_eq!(reconnect_backoff(10, base, max), max);
        assert_eq!(reconnect_backoff(999, base, max), max);
    }

    /// IO-5（纯函数面）：已带行尾（\n、\r\n、\r）的文本在全部三种策略下一律
    /// 原文直传——不得把 \n 重复追加成 \n\r\n、把 \r\n 补成 \r\n\r\n。
    /// 与既有 line_ending_apply_adds_configured_suffix 互补：那边只覆盖 3 种
    /// 组合，这里锁 3 策略 × 3 已带行尾形态的全矩阵。
    #[test]
    fn line_ending_apply_passthrough_matrix_all_policies() {
        for ending in [LineEnding::Off, LineEnding::Lf, LineEnding::Crlf] {
            for ended in ["x\n", "x\r\n", "x\r"] {
                assert_eq!(
                    ending.apply(ended),
                    ended,
                    "{ending:?} 不得对已带行尾的 {ended:?} 重复追加"
                );
            }
        }
    }

    /// IO-5（会话发送路径）：demo 会话对每条 Write 按当前会话 LineEnding 策略
    /// 补行尾，以 .bin 线上字节为准：全新会话默认 Lf（无需 SetEnding）、
    /// SetEnding(Crlf/Off) 对后续写生效、已带行尾的文本在任何策略下原文直传。
    /// 轮询只作宽松时限上界（0 延迟会话 5s 内必达），不写会闪断的窄时序断言；
    /// 断言前先经 Flush 屏障排空日志写线程，join 完成即 .bin 完整落盘。
    #[tokio::test]
    async fn session_send_path_applies_line_ending_policy_to_wire_bytes() {
        // 临时目录照既有测试写法：target/test-tmp/… + 进程 id
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device")
            .join(format!("session-line-ending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logs = Arc::new(Mutex::new(
            crate::logs::SessionLogWriter::new(&dir, "sess-lio").unwrap(),
        ));
        let bus = Arc::new(EventBus::new(256));
        let spec = SessionSpec {
            dut_id: "dut-lio",
            session_id: "sess-lio",
            kind: SessionKind::Demo,
            port: "COM3",
            baud: 115200,
            demo_line_delay_ms: 0,
            scrollback_rows: crate::term::SCROLLBACK_HISTORY,
            snapshot_max_rows: crate::term::SNAPSHOT_MAX_ROWS,
            logs: logs.clone(),
            opened_flag: None,
            lease_authority: None,
            audit: None,
            garble_enabled: Arc::new(Mutex::new(HashMap::new())),
        };
        let (mut handle, cmd_tx) = spawn_session(bus.clone(), spec).unwrap();

        let send_write = |text: &str| {
            cmd_tx.send(SessionCommand::Write {
                text: text.to_string(),
                lease: None,
            })
        };
        // 全新会话默认 Lf：不设 SetEnding 也补 \n
        send_write("probe-default").await.unwrap();
        cmd_tx
            .send(SessionCommand::SetEnding(LineEnding::Crlf))
            .await
            .unwrap();
        send_write("probe-crlf").await.unwrap();
        // 已带 \n 的文本在 Crlf 策略下原文直传，不得追加成 \n\r\n
        send_write("probe-ends-lf\n").await.unwrap();
        cmd_tx
            .send(SessionCommand::SetEnding(LineEnding::Off))
            .await
            .unwrap();
        send_write("probe-off").await.unwrap();
        // 已带 \r\n 的文本在 Off 策略下同样原文直传
        send_write("probe-ends-crlf\r\n").await.unwrap();

        // 宽松上界：末条写入的 [TX] 回显 5s 内必须到达。emit_tx 先把线上字节
        // 入日志队列再发布总线，回显可见即全部写入已过发送路径。
        let mut processed = false;
        for _ in 0..250 {
            processed = bus
                .read_log("sess-lio", 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(&e.event, UiEvent::SerialChunk { text, .. } if text == "[TX] probe-ends-crlf\\r\\n")
                });
            if processed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            processed,
            "全部写入必须到达 demo 会话（末条 [TX] 回显可见）"
        );

        // 断言前先排空：Flush 屏障等待写线程把队列全部落盘（照 flush_session_logs）
        if let Some(tx) = &handle.log_tx {
            let (ack, rx) = std::sync::mpsc::channel();
            if tx.send(crate::demo::LogMsg::Flush(ack)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_secs(3));
            }
        }
        // 收尾：丢弃发送端克隆让会话退出；join 完成 = SessionCtx::Drop 已等写线程收尾
        let join = handle.join.take();
        drop(handle);
        drop(cmd_tx);
        let _ = join.unwrap().await;

        let raw = std::fs::read(dir.join("sess-lio.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        // 默认策略 Lf
        assert!(
            raw.contains("probe-default\n"),
            "默认策略必须补 \\n：{raw:?}"
        );
        assert!(!raw.contains("probe-default\r\n"), "默认策略不得写出 CRLF");
        // SetEnding(Crlf) 对后续写生效
        assert!(
            raw.contains("probe-crlf\r\n"),
            "Crlf 策略必须补 \\r\\n：{raw:?}"
        );
        // 已带行尾的文本按原文直传，任何策略都不重复追加
        assert!(
            raw.contains("probe-ends-lf\n"),
            "已带 \\n 的文本应原文直传：{raw:?}"
        );
        assert!(
            !raw.contains("probe-ends-lf\n\r\n"),
            "Crlf 策略不得对已带行尾的文本重复追加：{raw:?}"
        );
        // Off 策略不补任何行尾
        assert!(raw.contains("probe-off"), "Off 策略正文必须保留：{raw:?}");
        assert!(!raw.contains("probe-off\n"), "Off 策略不得补 \\n：{raw:?}");
        assert!(
            !raw.contains("probe-off\r\n"),
            "Off 策略不得补 \\r\\n：{raw:?}"
        );
        assert!(
            raw.contains("probe-ends-crlf\r\n"),
            "Off 策略下已带 \\r\\n 的文本按原文直传：{raw:?}"
        );
    }

    /// IO-5（write_line 多行归一，P2-034）：归一顺序为先折叠 CRLF→CR、再清残留
    /// 裸 LF，最终线上 payload 行分隔符全为 CR——无透传 CRLF、无残留裸 LF、
    /// 折叠顺序错误会产生的 \r\r 混排不得出现；行尾策略只作用于末尾（默认 Lf
    /// 补 \n），已以 \r 结尾的文本不再追加。
    #[test]
    fn write_line_normalizes_multiline_to_cr_without_crlf_mixing() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/device")
            .join(format!("session-normalize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let service = crate::DeviceService::custom(64, dir.clone(), 0);
        service
            .open_session("dut-norm", "COM3", 115200, SessionKind::Demo)
            .unwrap();
        // 混合输入一次覆盖归一全序：CRLF 折叠 + 残留 LF 清理 + 裸 CR 直传
        service
            .write_line("dut-norm", WriteIdentity::gui(), "M1\r\nM2\nM3\rM4")
            .unwrap();
        // 末尾为裸 CR：视为已带行尾，默认 Lf 策略不再追加
        service
            .write_line("dut-norm", WriteIdentity::gui(), "tail\r")
            .unwrap();
        // 宽松上界：末条 [TX] 回显 5s 内到达 = 全部写入已过发送路径
        let mut processed = false;
        for _ in 0..250 {
            processed = service.read_log("sess-dut-norm", 0, usize::MAX).iter().any(
                |e| matches!(&e.event, UiEvent::SerialChunk { text, .. } if text == "[TX] tail\\r"),
            );
            if processed {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            processed,
            "全部写入必须到达 demo 会话（末条 [TX] 回显可见）"
        );
        // 断言前先 flush：写线程逐条落盘后 .bin 才完整
        service.flush_session_logs("dut-norm");
        service.close_session("dut-norm").ok();
        let raw = std::fs::read(dir.join("sess-dut-norm.bin")).unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        // 线上 payload：行分隔符全为 CR，末尾按默认 Lf 补 \n
        assert!(
            raw.contains("M1\rM2\rM3\rM4\n"),
            "多行 payload 必须以 CR 分隔并按默认 Lf 收尾：{raw:?}"
        );
        assert!(!raw.contains("M1\r\n"), "CRLF 不得原样透传：{raw:?}");
        assert!(
            !raw.contains("M1\r\r"),
            "折叠顺序错误会把残留 LF 折成 \\r\\r 混排：{raw:?}"
        );
        assert!(!raw.contains("M2\n"), "残留裸 LF 必须清为 CR：{raw:?}");
        // 已以 \r 结尾的文本原文直传（write_line 端不重复追加行尾）
        assert!(raw.contains("tail\r"), "已带行尾的文本应原文直传：{raw:?}");
        assert!(
            !raw.contains("tail\r\n"),
            "默认 Lf 不得对已带行尾的文本重复追加：{raw:?}"
        );
    }
}
