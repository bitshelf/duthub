//! CLI profile adapter (W2c): Claude Code argv, NDJSON event stream, resume and
//! the per-turn process lifecycle.
//!
//! Unlike an ACP Agent, a CLI profile is not a long-lived server: every user
//! turn spawns one short-lived process (`claude -p ...`) whose stdout is a
//! newline-delimited JSON stream. stdout is protocol-only; stderr is collected
//! with a bounded, redacted tail and never mixed into the transcript.
//!
//! Scope of this wave:
//! - one real profile (`claude-code`); `start_entry` is the additive entry,
//!   `HostManager::start_with_agent` keeps rejecting CLI profiles as before;
//! - argv is fixed by the profile (plan permission mode, strict MCP config);
//! - the duthub MCP token never appears in argv or in the MCP config file: the
//!   config expands `${DUTHUB_AGENT_TOKEN}` and the value is passed via the
//!   child environment only;
//! - resume state reuses the ACP `resume.json` shape, so switching between an
//!   ACP Agent and a CLI profile invalidates the other side's marker.
//!
//! Not covered here (later waves): copying/mounting the user's Claude login
//! into a persistent `CLAUDE_CONFIG_DIR` (T29), probing the real locked
//! Claude version, and product wiring (W4b).
//!
//! Verified against Claude Code 2.1.204 (2026-11-21): the profile flags exist,
//! `${DUTHUB_AGENT_TOKEN}` is expanded in the HTTP header, and `claude mcp
//! list` connects to the session listener. The real CLI currently rejects the
//! tool list because rmcp 3.10.0 emits a non-object tool `outputSchema`; that
//! fix belongs to duthub-mcp, not this adapter.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use duthub_core::agent_access::AgentAccessRuntime;
use duthub_core::agent_config::AgentDefinition;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::state::McpState;
use duthub_sandbox::{
    CellSpec, NetworkPolicy, SandboxPolicy, SandboxedProcess, SpawnRequest, spawn_streaming,
};
use serde_json::{Value, json};

use super::acp::redact_secrets;
use super::host::{self, HostEvents, PROMPT_HANG_TIMEOUT, WorkerCommand};
use super::listener::{self, SessionListener};
use super::registry::{self, AgentLaunchSpec};
use super::sandbox::{self, AgentPaths};
use super::session::{SessionStatus, SessionView, TranscriptEntry, TranscriptKind};
use super::transcript::TranscriptWriter;

pub const CLAUDE_CODE_PROFILE: &str = "claude-code";
/// Environment variable that carries the per-session read-only MCP token.
pub const MCP_TOKEN_ENV: &str = "DUTHUB_AGENT_TOKEN";
/// File name of the generated MCP config inside the per-DUT sandbox.
pub const MCP_CONFIG_FILE_NAME: &str = "duthub-mcp.json";

const MAX_STDOUT_LINE: usize = 4 * 1024 * 1024;
const STDOUT_QUEUE_CAPACITY: usize = 1024;
const DEFAULT_STDERR_CAP: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const STDERR_POLL_INTERVAL: Duration = Duration::from_millis(200);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded wait for the stdout pipe to reach EOF after the child exited.
const STDOUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Tool results can be large (a serial capture); keep the transcript readable.
const TOOL_RESULT_PREVIEW: usize = 4000;
/// The 13 read-only Plan tools the duthub MCP listener exposes (shared with `duthub-mcp`).
pub const PLAN_TOOLS: &[&str] = duthub_mcp::PLAN_TOOL_NAMES;
/// Only used when a caller injects an MCP URL instead of issuing a listener.
const TEST_MCP_TOKEN: &str = "dth-test-token";

/// System variables a CLI child may inherit.
///
/// A CLI profile authenticates as the user (`auth_mode=inherit`), so the user
/// profile keys are inherited on purpose: Claude Code looks up its own login
/// there. T29 will move credentials into a per-session config directory and
/// then shrink this list. `TEMP`/`TMP` are overwritten with the per-session
/// sandbox path after the inherited set is applied.
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
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
];

/// One turn's launch plan. `env` deliberately does **not** contain the MCP
/// token; the worker appends [`MCP_TOKEN_ENV`] right before spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mcp_config: PathBuf,
    pub stdin_prompt: String,
}

/// Events parsed from one `stream-json` stdout line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliEvent {
    /// `{"type":"system","subtype":"init",...}` (init, status, hook results).
    System {
        subtype: Option<String>,
        session_id: Option<String>,
        text: String,
        /// MCP servers that failed to start/connect (`mcp_server_errors` and
        /// failed `mcp_servers[]` statuses). Empty when all servers are fine.
        mcp_errors: Vec<String>,
    },
    /// `{"type":"assistant","message":{"content":[...]}}`.
    Assistant {
        text: String,
        session_id: Option<String>,
    },
    /// `{"type":"user",...}`. In headless mode this carries tool results
    /// (`tool_result` blocks); a plain echoed prompt is also possible.
    User {
        text: String,
        tool_result: bool,
        is_error: bool,
    },
    /// `{"type":"result",...}`: the only event that carries a resumable id.
    Result {
        subtype: Option<String>,
        session_id: Option<String>,
        is_error: bool,
        text: String,
        /// Literal `permission_denials[]` entries (plan mode denials).
        permission_denials: Vec<String>,
    },
    /// `{"type":"error",...}` or an error-shaped object.
    Error { message: String },
}

/// Worker-side context for one CLI session. Fields mirror the ACP worker so
/// `HostManager` can reuse its stop/tick/prompt plumbing.
pub(crate) struct CliWorkerContext {
    pub(crate) dut_id: String,
    pub(crate) agent: AgentDefinition,
    pub(crate) work_dir: PathBuf,
    pub(crate) view: Arc<SessionView>,
    pub(crate) state: Arc<dyn McpState>,
    pub(crate) access: Arc<AccessContext>,
    pub(crate) auth: Arc<AgentAccessRuntime>,
    pub(crate) events: Option<Arc<dyn HostEvents>>,
    pub(crate) launch: Arc<Mutex<Option<AgentLaunchSpec>>>,
    pub(crate) listener_slot: Arc<Mutex<Option<SessionListener>>>,
    pub(crate) pending: Arc<AtomicBool>,
    pub(crate) last_activity_ms: Arc<AtomicU64>,
    pub(crate) stop_flag: Arc<AtomicBool>,
    pub(crate) dirty: Arc<AtomicBool>,
    /// Test-only override; the product path always issues a per-session
    /// loopback listener.
    pub(crate) mcp_url_override: Option<String>,
}

