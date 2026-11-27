//! Line-delimited JSON-RPC ACP client.
//!
//! Contracts:
//! - stdout is protocol-only; stderr is collected separately and redacted.
//! - Each stdout line is bounded by [`MAX_STDOUT_LINE`] (4 MiB).
//! - The body event queue is bounded (4096); critical events live in a separate
//!   bounded queue that fails closed on overflow.
//! - Agent -> client `session/request_permission` requests are cancelled;
//!   `fs/*` and `terminal/*` return `MethodNotFound`.
//! - The child environment inherits only the system allowlist (`INHERITED_ENV`);
//!   host user-profile keys (`USERPROFILE`, `APPDATA`, `HOME`, `HOMEDRIVE`,
//!   `HOMEPATH`) are never inherited, while `LOCALAPPDATA`, `TEMP`, and `TMP`
//!   are redirected into the per-DUT sandbox cell.
//!
//! OS containment is delegated to `duthub-sandbox` (`mxc-sdk`): filesystem
//! read/write isolation, directional network policy, UI/clipboard/input
//! isolation, and Job/container process-tree termination.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duthub_sandbox::{CellSpec, SandboxPolicy, SandboxedProcess, SpawnRequest, spawn_streaming};
use serde_json::{Value, json};

pub const MAX_STDOUT_LINE: usize = 4 * 1024 * 1024;
pub const EVENT_QUEUE_CAPACITY: usize = 4096;
/// duthub 实现的 ACP 协议版本（initialize 必须显式等于该值）。
pub const ACP_PROTOCOL_VERSION: u64 = 1;
/// 控制事件队列上限。正常低频；若失控的 agent 持续制造关键事件，
/// 清空队列并以 `CRITICAL_OVERFLOW_PREFIX` 的 ProtocolError fail closed。
pub const CRITICAL_QUEUE_CAPACITY: usize = 1024;
/// 关键事件队列溢出标记（host 见到它无条件终止会话）。
pub const CRITICAL_OVERFLOW_PREFIX: &str = "ACP 关键事件队列溢出";
const DEFAULT_STDERR_CAP: usize = 64 * 1024;
const PROTOCOL_BODY_PREVIEW: usize = 200;
/// A request checks the closed/cancel flags at this cadence even while the
/// agent is silent, so stop/EOF never waits for the full request timeout.
const REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Drop 关闭 Job 句柄后等待主进程退出的上限（KILL_ON_JOB_CLOSE 是异步终止）；
/// 超时退回 `kill_process_tree`，避免 Drop 永久阻塞工作线程。
const JOB_CLOSE_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// 子进程可继承的系统变量（goose/ACP 运行必需）。
///
/// 明确**不**包含用户目录键（`USERPROFILE`、`APPDATA`、`LOCALAPPDATA`、`HOME`、
/// `HOMEDRIVE`、`HOMEPATH`）与 `TEMP`/`TMP`：goose 的配置/会话由
/// `GOOSE_PATH_ROOT` 指定，临时目录由 [`AcpClient::spawn`] 指向会话沙箱。
const INHERITED_ENV: &[&str] = &[
    "SystemRoot",
    "SYSTEMROOT",
    "SystemDrive",
    "windir",
    "WINDIR",
    "PATH",
    "Path",
    "PATHEXT",
    "COMSPEC",
    "ComSpec",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "ProgramFiles",
    "PROGRAMFILES(X86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "LANG",
    "LC_ALL",
    "TERM",
];

/// 用户目录 / 用户 TEMP 键：**不得**继承（测试据此做负向断言）。
#[cfg(test)]
const USER_SCOPE_ENV: &[&str] = &[
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
    "TEMP",
    "TMP",
];

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub extension_results: Value,
    pub current_mode: Option<String>,
}

#[derive(Clone, Copy)]
pub struct McpServerRef<'a> {
    pub name: &'a str,
    pub url: &'a str,
    pub token: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpEvent {
    AgentChunk(String),
    AgentThought(String),
    ToolCall {
        id: String,
        title: String,
        status: String,
    },
    PromptDone {
        stop_reason: String,
    },
    PermissionDenied {
        title: String,
    },
    ProtocolError(String),
    Exited(Option<i32>),
}

#[derive(Debug, Clone, Default)]
pub struct InitializeInfo {
    pub protocol_version: u64,
    pub agent_name: String,
    pub agent_version: String,
    pub load_session: bool,
    pub mcp_http: bool,
}

#[derive(Debug)]
pub enum AcpError {
    Spawn(String),
    Io(String),
    Protocol(String),
    /// JSON-RPC error 响应（保留 code 供 `session/load` 的 -32601 判定）。
    Rpc {
        code: i64,
        message: String,
    },
    Timeout(String),
    Closed,
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::Spawn(error) => write!(formatter, "ACP 进程启动失败: {error}"),
            AcpError::Io(error) => write!(formatter, "ACP I/O 失败: {error}"),
            AcpError::Protocol(error) => write!(formatter, "ACP 协议错误: {error}"),
            AcpError::Rpc { code, message } => {
                write!(formatter, "ACP JSON-RPC 错误({code}): {message}")
            }
            AcpError::Timeout(what) => write!(formatter, "ACP {what} 超时"),
            AcpError::Closed => write!(formatter, "ACP 进程已关闭"),
        }
    }
}

