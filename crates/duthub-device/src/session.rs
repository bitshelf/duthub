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
}

impl Registry {
    pub fn get(&self, dut_id: &str) -> Option<&SessionHandle> {
        self.sessions.get(dut_id)
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

    /// 首开失败回收：移除会话句柄并释放端口锁（task 已死，无 Close 可发）。
    pub fn reap_never_opened(&mut self, dut_id: &str, host: &str, port: &str) {
        if self.sessions.remove(dut_id).is_some() {
            self.unlock_port(host, port);
        }
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
    pub logs: Arc<Mutex<crate::logs::SessionLogWriter>>,
    /// real 会话：成功打开串口后置 true（首开失败回收注册表用）
    pub opened_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 串口 actor 在写入前验证命令携带的租约。None 表示该路径不要求租约。
    pub lease_authority: Option<Arc<dyn LeaseAuthority>>,
    /// 审计接收端（actor 拒绝写入时记 write.deny；None = 不落审计）。
    pub audit: Option<Arc<AuditSink>>,
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
        .with_audit(spec.audit.clone());
    let task = match spec.kind {
        SessionKind::Demo => tokio::spawn(async move {
            run_demo_session(ctx, cmd_rx, spec.demo_line_delay_ms).await;
        }),
        SessionKind::Real => {
            #[cfg(feature = "real-hardware")]
            {
                let port = spec.port.to_string();
                let baud = spec.baud;
                let flag = spec.opened_flag.clone().unwrap_or_else(|| {
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
                });
                tokio::spawn(crate::real::run_real_session(ctx, cmd_rx, port, baud, flag))
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
}