/// Synchronous pre-flight used by `HostManager::start_entry` on the UI thread.
pub fn ensure_cli_launchable(agent: &AgentDefinition) -> Result<(), String> {
    if !agent.enabled {
        return Err(format!("Agent {} 已停用", agent.id));
    }
    if agent.kind != duthub_core::agent_config::AgentKind::Cli {
        return Err(format!("Agent {} 不是 CLI profile", agent.id));
    }
    match agent
        .cli_profile
        .as_deref()
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
    {
        Some(CLAUDE_CODE_PROFILE) => {}
        Some(other) => {
            return Err(format!(
                "暂不支持 CLI profile {other}；本轮实现 {CLAUDE_CODE_PROFILE}"
            ));
        }
        None => {
            return Err(format!(
                "Agent {} 是 CLI profile 但未配置 cliProfile；请选择 {CLAUDE_CODE_PROFILE}",
                agent.id
            ));
        }
    }
    let raw = agent
        .program_path()
        .ok_or_else(|| format!("Agent {} 缺少 program（CLI 不支持内嵌）", agent.id))?;
    if !registry::is_absolute_program(raw) {
        return Err(format!("CLI 程序必须是绝对路径: {raw}"));
    }
    expected_sha256(agent)?;
    validate_custom_args(&agent.args)
        .map_err(|reason| format!("Agent {} 的 args 无效: {reason}", agent.id))
}

/// Custom args are an allowlist: only model/effort selection is accepted.
///
/// A bare positional would be treated by Claude as the prompt (overriding the
/// stdin turn), so positionals are rejected. Flags that change identity
/// (`--session-id`, `--fork-session`), persistence (`--no-session-persistence`),
/// sandbox/settings, prompts, permissions, or debugging are rejected either by
/// [`is_reserved_cli_arg`] (clear message) or by not being on the allowlist.
fn validate_custom_args(args: &[String]) -> Result<(), String> {
    const VALUE_FLAGS: &[&str] = &[
        "--model",
        "--effort",
        "--fallback-model",
        "--max-budget-usd",
    ];
    const SWITCH_FLAGS: &[&str] = &["--verbose"];
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg.contains('\0') || arg.contains('\r') || arg.contains('\n') {
            return Err("自定义参数含控制字符".into());
        }
        if is_reserved_cli_arg(arg) {
            return Err(format!("不能覆盖 duthub 固定的 CLI 参数: {arg}"));
        }
        if let Some((name, value)) = arg.split_once('=') {
            if VALUE_FLAGS.contains(&name) {
                if value.is_empty() {
                    return Err(format!("参数 {name} 缺少值"));
                }
                index += 1;
                continue;
            }
            if SWITCH_FLAGS.contains(&name) {
                return Err(format!("参数 {name} 不接受值"));
            }
            return Err(format!("不支持的 CLI 自定义参数: {arg}"));
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            let Some(value) = args.get(index + 1) else {
                return Err(format!("参数 {arg} 缺少值"));
            };
            if value.starts_with('-') {
                return Err(format!("参数 {arg} 缺少值（下一项 {value} 是另一个参数）"));
            }
            index += 2;
            continue;
        }
        if SWITCH_FLAGS.contains(&arg.as_str()) {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("不支持的 CLI 自定义参数: {arg}"));
        }
        return Err(format!(
            "位置参数 {arg} 会被 Claude 当作 prompt 并覆盖 stdin 轮次，已拒绝"
        ));
    }
    Ok(())
}

/// Build the Claude Code launch plan for a fresh turn.
pub fn claude_code_plan(
    agent: &AgentDefinition,
    sandbox: &Path,
    mcp_url: &str,
) -> Result<CliPlan, String> {
    claude_code_plan_with_resume(agent, sandbox, mcp_url, None)
}

/// Build the Claude Code launch plan; `resume` adds `--resume <session id>`.
pub fn claude_code_plan_with_resume(
    agent: &AgentDefinition,
    sandbox: &Path,
    mcp_url: &str,
    resume: Option<&str>,
) -> Result<CliPlan, String> {
    ensure_cli_launchable(agent)?;
    if !sandbox.is_dir() {
        return Err(format!("CLI 沙箱目录不存在: {}", sandbox.display()));
    }
    validate_mcp_url(mcp_url)?;
    let raw = agent
        .program_path()
        .ok_or_else(|| format!("Agent {} 缺少 program", agent.id))?;
    let program = PathBuf::from(raw);
    verify_program_hash(&program, agent)?;
    let mcp_config = sandbox.join(MCP_CONFIG_FILE_NAME);
    write_mcp_config(&mcp_config, mcp_url)?;

    let mut args = vec![
        "-p".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--permission-mode".to_owned(),
        "plan".to_owned(),
    ];
    if let Some(session_id) = resume.map(str::trim).filter(|id| !id.is_empty()) {
        if !is_valid_session_id(session_id) {
            return Err(format!("CLI resume session id 不合法: {session_id}"));
        }
        args.push("--resume".to_owned());
        args.push(session_id.to_owned());
    }
    args.push("--mcp-config".to_owned());
    args.push(mcp_config.display().to_string());
    args.push("--strict-mcp-config".to_owned());
    // User args were checked by `ensure_cli_launchable`; they may add flags
    // such as `--model` but cannot override the profile's fixed arguments.
    args.extend(agent.args.iter().cloned());

    Ok(CliPlan {
        program,
        args,
        env: Vec::new(),
        mcp_config,
        stdin_prompt: String::new(),
    })
}