impl std::error::Error for AcpError {}

struct StderrState {
    bytes: VecDeque<u8>,
    cap: usize,
}

impl StderrState {
    fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            cap: cap.max(1024),
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        for byte in chunk {
            if self.bytes.len() >= self.cap {
                self.bytes.pop_front();
            }
            self.bytes.push_back(*byte);
        }
    }

    fn text(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

struct AcpInner {
    stdin: Mutex<Option<Box<dyn Write + Send>>>,
    events: Mutex<VecDeque<AcpEvent>>,
    /// 控制事件独立通道（F15）：PromptDone / Exited / ProtocolError 不能被
    /// 正文事件溢出丢弃；自身有上限，越界即 fail closed。
    critical: Mutex<VecDeque<AcpEvent>>,
    critical_overflowed: Mutex<bool>,
    overflowed: Mutex<bool>,
    pending: Mutex<HashMap<u64, Sender<Result<Value, AcpError>>>>,
    prompts: Mutex<HashSet<u64>>,
    next_id: AtomicU64,
    last_activity_ms: AtomicU64,
    exited: AtomicBool,
    /// Set once the stdout pipe reaches EOF, fails, or the child exits.
    closed: AtomicBool,
    /// Host stop flag: pending requests abort promptly instead of timing out.
    cancel: Arc<AtomicBool>,
    stderr: Mutex<StderrState>,
    secrets: Vec<String>,
}

fn is_critical_event(event: &AcpEvent) -> bool {
    matches!(
        event,
        AcpEvent::PromptDone { .. } | AcpEvent::ProtocolError(_) | AcpEvent::Exited(_)
    )
}

impl AcpInner {
    fn push_event(&self, event: AcpEvent) {
        if is_critical_event(&event) {
            let mut critical = self.critical.lock().unwrap();
            if critical.len() >= CRITICAL_QUEUE_CAPACITY {
                let mut overflowed = self.critical_overflowed.lock().unwrap();
                if !*overflowed {
                    *overflowed = true;
                    critical.clear();
                    critical.push_back(AcpEvent::ProtocolError(format!(
                        "{CRITICAL_OVERFLOW_PREFIX}（{CRITICAL_QUEUE_CAPACITY}），会话必须终止"
                    )));
                }
                return;
            }
            critical.push_back(event);
            return;
        }
        let mut events = self.events.lock().unwrap();
        if events.len() >= EVENT_QUEUE_CAPACITY {
            events.pop_front();
            let mut overflowed = self.overflowed.lock().unwrap();
            if !*overflowed {
                *overflowed = true;
                self.critical
                    .lock()
                    .unwrap()
                    .push_back(AcpEvent::ProtocolError(
                        "ACP 事件队列溢出（4096），已丢弃最旧正文事件".into(),
                    ));
            }
        }
        events.push_back(event);
    }

    fn redact(&self, text: &str) -> String {
        redact_secrets(text, &self.secrets)
    }

    fn stderr_tail(&self) -> String {
        let raw = self.stderr.lock().unwrap().text();
        self.redact(&raw)
    }

    fn write_json(&self, value: &Value) -> Result<(), AcpError> {
        let mut line = serde_json::to_vec(value)
            .map_err(|error| AcpError::Protocol(format!("序列化 JSON-RPC 失败: {error}")))?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().unwrap();
        let Some(stdin) = stdin.as_mut() else {
            return Err(AcpError::Closed);
        };
        stdin
            .write_all(&line)
            .and_then(|()| stdin.flush())
            .map_err(|error| AcpError::Io(error.to_string()))
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, AcpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(AcpError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, sender);
        let write = self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        if let Err(error) = write {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.cancel.load(Ordering::Relaxed) || self.closed.load(Ordering::SeqCst) {
                self.pending.lock().unwrap().remove(&id);
                return Err(AcpError::Closed);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.pending.lock().unwrap().remove(&id);
                // Name the method: a hung `session/new` (e.g. a goose that keeps
                // retrying an unreachable MCP endpoint) must be diagnosable from
                // the session status, not just "request timed out".
                return Err(AcpError::Timeout(format!("{method} 请求响应")));
            }
            match receiver.recv_timeout(remaining.min(REQUEST_POLL_INTERVAL)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.pending.lock().unwrap().remove(&id);
                    return Err(AcpError::Closed);
                }
            }
        }
    }

    /// Mark the pipe as closed and fail every pending request with
    /// [`AcpError::Closed`]. Called on stdout EOF/read errors and on child exit;
    /// without it an `initialize` against a dead process would block for the
    /// whole request timeout.
    fn mark_closed(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let pending: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, sender) in pending {
            let _ = sender.send(Err(AcpError::Closed));
        }
    }

    fn handle_stdout_line(&self, line: &str) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                let preview: String = self
                    .redact(line)
                    .chars()
                    .take(PROTOCOL_BODY_PREVIEW)
                    .collect();
                self.push_event(AcpEvent::ProtocolError(format!(
                    "stdout 不是 JSON-RPC: {preview}"
                )));
                return;
            }
        };
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            let method = method.to_owned();
            if let Some(id) = value.get("id") {
                let response = self.agent_request_response(id, &method, &value);
                let _ = self.write_json(&response);
            } else {
                self.handle_notification(&method, &value);
            }
            return;
        }
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            return;
        };
        if self.prompts.lock().unwrap().remove(&id) {
            if let Some(error) = value.get("error") {
                self.push_event(AcpEvent::ProtocolError(format!(
                    "prompt 失败: {}",
                    self.redact(&compact_json(error))
                )));
            } else {
                let stop_reason = value
                    .pointer("/result/stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_owned();
                self.push_event(AcpEvent::PromptDone { stop_reason });
            }
            return;
        }
        let Some(sender) = self.pending.lock().unwrap().remove(&id) else {
            return;
        };
        let outcome = match value.get("error") {
            Some(error) => Err(AcpError::Rpc {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: self.redact(&compact_json(error)),
            }),
            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = sender.send(outcome);
    }

    /// agent → client 请求的响应（拒绝权限，fs/terminal 与未知方法 MethodNotFound）。
    fn agent_request_response(&self, id: &Value, method: &str, value: &Value) -> Value {
        match method {
            "session/request_permission" => {
                let title = value
                    .pointer("/params/toolCall/title")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                    .or_else(|| {
                        value
                            .pointer("/params/toolCall/toolCallId")
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("工具调用")
                    .to_owned();
                self.push_event(AcpEvent::PermissionDenied { title });
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "outcome": { "outcome": "cancelled" } },
                })
            }
            other => {
                if other.starts_with("fs/") || other.starts_with("terminal/") {
                    self.push_event(AcpEvent::ProtocolError(format!(
                        "已拒绝 {other}：duthub 不提供文件/终端能力"
                    )));
                }
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" },
                })
            }
        }
    }

    fn handle_notification(&self, method: &str, value: &Value) {
        if method != "session/update" {
            return;
        }
        let Some(update) = value.pointer("/params/update") else {
            return;
        };
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "agent_message_chunk" => {
                if let Some(text) = content_text(update.get("content")) {
                    self.push_event(AcpEvent::AgentChunk(text));
                }
            }
            "agent_thought_chunk" => {
                if let Some(text) = content_text(update.get("content")) {
                    self.push_event(AcpEvent::AgentThought(text));
                }
            }
            "tool_call" | "tool_call_update" => {
                let id = update
                    .get("toolCallId")
                    .or_else(|| update.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let status = update
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                    .to_owned();
                self.push_event(AcpEvent::ToolCall { id, title, status });
            }
            _ => {}
        }
    }
}