/// Parse one `claude --output-format stream-json` line.
///
/// Non-JSON lines, JSON arrays, and unknown event types return `None`: they are
/// diagnostics, not part of the profile's event contract.
pub fn parse_claude_event(line: &str) -> Option<CliEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let kind = value.get("type").and_then(Value::as_str)?;
    let session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    match kind {
        "system" => {
            let subtype = value
                .get("subtype")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut text = subtype.clone().unwrap_or_else(|| "event".to_owned());
            if let Some(model) = value.get("model").and_then(Value::as_str) {
                text.push_str(&format!(" · model={model}"));
            }
            let mut mcp_errors = Vec::new();
            if let Some(servers) = value.get("mcp_servers").and_then(Value::as_array) {
                let labels: Vec<String> = servers
                    .iter()
                    .map(|server| {
                        let name = server
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("<unnamed>");
                        if let Some(error) = server.get("error") {
                            mcp_errors.push(format!("{name}: {}", summarize_json(error)));
                        }
                        match server.get("status").and_then(Value::as_str) {
                            Some(status) => {
                                if looks_like_mcp_failure(status) {
                                    mcp_errors.push(format!("{name}: status={status}"));
                                }
                                format!("{name}={status}")
                            }
                            None => name.to_owned(),
                        }
                    })
                    .collect();
                text.push_str(&format!(" · MCP [{}]", labels.join(", ")));
            }
            if let Some(tools) = value.get("tools").and_then(Value::as_array) {
                // 2.1.204 emits strings, but tolerate `{ "name": ... }` objects
                // so the live gate is not tied to one serialization shape.
                let names: Vec<&str> = tools.iter().filter_map(tool_name).collect();
                text.push_str(&format!(" · tools={}", names.len()));
                let duthub: Vec<&str> = names
                    .iter()
                    .copied()
                    .filter(|name| is_duthub_tool_name(name))
                    .collect();
                if !duthub.is_empty() {
                    text.push_str(&format!(
                        " · duthub 工具 {} 个: {}",
                        duthub.len(),
                        duthub.join(", ")
                    ));
                }
            }
            mcp_errors.extend(parse_mcp_server_errors(value.get("mcp_server_errors")));
            if !mcp_errors.is_empty() {
                text.push_str(&format!(" · MCP 错误: {}", mcp_errors.join("; ")));
            }
            Some(CliEvent::System {
                subtype,
                session_id,
                text,
                mcp_errors,
            })
        }
        "assistant" => {
            let mut texts = Vec::new();
            let mut tools = Vec::new();
            if let Some(message) = value.get("message") {
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str)
                                    && !text.is_empty()
                                {
                                    texts.push(text.to_owned());
                                }
                            }
                            Some("tool_use") => {
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("<tool>");
                                tools.push(format!("[tool] {name}"));
                            }
                            _ => {}
                        }
                    }
                } else if let Some(text) = message.as_str() {
                    texts.push(text.to_owned());
                }
            }
            if texts.is_empty() && tools.is_empty() {
                return None;
            }
            texts.extend(tools);
            Some(CliEvent::Assistant {
                text: texts.join("\n"),
                session_id,
            })
        }
        // Headless `stream-json` emits `user` events for tool results (and,
        // when replaying, for user text). They are valid NDJSON, not protocol
        // noise: parse them so tool results stay visible in the transcript.
        "user" => {
            let mut texts = Vec::new();
            let mut tool_result = false;
            let mut is_error = false;
            if let Some(message) = value.get("message") {
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("tool_result") => {
                                tool_result = true;
                                is_error |= block
                                    .get("is_error")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                texts.push(content_text(block.get("content")));
                            }
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str)
                                    && !text.is_empty()
                                {
                                    texts.push(text.to_owned());
                                }
                            }
                            _ => {}
                        }
                    }
                } else if let Some(text) = message.as_str() {
                    texts.push(text.to_owned());
                }
            }
            Some(CliEvent::User {
                text: texts.join("\n"),
                tool_result,
                is_error,
            })
        }
        "result" => {
            let subtype = value
                .get("subtype")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let is_error = value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || subtype
                    .as_deref()
                    .is_some_and(|subtype| subtype.contains("error"));
            let mut text = value
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            // Error subtypes carry the diagnostic strings in `errors[]`.
            if let Some(errors) = value.get("errors").and_then(Value::as_array) {
                let joined: Vec<&str> = errors.iter().filter_map(Value::as_str).collect();
                if !joined.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&joined.join("\n"));
                }
            }
            let permission_denials = value
                .get("permission_denials")
                .and_then(Value::as_array)
                .map(|entries| entries.iter().map(describe_permission_denial).collect())
                .unwrap_or_default();
            Some(CliEvent::Result {
                subtype,
                session_id,
                is_error,
                text,
                permission_denials,
            })
        }
        "error" => {
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .or_else(|| value.get("error").and_then(Value::as_str))
                .unwrap_or(line)
                .to_owned();
            Some(CliEvent::Error { message })
        }
        _ => None,
    }
}

/// Accept both `"tool"` and `{"name": "tool"}` entries in `system/init.tools`.
fn tool_name(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("name").and_then(Value::as_str))
}

/// Flatten a tool-result/text `content` value (string or content-block array).
fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => summarize_json(other),
    }
}

fn summarize_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".into())
        }
        other => other.to_string(),
    }
}

/// Normalise `mcp_server_errors` regardless of whether the CLI emits a map,
/// an array of `{name,error}` objects, or plain strings.
fn parse_mcp_server_errors(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    match value {
        Value::Array(entries) => entries.iter().map(describe_mcp_error).collect(),
        Value::Object(map) => map
            .iter()
            .map(|(name, detail)| format!("{name}: {}", summarize_json(detail)))
            .collect(),
        Value::String(text) => vec![text.clone()],
        Value::Null => Vec::new(),
        other => vec![summarize_json(other)],
    }
}

fn describe_mcp_error(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    let name = value
        .get("name")
        .or_else(|| value.get("server"))
        .or_else(|| value.get("mcp_server"))
        .and_then(Value::as_str);
    let detail = value
        .get("error")
        .or_else(|| value.get("message"))
        .map(summarize_json)
        .unwrap_or_else(|| summarize_json(value));
    match name {
        Some(name) => format!("{name}: {detail}"),
        None => detail,
    }
}

fn looks_like_mcp_failure(status: &str) -> bool {
    let status = status.to_ascii_lowercase();
    ["fail", "error", "unavailable", "needs-auth", "needs_auth"]
        .iter()
        .any(|needle| status.contains(needle))
}

fn describe_permission_denial(value: &Value) -> String {
    let tool = value
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("<tool>");
    let id = value
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input = value
        .get("tool_input")
        .map(|input| truncate_chars(&summarize_json(input), 300))
        .unwrap_or_default();
    let mut text = tool.to_owned();
    if !id.is_empty() {
        text.push_str(&format!("（{id}）"));
    }
    if !input.is_empty() {
        text.push(' ');
        text.push_str(&input);
    }
    text
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

/// Match both the bare Plan tool name and Claude's `mcp__<server>__<tool>`
/// naming; the init event reports the connected MCP tool surface.
fn is_duthub_tool_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("duthub")
        || PLAN_TOOLS
            .iter()
            .any(|tool| name == *tool || name.ends_with(&format!("__{tool}")))
}

/// Per-session worker: validate, issue the MCP listener, then loop over prompts.
///
/// The worker only returns `Err` for fatal setup failures; a failed turn is
/// recorded in the transcript and leaves the session `Ready` for a retry.
pub(crate) fn run_cli_worker(
    context: CliWorkerContext,
    rx: Receiver<WorkerCommand>,
) -> Result<(), String> {
    let result = run_cli_session(&context, &rx);
    if let Err(error) = &result {
        let message = redact_secrets(error, &[]);
        if context.stop_flag.load(Ordering::Relaxed) {
            context.view.stop_normally();
        } else {
            context
                .view
                .push_entry(TranscriptEntry::error(message.clone()));
            if context.view.fail(message.clone())
                && let Some(events) = &context.events
            {
                events.on_failed(&context.dut_id, &message);
            }
        }
        if let Some(listener) = context.listener_slot.lock().unwrap().take() {
            listener::revoke(listener, &context.auth);
        }
        context.pending.store(false, Ordering::Relaxed);
        context.dirty.store(true, Ordering::Relaxed);
    }
    result
}