/// F16：initialize 的 protocolVersion 必须显式等于 duthub 支持的版本。
fn check_protocol_version(result: &Value) -> Result<u64, AcpError> {
    let protocol_version = result
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if protocol_version != ACP_PROTOCOL_VERSION {
        return Err(AcpError::Protocol(format!(
            "initialize protocolVersion 不支持: {protocol_version}（duthub 仅支持 {ACP_PROTOCOL_VERSION}）"
        )));
    }
    Ok(protocol_version)
}

fn content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    let blocks = content.as_array()?;
    let text: String = blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    (!text.is_empty()).then_some(text)
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".into())
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Stable, filesystem-safe scope for a cell rooted at `cwd`.
///
/// The canonical per-DUT path also has a stable `dut-<hex>` scope, but embedded
/// callers use arbitrary directories; the capability SID is persisted in the
/// cell root, so the scope only has to avoid cross-cell profile collisions.
fn cell_scope(cwd: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in cwd.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("acp-{hash:016x}")
}

/// Agent host policy: MXC container with read/write cell isolation,
/// loopback + outbound network, UI isolation, and optional `GOOSE_PATH_ROOT`.
///
/// `goose_root` is the per-DUT `GOOSE_PATH_ROOT` (config/sessions) sibling of
/// the cell. `allowed_exe_dir` grants read-only access to the Agent binary
/// directory even when installed under `.duthub/tools/<tool>/`.
pub(crate) fn agent_policy(
    cell: &CellSpec,
    goose_root: Option<&Path>,
    allowed_exe_dir: Option<&Path>,
) -> SandboxPolicy {
    let mut policy = SandboxPolicy::agent_cell(cell, allowed_exe_dir);
    if let Some(root) = goose_root
        && root != cell.root()
    {
        policy.writable.push(root.to_path_buf());
    }
    policy
}