fn run_cli_session(context: &CliWorkerContext, rx: &Receiver<WorkerCommand>) -> Result<(), String> {
    let paths =
        sandbox::prepare(&context.work_dir, &context.dut_id).map_err(|error| error.to_string())?;
    let probe_status = duthub_sandbox::status();
    if !probe_status.supports_agent_isolation() {
        return Err(format!(
            "OS 沙箱后端不可用或缺少必需隔离能力，已拒绝启动 CLI Agent（{}）",
            probe_status.summary()
        ));
    }
    let launch = resolve_cli_launch(&context.agent)?;
    *context.launch.lock().unwrap() = Some(launch.clone());
    if context.stop_flag.load(Ordering::Relaxed) {
        finalize_stopped(context);
        return Ok(());
    }
    let mut extra_writable_owned: Vec<PathBuf> = Vec::new();
    if context.agent.auth_mode.eq_ignore_ascii_case("inherit")
        && let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    {
        let claude_dir = PathBuf::from(home).join(".claude");
        if claude_dir.is_dir() {
            extra_writable_owned.push(claude_dir);
        }
    }
    let extra_writable: Vec<&Path> = extra_writable_owned.iter().map(PathBuf::as_path).collect();
    // Grant read-only access to the CLI program directory so `.cmd`/`.bat`
    // shims launched through `cmd.exe /D /C call <shim>` stay readable inside
    // the deny-by-default container.
    let extra_readable: Vec<&Path> = launch.program.parent().into_iter().collect();
    let sandbox_state = sandbox::prepare_agent_sandbox(
        &context.work_dir,
        &context.dut_id,
        &paths,
        launch.program.parent(),
        &extra_writable,
        &extra_readable,
        NetworkPolicy::LoopbackAndOutbound { proxy_url: None },
    )?;
    context.view.push_entry(TranscriptEntry::system(format!(
        "OS 隔离状态：{}",
        sandbox_state.status.summary()
    )));
    context.view.set_os_isolation(sandbox_state.status.clone());
    context.dirty.store(true, Ordering::Relaxed);

    let (port, token, url) = if let Some(url) = &context.mcp_url_override {
        (None, TEST_MCP_TOKEN.to_owned(), url.clone())
    } else {
        let listener = listener::issue(
            context.state.clone(),
            context.access.clone(),
            context.auth.clone(),
            &context.dut_id,
        )
        .map_err(|error| error.to_string())?;
        let port = listener.port;
        let token = listener.token.clone();
        context
            .view
            .set_credential_id(listener.credential_id.clone());
        *context.listener_slot.lock().unwrap() = Some(listener);
        (Some(port), token, format!("http://127.0.0.1:{port}/mcp"))
    };
    if context.stop_flag.load(Ordering::Relaxed) {
        finalize_stopped(context);
        return Ok(());
    }

    let resume = read_cli_resume(&paths, &launch, context);
    if let Some(session_id) = &resume {
        context.view.set_session_id(session_id.clone());
    }
    if let Some(port) = port {
        context.view.set_port(port);
    }
    let ready = context
        .view
        .try_transition(&SessionStatus::Starting, SessionStatus::Ready);
    if !ready {
        // The user stopped while we were setting up.
        finalize_stopped(context);
        return Ok(());
    }
    let auth_detail = if context.agent.auth_mode.eq_ignore_ascii_case("inherit") {
        "继承本机 CLI 登录"
    } else {
        "CLI 自带凭据"
    };
    context.view.push_entry(TranscriptEntry::system(format!(
        "Agent 会话已启动（{} · Claude Code CLI · {auth_detail} · MCP {url}）",
        launch.agent_name
    )));
    if let Some(events) = &context.events {
        events.on_started(&context.dut_id, resume.as_deref().unwrap_or("cli"));
    }
    context.dirty.store(true, Ordering::Relaxed);

    let transcript = TranscriptWriter::open(&paths.transcript, context.view.clone());
    // The loop only ends on stop/cancel/disconnect: a CLI worker is idle
    // between turns and never exits on its own.
    loop {
        if context.stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match rx.try_recv() {
            Ok(WorkerCommand::Prompt(prompt)) => {
                let turn = TurnContext {
                    context,
                    paths: &paths,
                    sandbox: &sandbox_state,
                    launch: &launch,
                    mcp_url: &url,
                    mcp_token: &token,
                    prompt: &prompt,
                    transcript: &transcript,
                };
                record(
                    &turn,
                    TranscriptEntry::new(TranscriptKind::User, prompt.clone()),
                );
                context.pending.store(true, Ordering::Relaxed);
                context.last_activity_ms.store(now_ms(), Ordering::Relaxed);
                let outcome = run_cli_turn(&turn, rx);
                context.pending.store(false, Ordering::Relaxed);
                match outcome {
                    Ok(TurnOutcome::Completed { saw_result }) => {
                        if !saw_result {
                            record(
                                &turn,
                                TranscriptEntry::system("本轮结束（CLI 未输出 result 事件）"),
                            );
                        }
                        context.view.try_transition_if(
                            |status| !status.is_terminal() && *status != SessionStatus::Stopping,
                            SessionStatus::Ready,
                        );
                    }
                    Ok(TurnOutcome::Cancelled) => {
                        record(&turn, TranscriptEntry::system("本轮已取消"));
                        context.view.try_transition_if(
                            |status| !status.is_terminal() && *status != SessionStatus::Stopping,
                            SessionStatus::Ready,
                        );
                    }
                    Ok(TurnOutcome::Stopped) => break,
                    Err(error) => {
                        record(
                            &turn,
                            TranscriptEntry::error(format!(
                                "本轮失败（会话保持可用，可重试）: {error}"
                            )),
                        );
                        context.view.try_transition_if(
                            |status| !status.is_terminal() && *status != SessionStatus::Stopping,
                            SessionStatus::Ready,
                        );
                    }
                }
                context.dirty.store(true, Ordering::Relaxed);
            }
            Ok(WorkerCommand::Stop) => break,
            // Idle cancel has nothing to abort.
            Ok(WorkerCommand::Cancel) => {}
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Unlike the ACP path, an explicit stop keeps `resume.json`: resuming the
    // CLI conversation across a duthub restart is the profile's core contract.
    // Starting a fresh conversation is a separate product action (W4b).
    if let Some(listener) = context.listener_slot.lock().unwrap().take() {
        listener::revoke(listener, &context.auth);
    }
    context
        .view
        .try_transition_if(|status| !status.is_terminal(), SessionStatus::Stopping);
    context.view.stop_normally();
    if let Some(events) = &context.events {
        events.on_exited(&context.dut_id, None);
    }
    context.dirty.store(true, Ordering::Relaxed);
    transcript.finish();
    Ok(())
}

struct TurnContext<'a> {
    context: &'a CliWorkerContext,
    paths: &'a AgentPaths,
    sandbox: &'a sandbox::PreparedAgentSandbox,
    launch: &'a AgentLaunchSpec,
    mcp_url: &'a str,
    mcp_token: &'a str,
    prompt: &'a str,
    transcript: &'a TranscriptWriter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnOutcome {
    Completed { saw_result: bool },
    Cancelled,
    Stopped,
}

/// Spawn one CLI process, stream its NDJSON stdout, and wait for it to finish.
fn run_cli_turn(
    turn: &TurnContext<'_>,
    rx: &Receiver<WorkerCommand>,
) -> Result<TurnOutcome, String> {
    let resume = turn.context.view.session_id();
    let mut plan = claude_code_plan_with_resume(
        &turn.context.agent,
        &turn.paths.sandbox,
        turn.mcp_url,
        resume.as_deref(),
    )?;
    plan.stdin_prompt = turn.prompt.to_owned();
    // envKeys values are loaded by `resolve_cli_launch`; the MCP token is
    // appended here and never placed in argv or the MCP config file.
    let mut env: Vec<(String, String)> = turn
        .launch
        .env
        .iter()
        .filter(|(key, _)| !key.eq_ignore_ascii_case(MCP_TOKEN_ENV))
        .cloned()
        .collect();
    env.push((MCP_TOKEN_ENV.to_owned(), turn.mcp_token.to_owned()));
    let process = CliProcess::spawn(&plan, &turn.sandbox.cell, &turn.sandbox.policy, &env)
        .map_err(|error| format!("CLI 进程启动失败: {error}"))?;
    turn.context.view.set_stderr_tail(process.stderr_tail());

    let mut saw_result = false;
    let mut result_at: Option<Instant> = None;
    let mut last_stderr_poll = Instant::now();
    loop {
        if turn.context.stop_flag.load(Ordering::Relaxed) {
            process.kill();
            turn.context.view.set_stderr_tail(process.stderr_tail());
            return Ok(TurnOutcome::Stopped);
        }
        loop {
            match rx.try_recv() {
                Ok(WorkerCommand::Stop) => {
                    process.kill();
                    turn.context.view.set_stderr_tail(process.stderr_tail());
                    return Ok(TurnOutcome::Stopped);
                }
                Ok(WorkerCommand::Cancel) => {
                    process.kill();
                    turn.context.view.set_stderr_tail(process.stderr_tail());
                    return Ok(TurnOutcome::Cancelled);
                }
                // `HostManager::prompt` only enqueues while Ready, so a prompt
                // arriving mid-turn means a caller bypassed the status gate.
                Ok(WorkerCommand::Prompt(_)) => record(
                    turn,
                    TranscriptEntry::system("上一轮尚未结束时收到的 prompt 已忽略"),
                ),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    process.kill();
                    return Ok(TurnOutcome::Stopped);
                }
            }
        }

        match process.lines.recv_timeout(POLL_INTERVAL) {
            Ok(CliLine::Line(text)) => {
                turn.context
                    .last_activity_ms
                    .store(now_ms(), Ordering::Relaxed);
                if handle_cli_line(turn, &text) && !saw_result {
                    saw_result = true;
                    result_at = Some(Instant::now());
                }
                turn.context.dirty.store(true, Ordering::Relaxed);
            }
            Ok(CliLine::Overflow) => record(
                turn,
                TranscriptEntry::system(format!(
                    "CLI stdout 单行超过 {MAX_STDOUT_LINE} 字节，已丢弃"
                )),
            ),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {}
        }

        if last_stderr_poll.elapsed() >= STDERR_POLL_INTERVAL {
            turn.context.view.set_stderr_tail(process.stderr_tail());
            last_stderr_poll = Instant::now();
        }

        // After `result` the CLI is expected to exit immediately; a hung
        // process must not keep the session Busy forever.
        if let Some(result_at) = result_at
            && result_at.elapsed() >= STOP_TIMEOUT
        {
            process.kill();
            turn.context.view.set_stderr_tail(process.stderr_tail());
            return Ok(TurnOutcome::Completed { saw_result: true });
        }
        let idle = now_ms().saturating_sub(turn.context.last_activity_ms.load(Ordering::Relaxed));
        if !saw_result && idle >= PROMPT_HANG_TIMEOUT.as_millis() as u64 {
            process.kill();
            turn.context.view.set_stderr_tail(process.stderr_tail());
            return Err("CLI 一轮超过 10 分钟无任何输出，已终止本轮".to_owned());
        }

        if let Some(status) = process
            .try_wait()
            .map_err(|error| format!("等待 CLI 进程失败: {error}"))?
        {
            // The child may exit before the stdout reader has forwarded the
            // last pipe buffer; wait (bounded) for EOF instead of reading
            // `try_recv` once and losing the final `result` line. A lingering
            // grandchild can keep the pipe open, hence the deadline.
            let drain_deadline = Instant::now() + STDOUT_DRAIN_TIMEOUT;
            loop {
                match process.lines.recv_timeout(POLL_INTERVAL) {
                    Ok(CliLine::Line(text)) => {
                        saw_result |= handle_cli_line(turn, &text);
                    }
                    Ok(CliLine::Overflow) => record(
                        turn,
                        TranscriptEntry::system("CLI stdout 单行超过上限，已丢弃"),
                    ),
                    Err(RecvTimeoutError::Timeout) => {
                        if Instant::now() >= drain_deadline {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            turn.context.view.set_stderr_tail(process.stderr_tail());
            let code = Some(status);
            if code != Some(0) && !turn.context.stop_flag.load(Ordering::Relaxed) {
                record(
                    turn,
                    TranscriptEntry::error(format!(
                        "{} 进程退出（code {}）",
                        turn.launch.agent_name,
                        code_text(code)
                    )),
                );
            }
            return Ok(TurnOutcome::Completed { saw_result });
        }
    }
}

/// Apply one parsed stdout line; returns whether it was a `result` event.
fn handle_cli_line(turn: &TurnContext<'_>, line: &str) -> bool {
    let Some(event) = parse_claude_event(line) else {
        // Valid NDJSON with an event type this profile does not render is not
        // protocol noise (e.g. `rate_limit_event`); only non-JSON stdout is.
        let preview = truncate_chars(line, 200);
        let entry = match serde_json::from_str::<Value>(line) {
            Ok(value) => {
                let kind = value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("<untyped>");
                TranscriptEntry::system(format!("未处理的 CLI 事件: {kind}"))
            }
            Err(_) => TranscriptEntry::system(format!("忽略非 NDJSON stdout: {preview}")),
        };
        record(turn, entry);
        return false;
    };
    match event {
        CliEvent::System {
            subtype,
            session_id: _,
            text,
            mcp_errors,
        } => {
            let label = subtype.unwrap_or_else(|| "event".to_owned());
            record(
                turn,
                TranscriptEntry::system(format!("CLI system/{label}: {text}")),
            );
            for error in mcp_errors {
                record(
                    turn,
                    TranscriptEntry::error(format!("MCP 连接错误：{error}")),
                );
            }
            false
        }
        CliEvent::Assistant { text, .. } => {
            record(turn, TranscriptEntry::new(TranscriptKind::Agent, text));
            false
        }
        CliEvent::User {
            text,
            tool_result,
            is_error,
        } => {
            // A plain echoed user prompt is already in the transcript; only
            // tool results add new information here.
            if tool_result {
                let text = truncate_chars(text.trim(), TOOL_RESULT_PREVIEW);
                let entry = if is_error {
                    TranscriptEntry::error(format!("工具结果失败：{text}"))
                } else {
                    TranscriptEntry::new(TranscriptKind::Tool, format!("工具结果：{text}"))
                };
                record(turn, entry);
            }
            false
        }
        CliEvent::Result {
            subtype,
            session_id,
            is_error,
            text,
            permission_denials,
        } => {
            if let Some(session_id) = session_id {
                if is_valid_session_id(&session_id) {
                    turn.context.view.set_session_id(session_id.clone());
                    if let Err(error) =
                        host::persist_resume_state(turn.paths, turn.launch, &session_id)
                    {
                        record(
                            turn,
                            TranscriptEntry::system(format!(
                                "写入 resume.json 失败（下次启动将新建会话）: {error}"
                            )),
                        );
                    }
                } else {
                    record(
                        turn,
                        TranscriptEntry::system(format!(
                            "CLI 返回的 session id 不合法，已忽略: {session_id}"
                        )),
                    );
                }
            }
            for denial in permission_denials {
                record(
                    turn,
                    TranscriptEntry::system(format!("权限拒绝（plan 模式）：{denial}")),
                );
            }
            let label = subtype.unwrap_or_else(|| "result".to_owned());
            let entry = if is_error {
                TranscriptEntry::error(format!("本轮失败（{label}）: {}", text.trim()))
            } else {
                TranscriptEntry::system(format!("本轮结束（{label}）"))
            };
            record(turn, entry);
            true
        }
        CliEvent::Error { message } => {
            record(turn, TranscriptEntry::error(message));
            false
        }
    }
}

fn record(turn: &TurnContext<'_>, entry: TranscriptEntry) {
    host::record_entry(turn.context.view.as_ref(), turn.transcript, entry);
}

fn finalize_stopped(context: &CliWorkerContext) {
    // Resume markers survive an early stop for the same reason as a normal
    // stop: the next start can `--resume` this conversation.
    if let Some(listener) = context.listener_slot.lock().unwrap().take() {
        listener::revoke(listener, &context.auth);
    }
    context.pending.store(false, Ordering::Relaxed);
    if context.view.stop_normally()
        && let Some(events) = &context.events
    {
        events.on_exited(&context.dut_id, None);
    }
    context.dirty.store(true, Ordering::Relaxed);
}

/// Resolve the CLI program and record the launch spec for the UI.
fn resolve_cli_launch(agent: &AgentDefinition) -> Result<AgentLaunchSpec, String> {
    ensure_cli_launchable(agent)?;
    let raw = agent
        .program_path()
        .ok_or_else(|| format!("Agent {} 缺少 program", agent.id))?;
    let program = PathBuf::from(raw);
    verify_program_hash(&program, agent)?;
    let env =
        duthub_core::agent_config::load_agent_env(agent).map_err(|error| error.to_string())?;
    Ok(AgentLaunchSpec {
        agent_id: agent.id.clone(),
        agent_name: agent.name.clone(),
        program,
        args: agent.args.clone(),
        env,
        embedded: false,
        sha256: expected_sha256(agent)?.to_ascii_lowercase(),
        version: None,
        signature: "NotSigned".into(),
    })
}

/// Recompute the full SHA-256 on every start and every turn (fail closed).
fn verify_program_hash(path: &Path, agent: &AgentDefinition) -> Result<(), String> {
    let raw = path.to_string_lossy();
    if !registry::is_absolute_program(&raw) {
        return Err(format!("CLI 程序必须是绝对路径: {raw}"));
    }
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("CLI 程序不可用（{}）: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("CLI 程序不是文件: {}", path.display()));
    }
    let expected = expected_sha256(agent)?;
    let actual = duthub_core::store::sha256_file(path)
        .map_err(|error| format!("读取 CLI 程序哈希失败: {error}"))?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "CLI 程序哈希不匹配（{}）：期望 {expected}，实际 {actual}",
            path.display()
        ));
    }
    Ok(())
}

fn expected_sha256(agent: &AgentDefinition) -> Result<&str, String> {
    let expected = agent
        .expected_sha256
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .ok_or_else(|| {
            format!(
                "Agent {} 配置了 CLI 程序但缺少 expectedSha256；请选择程序并保存哈希",
                agent.id
            )
        })?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "Agent {} 的 expectedSha256 必须是 64 位十六进制",
            agent.id
        ));
    }
    Ok(expected)
}

/// Flags that would undo the profile's plan/streaming/MCP confinement, change
/// the session identity (`--resume` conflicts with `--session-id`), disable
/// resume persistence, or widen the sandbox/permission surface. Custom args
/// are otherwise restricted to the [`validate_custom_args`] allowlist.
fn is_reserved_cli_arg(arg: &str) -> bool {
    const EXACT: &[&str] = &[
        "-p",
        "--print",
        "-c",
        "--continue",
        "-r",
        "--resume",
        "--session-id",
        "--fork-session",
        "--from-pr",
        "--no-session-persistence",
        "--system-prompt",
        "--system-prompt-file",
        "--append-system-prompt",
        "--append-system-prompt-file",
        "--debug",
        "-d",
        "--debug-file",
        "--output-format",
        "--input-format",
        "--mcp-config",
        "--strict-mcp-config",
        "--permission-mode",
        "--permission-prompt-tool",
        "--dangerously-skip-permissions",
        "--allow-dangerously-skip-permissions",
        "--allowedTools",
        "--allowed-tools",
        "--disallowedTools",
        "--disallowed-tools",
        "--tools",
        "--add-dir",
        "--settings",
        "--setting-sources",
        "--agent",
        "--agents",
        "--plugin-dir",
        "--plugin-url",
        "--worktree",
        "-w",
        "--bg",
        "--background",
        "--remote-control",
        "--safe-mode",
        "--bare",
    ];
    let name = arg.split('=').next().unwrap_or(arg);
    EXACT.contains(&name)
}