/// Replace `key` (case-insensitively) so the cell's TEMP/TMP cannot be
/// overridden by a caller-supplied environment entry.
fn set_environment(
    environment: &mut Vec<(std::ffi::OsString, std::ffi::OsString)>,
    key: &str,
    value: &std::ffi::OsStr,
) {
    environment.retain(|(existing, _)| !existing.eq_ignore_ascii_case(key));
    environment.push((key.into(), value.to_owned()));
}

pub struct AcpClient {
    inner: Arc<AcpInner>,
    /// Long-lived sandboxed process. The pipes were taken at spawn; this value
    /// owns the process handle and the Job that bounds the tree, so `Drop` is
    /// the last-resort tree kill.
    process: Mutex<SandboxedProcess>,
}

impl AcpClient {
    /// 启动 ACP 子进程（stdin/stdout/stderr 全 piped；Windows CREATE_NO_WINDOW）。
    ///
    /// 环境由三部分组成：
    /// 1. [`INHERITED_ENV`] 的系统变量白名单（**不含**用户目录键）；
    /// 2. 调用方 `env`（模型密钥、GOOSE_PATH_ROOT 等），同名时覆盖白名单；
    /// 3. `TEMP`/`TMP` 最后写入，固定指向 `cwd/tmp`（先创建并做普通目录校验，
    ///    调用方 env 不得覆盖）。
    ///
    /// `cwd` 就是该 DUT 的 sandbox 单元：子进程以该单元的 `WRITE_RESTRICTED`
    /// 受限令牌启动（`duthub_sandbox::spawn_streaming`），令牌/Job/ACL 全部由
    /// 沙箱 crate 建立并在 resume 前回验；建立失败即返回错误，绝不回退完整令牌。
    pub fn spawn(
        program: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        stderr_cap: usize,
    ) -> Result<Self, AcpError> {
        Self::spawn_with_cancel(
            program,
            args,
            cwd,
            env,
            stderr_cap,
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// Same as [`AcpClient::spawn`], but wakes pending requests when `cancel`
    /// is set (host stop). The flag is polled every 20 ms.
    pub fn spawn_with_cancel(
        program: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        stderr_cap: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, AcpError> {
        if !cwd.is_dir() {
            return Err(AcpError::Spawn(format!(
                "工作目录不存在: {}",
                cwd.display()
            )));
        }
        // The production path passes `<work_dir>/agent/dut/<key>/sandbox`;
        // embedded/test callers point the cell at the directory they already
        // own. Either way the writable root is exactly this directory.
        let scope = cell_scope(cwd);
        let cell = CellSpec::new(cwd, &scope)
            .or_else(|_| CellSpec::at(cwd, &scope))
            .map_err(|error| AcpError::Spawn(error.to_string()))?;
        // Direct callers (tests, future launchers) get the same subcommand rule
        // as the registry: a generic `goose.exe` is only an ACP server under
        // `acp`, and without it the process drops into `configure` and exits.
        let args = super::registry::goose_acp_args(program, args);
        // Direct callers (tests, future launchers) pass GOOSE_PATH_ROOT in
        // `env`; the restricted token can only write where the policy grants a
        // capability SID, and goose must persist config and sessions there.
        // Derive it from the environment so a direct caller gets the same second
        // writable root the host establishes, instead of a session that starts
        // and then fails to save anything.
        let goose_root = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("GOOSE_PATH_ROOT"))
            .map(|(_, value)| PathBuf::from(value));
        let policy = agent_policy(&cell, goose_root.as_deref(), program.parent());
        Self::spawn_in_cell(program, &args, &cell, &policy, env, stderr_cap, cancel)
    }

    /// Spawn in an already prepared cell (the host path: the worker established
    /// the cell and recorded its identity before calling this).
    pub fn spawn_in_cell(
        program: &Path,
        args: &[String],
        cell: &CellSpec,
        policy: &SandboxPolicy,
        env: &[(String, String)],
        stderr_cap: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, AcpError> {
        let cwd = cell.root();
        duthub_sandbox::prepare_cell(cell).map_err(|error| AcpError::Spawn(error.to_string()))?;
        let local_app_data = cell.layout().home().join("AppData").join("Local");
        // Validate `<cell>/tmp` as a plain directory (fail closed, never fall
        // back to the host user's TEMP).
        let temp_dir =
            super::sandbox::temp_dir(cwd).map_err(|error| AcpError::Spawn(error.to_string()))?;
        let mut environment: Vec<(std::ffi::OsString, std::ffi::OsString)> = Vec::new();
        for key in INHERITED_ENV {
            if let Some(value) = std::env::var_os(key) {
                environment.push((key.into(), value));
            }
        }
        for (key, value) in env {
            if key.contains('\0') || value.contains('\0') {
                return Err(AcpError::Spawn("环境变量包含 NUL 字符".into()));
            }
            set_environment(&mut environment, key, value.as_ref());
        }
        // Redirect `LOCALAPPDATA` (required by Windows ProcessContainer when
        // default environment inheritance is disabled) and `TEMP`/`TMP` into
        // the per-DUT sandbox cell so host user directories are never exposed.
        set_environment(&mut environment, "LOCALAPPDATA", local_app_data.as_os_str());
        set_environment(&mut environment, "TEMP", temp_dir.as_os_str());
        set_environment(&mut environment, "TMP", temp_dir.as_os_str());
        let request = SpawnRequest {
            program: program.to_path_buf(),
            args: args.iter().map(std::ffi::OsString::from).collect(),
            cwd: cwd.to_path_buf(),
            environment,
        };
        let mut process = spawn_streaming(cell, policy, &request)
            .map_err(|error| AcpError::Spawn(error.to_string()))?;
        let Some(stdin) = process.take_stdin() else {
            return Err(AcpError::Spawn("stdin 未创建".into()));
        };
        let Some(stdout) = process.take_stdout() else {
            return Err(AcpError::Spawn("stdout 未创建".into()));
        };
        let Some(stderr) = process.take_stderr() else {
            return Err(AcpError::Spawn("stderr 未创建".into()));
        };

        let mut secrets: Vec<String> = env
            .iter()
            .filter(|(key, _)| {
                let key = key.to_ascii_uppercase();
                key.contains("KEY") || key.contains("TOKEN") || key.contains("SECRET")
            })
            .map(|(_, value)| value.trim().to_owned())
            .filter(|value| value.len() >= 4)
            .collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        let cap = if stderr_cap == 0 {
            DEFAULT_STDERR_CAP
        } else {
            stderr_cap
        };
        let inner = Arc::new(AcpInner {
            stdin: Mutex::new(Some(stdin)),
            events: Mutex::new(VecDeque::new()),
            critical: Mutex::new(VecDeque::new()),
            critical_overflowed: Mutex::new(false),
            overflowed: Mutex::new(false),
            pending: Mutex::new(HashMap::new()),
            prompts: Mutex::new(HashSet::new()),
            next_id: AtomicU64::new(1),
            last_activity_ms: AtomicU64::new(now_ms()),
            exited: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            cancel,
            stderr: Mutex::new(StderrState::new(cap)),
            secrets,
        });

        let stdout_thread = {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("duthub-acp-stdout".into())
                .spawn(move || read_stdout(stdout, inner))
        };
        if let Err(error) = stdout_thread {
            // Dropping the process handle terminates the Job tree; there is no
            // half-started session without a reader.
            drop(process);
            return Err(AcpError::Spawn(format!("stdout 线程启动失败: {error}")));
        }
        let stderr_thread = {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("duthub-acp-stderr".into())
                .spawn(move || read_stderr(stderr, inner))
        };
        if let Err(error) = stderr_thread {
            drop(process);
            return Err(AcpError::Spawn(format!("stderr 线程启动失败: {error}")));
        }

        Ok(Self {
            inner,
            process: Mutex::new(process),
        })
    }

    pub fn initialize(&self, timeout: Duration) -> Result<InitializeInfo, AcpError> {
        let result = self.inner.request(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                },
                "clientInfo": { "name": "duthub", "version": env!("CARGO_PKG_VERSION") },
            }),
            timeout,
        )?;
        let protocol_version = check_protocol_version(&result)?;
        Ok(InitializeInfo {
            protocol_version,
            agent_name: result
                .pointer("/agentInfo/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            agent_version: result
                .pointer("/agentInfo/version")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            load_session: result
                .pointer("/agentCapabilities/loadSession")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            mcp_http: result
                .pointer("/agentCapabilities/mcpCapabilities/http")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// 新建 session，并把每会话 MCP listener 作为 `mcpServers` 注入。
    /// 保留 `_meta.extensionResults`（F3；用于证明内置扩展已关）。
    pub fn new_session(
        &self,
        cwd: &Path,
        mcp: &McpServerRef<'_>,
        timeout: Duration,
    ) -> Result<SessionInfo, AcpError> {
        let result =
            self.inner
                .request("session/new", session_params(cwd, Some(mcp), None), timeout)?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| AcpError::Protocol("session/new 缺少 sessionId".into()))?
            .to_owned();
        Ok(SessionInfo {
            session_id,
            extension_results: result
                .pointer("/_meta/extensionResults")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
            current_mode: result
                .pointer("/modes/currentModeId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// 恢复已有 session；agent 不支持 `session/load` 时返回 `false`。
    pub fn load_session(
        &self,
        id: &str,
        cwd: &Path,
        mcp: &McpServerRef<'_>,
        timeout: Duration,
    ) -> Result<bool, AcpError> {
        let value = self.inner.request(
            "session/load",
            session_params(cwd, Some(mcp), Some(id)),
            timeout,
        );
        match value {
            Ok(_) => Ok(true),
            // 只有 JSON-RPC MethodNotFound 才表示 agent 不支持 session/load。
            Err(AcpError::Rpc { code: -32601, .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// 发送 prompt；完成事件经 [`AcpClient::drain`] 的 `PromptDone` 返回。
    pub fn prompt(&self, session_id: &str, text: &str) -> Result<(), AcpError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.prompts.lock().unwrap().insert(id);
        let write = self.inner.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [ { "type": "text", "text": text } ],
            },
        }));
        if let Err(error) = write {
            self.inner.prompts.lock().unwrap().remove(&id);
            return Err(error);
        }
        Ok(())
    }

    pub fn cancel(&self, session_id: &str) {
        let _ = self.inner.write_json(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id },
        }));
    }

    pub fn drain(&self) -> Vec<AcpEvent> {
        // 正文事件先出队，控制事件追加在后（PromptDone/ProtocolError 语义上
        // 总是晚于本批正文）；控制事件不受正文队列溢出影响。
        let mut events: Vec<AcpEvent> = {
            let mut queue = self.inner.events.lock().unwrap();
            queue.drain(..).collect()
        };
        {
            let mut critical = self.inner.critical.lock().unwrap();
            events.extend(critical.drain(..));
        }
        *self.inner.overflowed.lock().unwrap() = false;
        *self.inner.critical_overflowed.lock().unwrap() = false;
        events
    }

    pub fn pid(&self) -> u32 {
        self.process.lock().unwrap().pid()
    }

    /// Whether the process is bounded by the sandbox containment handle.
    /// Every MXC spawn attaches a process-tree teardown handle or fails closed.
    pub fn has_job(&self) -> bool {
        true
    }

    pub fn close_stdin(&self) {
        let _ = self.inner.stdin.lock().unwrap().take();
    }

    pub fn try_wait(&self) -> Result<Option<i32>, AcpError> {
        let code = self
            .process
            .lock()
            .unwrap()
            .try_wait()
            .map_err(|error| AcpError::Io(error.to_string()))?;
        if let Some(code) = code
            && !self.inner.exited.swap(true, Ordering::Relaxed)
        {
            self.inner.mark_closed();
            self.inner.push_event(AcpEvent::Exited(Some(code)));
        }
        Ok(code)
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Result<Option<i32>, AcpError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(code) = self.try_wait()? {
                return Ok(Some(code));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 任意 stdout 协议行的最近时间（watchdog 用；包含未映射的 update）。
    pub fn last_activity_unix_ms(&self) -> u64 {
        self.inner.last_activity_ms.load(Ordering::Relaxed)
    }

    /// stderr 尾部（已脱敏）。
    pub fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    /// 停止进程树（仅工作线程调用）：`TerminateJobObject` 杀掉整棵树后
    /// 有界等待主进程退出；句柄由 `SandboxedProcess` 的 `Drop` 关闭，
    /// `KILL_ON_JOB_CLOSE` 对残留进程兜底。
    pub fn kill(&self) {
        let process = self.process.lock().unwrap();
        // `terminate` 内部在 Job 调用失败时退回 TerminateProcess，因此不需要
        // 第二条 `taskkill /T /F` 路径。
        let _ = process.terminate();
        let _ = process.wait_timeout(JOB_CLOSE_REAP_TIMEOUT);
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.close_stdin();
        // The `SandboxedProcess` field's Drop terminates the Job tree and waits
        // bounded; there is no unsandboxed fallback path to run here.
    }
}

fn session_params(cwd: &Path, mcp: Option<&McpServerRef<'_>>, session_id: Option<&str>) -> Value {
    let mut params = json!({
        "cwd": cwd.to_string_lossy(),
        "mcpServers": mcp.map_or_else(Vec::new, |mcp| {
            vec![json!({
                "type": "http",
                "name": mcp.name,
                "url": mcp.url,
                "headers": [
                    { "name": "Authorization", "value": format!("Bearer {}", mcp.token) },
                ],
            })]
        }),
    });
    if let Some(session_id) = session_id {
        params["sessionId"] = Value::String(session_id.to_owned());
    }
    params
}

fn read_stdout(stdout: impl Read, inner: Arc<AcpInner>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_bounded_line(&mut reader, MAX_STDOUT_LINE) {
            Ok(LineChunk::Eof) => break,
            Ok(LineChunk::Overflow) => inner.push_event(AcpEvent::ProtocolError(format!(
                "stdout 单行超过 {MAX_STDOUT_LINE} 字节，已丢弃"
            ))),
            Ok(LineChunk::Line(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                let text = text.trim();
                if !text.is_empty() {
                    inner.handle_stdout_line(text);
                }
            }
            Err(error) => {
                inner.push_event(AcpEvent::ProtocolError(format!(
                    "读取 stdout 失败: {error}"
                )));
                break;
            }
        }
    }
    // EOF and read errors both mean no further JSON-RPC response can arrive:
    // wake every waiting request immediately instead of letting it time out.
    inner.mark_closed();
}

fn read_stderr(stderr: impl Read, inner: Arc<AcpInner>) {
    let mut stderr = stderr;
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => inner.stderr.lock().unwrap().push(&chunk[..read]),
            Err(_) => break,
        }
    }
}

enum LineChunk {
    Line(Vec<u8>),
    Overflow,
    Eof,
}

fn read_bounded_line<R: BufRead>(reader: &mut R, cap: usize) -> std::io::Result<LineChunk> {
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(if overflow {
                LineChunk::Overflow
            } else if line.is_empty() {
                LineChunk::Eof
            } else {
                LineChunk::Line(line)
            });
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(buffer.len(), |index| index + 1);
        if !overflow {
            if line.len() + take > cap {
                overflow = true;
                line.clear();
            } else {
                line.extend_from_slice(&buffer[..take]);
            }
        }
        reader.consume(take);
        if newline.is_some() {
            return Ok(if overflow {
                LineChunk::Overflow
            } else {
                LineChunk::Line(line)
            });
        }
    }
}

/// 脱敏：已知密钥值、`Bearer <token>`、`sk-…`。
pub(crate) fn redact_secrets(text: &str, secrets: &[String]) -> String {
    let mut output = text.to_owned();
    for secret in secrets {
        if secret.len() >= 4 {
            output = output.replace(secret, "[REDACTED]");
        }
    }
    output = redact_prefixed(&output, "bearer ", |byte| {
        !(byte.is_ascii_whitespace() || byte == b'"' || byte == b'\'' || byte == b',')
    });
    redact_prefixed(&output, "sk-", |byte| {
        byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
    })
}

fn redact_prefixed(text: &str, prefix: &str, is_value: impl Fn(u8) -> bool) -> String {
    let lower = text.to_ascii_lowercase();
    let mut output = String::with_capacity(text.len());
    let mut start = 0;
    while let Some(relative) = lower[start..].find(prefix) {
        let hit = start + relative;
        let value_start = hit + prefix.len();
        let mut value_end = value_start;
        let bytes = text.as_bytes();
        while value_end < bytes.len() && is_value(bytes[value_end]) {
            value_end += 1;
        }
        output.push_str(&text[start..value_start]);
        if value_end > value_start {
            output.push_str("[REDACTED]");
            start = value_end;
        } else {
            start = value_start;
        }
    }
    output.push_str(&text[start..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn test_inner() -> Arc<AcpInner> {
        Arc::new(AcpInner {
            stdin: Mutex::new(None),
            events: Mutex::new(VecDeque::new()),
            critical: Mutex::new(VecDeque::new()),
            critical_overflowed: Mutex::new(false),
            overflowed: Mutex::new(false),
            pending: Mutex::new(HashMap::new()),
            prompts: Mutex::new(HashSet::new()),
            next_id: AtomicU64::new(1),
            last_activity_ms: AtomicU64::new(now_ms()),
            exited: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            stderr: Mutex::new(StderrState::new(1024)),
            secrets: vec!["sk-top-secret".into()],
        })
    }

    #[test]
    fn inherited_env_is_system_only() {
        for required in [
            "PATH",
            "SystemRoot",
            "SystemDrive",
            "windir",
            "COMSPEC",
            "PATHEXT",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
        ] {
            assert!(
                INHERITED_ENV
                    .iter()
                    .any(|key| key.eq_ignore_ascii_case(required)),
                "缺少系统变量 {required}"
            );
        }
        for forbidden in USER_SCOPE_ENV {
            assert!(
                !INHERITED_ENV
                    .iter()
                    .any(|key| key.eq_ignore_ascii_case(forbidden)),
                "不得继承用户目录/TEMP 键 {forbidden}"
            );
        }
    }

    #[test]
    fn bounded_line_splits_and_flags_overflow() {
        let mut reader = Cursor::new(b"one\ntwo\n".to_vec());
        match read_bounded_line(&mut reader, 16).unwrap() {
            LineChunk::Line(line) => assert_eq!(line, b"one\n"),
            _ => panic!("expected line"),
        }
        let mut reader = Cursor::new(vec![b'x'; 32]);
        assert!(matches!(
            read_bounded_line(&mut reader, 16).unwrap(),
            LineChunk::Overflow
        ));
    }

    #[test]
    fn event_queue_drops_oldest_and_notes_overflow_once() {
        let inner = test_inner();
        for index in 0..=EVENT_QUEUE_CAPACITY {
            inner.push_event(AcpEvent::AgentChunk(index.to_string()));
        }
        {
            let events = inner.events.lock().unwrap();
            assert_eq!(events.len(), EVENT_QUEUE_CAPACITY);
            assert!(
                matches!(events.front(), Some(AcpEvent::AgentChunk(text)) if text == "1"),
                "只丢最旧的一条：{:?}",
                events.front()
            );
        }
        let critical = inner.critical.lock().unwrap();
        assert_eq!(critical.len(), 1);
        assert!(matches!(
            critical.front(),
            Some(AcpEvent::ProtocolError(text)) if text.contains("溢出")
        ));
    }

    #[test]
    fn critical_events_survive_body_overflow() {
        let inner = test_inner();
        for index in 0..EVENT_QUEUE_CAPACITY * 2 {
            inner.push_event(AcpEvent::AgentChunk(index.to_string()));
        }
        inner.push_event(AcpEvent::PromptDone {
            stop_reason: "end_turn".into(),
        });
        inner.push_event(AcpEvent::Exited(Some(0)));
        let events: Vec<AcpEvent> = {
            let mut body = inner.events.lock().unwrap();
            let mut all: Vec<AcpEvent> = body.drain(..).collect();
            let mut critical = inner.critical.lock().unwrap();
            all.extend(critical.drain(..));
            all
        };
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AcpEvent::PromptDone { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AcpEvent::Exited(Some(0))))
        );
        let overflow_notes = events
            .iter()
            .filter(|event| matches!(event, AcpEvent::ProtocolError(text) if text.contains("溢出")))
            .count();
        assert_eq!(overflow_notes, 1);
    }

    #[test]
    fn critical_queue_overflow_fails_closed() {
        let inner = test_inner();
        for _ in 0..=CRITICAL_QUEUE_CAPACITY {
            inner.push_event(AcpEvent::PromptDone {
                stop_reason: "end_turn".into(),
            });
        }
        let critical = inner.critical.lock().unwrap();
        assert_eq!(critical.len(), 1);
        assert!(matches!(
            critical.front(),
            Some(AcpEvent::ProtocolError(text)) if text.starts_with(CRITICAL_OVERFLOW_PREFIX)
        ));
    }

    #[test]
    fn initialize_protocol_version_must_be_one() {
        assert_eq!(
            check_protocol_version(&json!({"protocolVersion": 1})).unwrap(),
            1
        );
        assert!(check_protocol_version(&json!({"protocolVersion": 2})).is_err());
        assert!(check_protocol_version(&json!({})).is_err());
    }

    #[test]
    fn mark_closed_wakes_every_pending_request() {
        let inner = test_inner();
        let (sender, receiver) = mpsc::channel();
        inner.pending.lock().unwrap().insert(5, sender);
        inner.mark_closed();
        assert!(inner.closed.load(Ordering::SeqCst));
        assert!(
            matches!(receiver.recv().unwrap(), Err(AcpError::Closed)),
            "pending request 必须立即收到 Closed"
        );
        assert!(inner.pending.lock().unwrap().is_empty());
        // Idempotent: a second EOF/exit must not panic or re-send.
        inner.mark_closed();
    }

    #[test]
    fn json_rpc_error_keeps_code_for_load_session() {
        let inner = test_inner();
        let (sender, receiver) = mpsc::channel();
        inner.pending.lock().unwrap().insert(9, sender);
        inner.handle_stdout_line(
            &json!({
                "jsonrpc": "2.0",
                "id": 9,
                "error": { "code": -32601, "message": "method not found" },
            })
            .to_string(),
        );
        match receiver.recv().unwrap() {
            Err(AcpError::Rpc { code, .. }) => assert_eq!(code, -32601),
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }

    #[test]
    fn permission_is_denied_and_fs_terminal_are_method_not_found() {
        let inner = test_inner();
        let id = json!(7);
        let permission = inner.agent_request_response(
            &id,
            "session/request_permission",
            &json!({"params":{"toolCall":{"title":"write file"}}}),
        );
        assert_eq!(permission["result"]["outcome"]["outcome"], "cancelled");
        let fs = inner.agent_request_response(&id, "fs/read_text_file", &json!({}));
        assert_eq!(fs["error"]["code"], -32601);
        let terminal = inner.agent_request_response(&id, "terminal/create", &json!({}));
        assert_eq!(terminal["error"]["code"], -32601);
        let events = inner.events.lock().unwrap();
        assert!(events.iter().any(
            |event| matches!(event, AcpEvent::PermissionDenied { title } if title == "write file")
        ));
    }

    #[test]
    fn redacts_known_secret_bearer_and_sk_tokens() {
        let text = "key=sk-top-secret Authorization: Bearer dth-123456 other sk-abcdefghi";
        let redacted = redact_secrets(text, &["sk-top-secret".into()]);
        assert!(!redacted.contains("sk-top-secret"));
        assert!(!redacted.contains("dth-123456"), "{redacted}");
        assert!(!redacted.contains("sk-abcdefghi"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn parses_agent_chunks_and_tool_calls() {
        let inner = test_inner();
        inner.handle_stdout_line(
            &json!({
                "jsonrpc":"2.0","method":"session/update",
                "params":{"sessionId":"s","update":{
                    "sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"hello"},
                }},
            })
            .to_string(),
        );
        inner.handle_stdout_line(&json!({
            "jsonrpc":"2.0","method":"session/update",
            "params":{"sessionId":"s","update":{
                "sessionUpdate":"tool_call","toolCallId":"t1","title":"serial_read","status":"pending",
            }},
        }).to_string());
        let events = inner.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| *event == AcpEvent::AgentChunk("hello".into()))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AcpEvent::ToolCall { id, title, status }
                if id == "t1" && title == "serial_read" && status == "pending"
        )));
    }
}