fn validate_mcp_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("MCP URL 必须是 http(s) 绝对地址: {url}"));
    }
    if url.chars().any(char::is_whitespace) || url.chars().any(char::is_control) {
        return Err("MCP URL 不能包含空白或控制字符".into());
    }
    Ok(())
}

/// Write the per-session MCP config. The Bearer value is an env placeholder,
/// never the literal token (the token only travels in the child environment).
fn write_mcp_config(path: &Path, mcp_url: &str) -> Result<(), String> {
    let value = json!({
        "mcpServers": {
            "duthub": {
                "type": "http",
                "url": mcp_url,
                "headers": {
                    "Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}"),
                },
            }
        }
    });
    duthub_core::store::save_json_atomic(path, &value)
        .map_err(|error| format!("写入 MCP 配置失败（{}）: {error}", path.display()))
}

/// Read and validate the structured resume marker for this CLI Agent.
fn read_cli_resume(
    paths: &AgentPaths,
    launch: &AgentLaunchSpec,
    context: &CliWorkerContext,
) -> Option<String> {
    let session_id = host::read_resume_candidate(paths, launch, false, context.view.as_ref())?;
    if is_valid_session_id(&session_id) {
        return Some(session_id);
    }
    context.view.push_entry(TranscriptEntry::system(format!(
        "resume.json 中的 session id 不合法，已忽略并按新会话启动: {session_id}"
    )));
    host::remove_resume_markers(paths, context.view.as_ref());
    None
}

/// Session ids end up in argv (`.cmd` shims are parsed by cmd.exe), so only a
/// conservative alphabet is accepted.
fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn code_text(code: Option<i32>) -> String {
    code.map_or_else(|| "signal/unknown".into(), |code| code.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Per-turn process
// ---------------------------------------------------------------------------

enum CliLine {
    Line(String),
    Overflow,
}

struct CliProcess {
    process: SandboxedProcess,
    lines: Receiver<CliLine>,
    stderr: Arc<Mutex<StderrState>>,
    secrets: Vec<String>,
}

impl CliProcess {
    fn spawn(
        plan: &CliPlan,
        cell: &CellSpec,
        policy: &SandboxPolicy,
        env: &[(String, String)],
    ) -> Result<Self, String> {
        let cwd = cell.root();
        if !cwd.is_dir() {
            return Err(format!("工作目录不存在: {}", cwd.display()));
        }
        duthub_sandbox::prepare_cell(cell).map_err(|error| error.to_string())?;
        let local_app_data = cell.layout().home().join("AppData").join("Local");
        let temp = sandbox::temp_dir(cwd).map_err(|error| error.to_string())?;
        let (program, args) = resolve_sandboxed_argv(plan)?;
        let mut environment: Vec<(OsString, OsString)> = Vec::new();
        for key in INHERITED_ENV {
            if let Some(value) = std::env::var_os(key) {
                environment.push((OsString::from(key), value));
            }
        }
        for (key, value) in env {
            if key.contains('\0') || value.contains('\0') {
                return Err("环境变量包含 NUL 字符".into());
            }
            set_env_var(&mut environment, key, value.as_ref());
        }
        if !environment.iter().any(|(k, _)| {
            k.to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("LOCALAPPDATA"))
        }) {
            set_env_var(&mut environment, "LOCALAPPDATA", local_app_data.as_os_str());
        }
        // TEMP/TMP are written last so neither the inherited environment nor an
        // Agent envKeys entry can point them back at the user's temp directory.
        set_env_var(&mut environment, "TEMP", temp.as_os_str());
        set_env_var(&mut environment, "TMP", temp.as_os_str());

        let request = SpawnRequest {
            program,
            args,
            cwd: cwd.to_path_buf(),
            environment,
        };
        let mut process = spawn_streaming(cell, policy, &request)
            .map_err(|error| format!("沙箱启动失败: {error}"))?;
        let Some(stdin) = process.take_stdin() else {
            return Err("stdin 未创建".into());
        };
        let Some(stdout) = process.take_stdout() else {
            return Err("stdout 未创建".into());
        };
        let Some(stderr) = process.take_stderr() else {
            return Err("stderr 未创建".into());
        };

        let secrets = collect_secrets(env);
        let stderr_state = Arc::new(Mutex::new(StderrState::new(DEFAULT_STDERR_CAP)));
        let (tx, rx) = mpsc::sync_channel(STDOUT_QUEUE_CAPACITY);
        let stdout_thread = {
            let secrets = secrets.clone();
            std::thread::Builder::new()
                .name("duthub-cli-stdout".into())
                .spawn(move || read_stdout(stdout, tx, secrets))
        };
        if let Err(error) = stdout_thread {
            drop(process);
            return Err(format!("stdout 线程启动失败: {error}"));
        }
        let stderr_thread = {
            let stderr_state = stderr_state.clone();
            std::thread::Builder::new()
                .name("duthub-cli-stderr".into())
                .spawn(move || read_stderr(stderr, stderr_state))
        };
        if let Err(error) = stderr_thread {
            drop(process);
            return Err(format!("stderr 线程启动失败: {error}"));
        }
        // The prompt can be large; writing it from a dedicated thread keeps the
        // turn loop responsive and avoids a pipe deadlock if the child writes
        // output before consuming all of stdin.
        let prompt = plan.stdin_prompt.clone();
        let stdin_thread = std::thread::Builder::new()
            .name("duthub-cli-stdin".into())
            .spawn(move || write_prompt(stdin, &prompt));
        if let Err(error) = stdin_thread {
            drop(process);
            return Err(format!("stdin 线程启动失败: {error}"));
        }

        Ok(Self {
            process,
            lines: rx,
            stderr: stderr_state,
            secrets,
        })
    }

    fn stderr_tail(&self) -> String {
        let raw = self.stderr.lock().unwrap().text();
        redact_secrets(&raw, &self.secrets)
    }

    fn try_wait(&self) -> Result<Option<i32>, String> {
        self.process.try_wait().map_err(|error| error.to_string())
    }

    /// Terminate the whole sandbox Job tree deterministically.
    fn kill(&self) {
        let _ = self.process.terminate();
    }
}

impl Drop for CliProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

fn set_env_var(env: &mut Vec<(OsString, OsString)>, key: &str, value: &std::ffi::OsStr) {
    env.retain(|(existing, _)| {
        existing
            .to_str()
            .is_none_or(|name| !name.eq_ignore_ascii_case(key))
    });
    env.push((OsString::from(key), value.to_os_string()));
}

fn write_prompt(mut stdin: impl Write, prompt: &str) {
    let _ = stdin.write_all(prompt.as_bytes());
    let _ = stdin.write_all(b"\n");
    let _ = stdin.flush();
    // Dropping the handle closes stdin, which tells `claude -p` that the
    // prompt is complete.
}

fn collect_secrets(env: &[(String, String)]) -> Vec<String> {
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
    secrets
}

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

fn read_stdout(stdout: impl Read, tx: SyncSender<CliLine>, secrets: Vec<String>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_bounded_line(&mut reader, MAX_STDOUT_LINE) {
            Ok(LineChunk::Eof) => break,
            Ok(LineChunk::Overflow) => {
                if tx.send(CliLine::Overflow).is_err() {
                    break;
                }
            }
            Ok(LineChunk::Line(bytes)) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let text = redact_secrets(text.trim_end_matches(['\r', '\n']), &secrets);
                if !text.trim().is_empty() && tx.send(CliLine::Line(text)).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn read_stderr(stderr: impl Read, state: Arc<Mutex<StderrState>>) {
    let mut stderr = stderr;
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => state.lock().unwrap().push(&chunk[..read]),
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

/// Resolve `(program, args)` for `duthub-sandbox::spawn_streaming`.
///
/// On Windows, npm-style `.cmd`/`.bat` shims cannot be executed directly by
/// the Windows process launcher; they are launched through the fixed system
/// `%SystemRoot%\System32\cmd.exe` (never trusting `%COMSPEC%`) with `/D /C call`
/// so `cmd.exe` does not strip quotes around arguments and still runs inside
/// the sandbox's restricted token + Job Object.
fn resolve_sandboxed_argv(plan: &CliPlan) -> Result<(PathBuf, Vec<OsString>), String> {
    #[cfg(windows)]
    {
        let extension = plan
            .program
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
        if matches!(extension.as_deref(), Some("cmd" | "bat")) {
            for arg in &plan.args {
                if arg.contains('%') {
                    return Err(format!(
                        "CLI .cmd 启动参数不能包含 %（cmd.exe 会展开环境变量）: {arg}"
                    ));
                }
            }
            let system_root = std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            let cmd_exe = system_root.join("System32").join("cmd.exe");
            let mut args = Vec::with_capacity(plan.args.len() + 4);
            args.push(OsString::from("/D"));
            args.push(OsString::from("/C"));
            args.push(OsString::from("call"));
            args.push(plan.program.as_os_str().to_os_string());
            args.extend(plan.args.iter().map(OsString::from));
            return Ok((cmd_exe, args));
        }
    }
    Ok((
        plan.program.clone(),
        plan.args.iter().map(OsString::from).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::agent_config::AgentKind;

    fn cli_definition() -> AgentDefinition {
        let mut agent = AgentDefinition::goose();
        agent.id = "claude".into();
        agent.name = "Claude Code".into();
        agent.kind = AgentKind::Cli;
        agent.program = Some("C:/tools/claude.cmd".into());
        agent.cli_profile = Some(CLAUDE_CODE_PROFILE.into());
        agent.expected_sha256 = Some("a".repeat(64));
        agent
    }

    #[test]
    fn parses_system_assistant_result_and_error_events() {
        let system = parse_claude_event(
            r#"{"type":"system","subtype":"init","session_id":"s-1","model":"m","mcp_servers":[{"name":"duthub","status":"connected"}]}"#,
        )
        .unwrap();
        match system {
            CliEvent::System {
                subtype,
                session_id,
                text,
                mcp_errors,
            } => {
                assert_eq!(subtype.as_deref(), Some("init"));
                assert_eq!(session_id.as_deref(), Some("s-1"));
                assert!(text.contains("model=m"), "{text}");
                assert!(text.contains("duthub=connected"), "{text}");
                assert!(mcp_errors.is_empty(), "{mcp_errors:?}");
            }
            other => panic!("expected System, got {other:?}"),
        }

        // Real 2.1.204 init: `tools[]` reports the connected MCP surface and
        // `mcp_server_errors`/failed statuses must surface as errors.
        let rich = parse_claude_event(
            r#"{"type":"system","subtype":"init","session_id":"s-1","tools":["Bash","mcp__duthub__dut_list",{"name":"mcp__duthub__serial_read"}],"mcp_servers":[{"name":"duthub","status":"connected"},{"name":"other","status":"failed"}],"mcp_server_errors":[{"name":"other","error":"connection refused"}]}"#,
        )
        .unwrap();
        match rich {
            CliEvent::System {
                text, mcp_errors, ..
            } => {
                assert!(text.contains("tools=3"), "{text}");
                assert!(text.contains("duthub 工具 2 个"), "{text}");
                assert!(text.contains("connection refused"), "{text}");
                assert_eq!(mcp_errors.len(), 2, "{mcp_errors:?}");
                assert!(
                    mcp_errors
                        .iter()
                        .any(|error| error.contains("status=failed"))
                );
            }
            other => panic!("expected System, got {other:?}"),
        }

        let assistant = parse_claude_event(
            r#"{"type":"assistant","session_id":"s-1","message":{"content":[{"type":"text","text":"hello"},{"type":"tool_use","name":"serial_read"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            assistant,
            CliEvent::Assistant {
                text: "hello\n[tool] serial_read".into(),
                session_id: Some("s-1".into()),
            }
        );

        let result = parse_claude_event(
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s-1","result":"done"}"#,
        )
        .unwrap();
        assert!(matches!(
            result,
            CliEvent::Result {
                is_error: false,
                session_id: Some(ref id),
                ref subtype,
                ..
            } if id == "s-1" && subtype.as_deref() == Some("success")
        ));

        let error =
            parse_claude_event(r#"{"type":"error","error":{"message":"bad key"}}"#).unwrap();
        assert_eq!(
            error,
            CliEvent::Error {
                message: "bad key".into()
            }
        );

        // Headless `user` events carry tool results; they must parse instead of
        // falling into the "not NDJSON" noise bucket.
        let tool_result = parse_claude_event(
            r#"{"type":"user","session_id":"s-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"boot log line"}],"is_error":false}]}}"#,
        )
        .unwrap();
        assert_eq!(
            tool_result,
            CliEvent::User {
                text: "boot log line".into(),
                tool_result: true,
                is_error: false,
            }
        );
        let plain_user = parse_claude_event(r#"{"type":"user","message":"ignored"}"#).unwrap();
        assert_eq!(
            plain_user,
            CliEvent::User {
                text: "ignored".into(),
                tool_result: false,
                is_error: false,
            }
        );

        let denied = parse_claude_event(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s-1","errors":["boom"],"permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_2","tool_input":{"command":"rm -rf /"}}]}"#,
        )
        .unwrap();
        match denied {
            CliEvent::Result {
                text,
                permission_denials,
                is_error,
                ..
            } => {
                assert!(is_error);
                assert!(text.contains("boom"), "{text}");
                assert_eq!(permission_denials.len(), 1);
                assert!(permission_denials[0].contains("Bash"));
                assert!(
                    permission_denials[0].contains("rm -rf"),
                    "{permission_denials:?}"
                );
            }
            other => panic!("expected Result, got {other:?}"),
        }

        assert!(parse_claude_event("not json").is_none());
        assert!(parse_claude_event("  ").is_none());
    }

    #[test]
    fn cli_preflight_requires_profile_program_hash_and_no_reserved_args() {
        let agent = cli_definition();
        assert!(ensure_cli_launchable(&agent).is_ok());

        let mut acp = agent.clone();
        acp.kind = AgentKind::Acp;
        assert!(ensure_cli_launchable(&acp).is_err());

        let mut unknown = agent.clone();
        unknown.cli_profile = Some("codex".into());
        assert!(ensure_cli_launchable(&unknown).is_err());

        let mut relative = agent.clone();
        relative.program = Some("claude.cmd".into());
        assert!(ensure_cli_launchable(&relative).is_err());

        let mut unpinned = agent.clone();
        unpinned.expected_sha256 = None;
        assert!(
            ensure_cli_launchable(&unpinned)
                .unwrap_err()
                .contains("expectedSha256")
        );

        for reserved in [
            "--permission-mode",
            "--permission-mode=bypassPermissions",
            "--mcp-config",
            "--resume",
            "--session-id",
            "--session-id=00000000-0000-0000-0000-000000000000",
            "--fork-session",
            "--no-session-persistence",
            "--add-dir",
            "--settings",
            "--setting-sources",
            "--allowedTools",
            "--tools",
            "--permission-prompt-tool",
            "--system-prompt",
            "--system-prompt-file",
            "--append-system-prompt",
            "--append-system-prompt-file",
            "--debug",
            "--debug-file",
            "--plugin-dir",
            "--safe-mode",
            "--bare",
            "-p",
            "--dangerously-skip-permissions",
        ] {
            let mut overridden = agent.clone();
            overridden.args = vec![reserved.into()];
            assert!(
                ensure_cli_launchable(&overridden).is_err(),
                "reserved arg must be rejected: {reserved}"
            );
        }

        // A bare positional would replace the stdin prompt; reject it.
        let mut positional = agent.clone();
        positional.args = vec!["summarize the boot log".into()];
        let error = ensure_cli_launchable(&positional).unwrap_err();
        assert!(error.contains("位置参数"), "{error}");

        // Value-taking flags need a value and cannot swallow another flag.
        for missing in [
            vec!["--model"],
            vec!["--model", "--effort"],
            vec!["--model="],
        ] {
            let mut bad = agent.clone();
            bad.args = missing.iter().map(|arg| (*arg).to_owned()).collect();
            assert!(ensure_cli_launchable(&bad).is_err(), "{missing:?}");
        }

        for allowed in [
            vec!["--model", "claude-sonnet"],
            vec!["--model=claude-sonnet"],
            vec!["--effort", "high"],
            vec!["--max-budget-usd", "0.5"],
            vec!["--fallback-model", "opus"],
            vec!["--verbose"],
        ] {
            let mut extended = agent.clone();
            extended.args = allowed.iter().map(|arg| (*arg).to_owned()).collect();
            assert!(
                ensure_cli_launchable(&extended).is_ok(),
                "allowed args rejected: {allowed:?}"
            );
        }
    }
}
