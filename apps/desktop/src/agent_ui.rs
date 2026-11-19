//! Agent-facing UI support logic (W4a). Pure functions, no GPUI — integration
//! tests (`apps/desktop/tests/agent_ui_logic.rs`) drive everything in here.
//!
//! Blocks:
//! - A2A endpoint view: derive the per-DUT card URL from the *runtime* status.
//!   Only a `Running` listener yields a URL, and it is always loopback (ADR-2).
//! - A2A export payload: serialize + fail closed when the secret scanner
//!   (`duthub_core::export_contains_secret`) flags the payload.
//! - Port validation: reject `0` and ports reserved by other subsystems.
//! - DUT removal: explicit order (strip MCP → persist → stop & join → delete
//!   A2A token → delete agent dir) with a pending-cleanup journal for steps
//!   that failed, so nothing silently claims a cleanup that never happened.
//! - OS isolation badge: probe facts only. The `os_sandboxed` trust label is
//!   claimed only when a sandbox *backend* verified the boundary primitives
//!   (ADR-12); capability probing never upgrades the label by itself.

use std::path::{Path, PathBuf};

use crate::agent_host::{AgentLaunchSpec, SessionStatus, TranscriptEntry, TranscriptKind};
use duthub_a2a::listener::A2aStatus;
use duthub_core::agent_config::{AgentDefinition, AgentsFile};
use duthub_core::audit::AuditEvent;
use duthub_core::model::DutA2aConfig;

// ---------------------------------------------------------------------------
// A2A endpoint view
// ---------------------------------------------------------------------------

/// Display projection of one DUT's A2A runtime status.
///
/// `card_url` is `Some` only while the listener is actually bound, and by
/// construction (ADR-2, `resolve_loopback`) always points at `127.0.0.1`.
/// There is deliberately no LAN URL field: W3 refuses non-loopback binds, so
/// advertising a LAN card URL would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aEndpointView {
    /// Agent Card URL of the running listener (loopback only).
    pub card_url: Option<String>,
    /// One-line status for the panel; failures carry the reason.
    pub status_text: String,
    /// True when the runtime reports a failure (row is tinted red/warn).
    pub failed: bool,
}

/// Project the per-DUT config + live runtime status onto display fields.
pub fn a2a_endpoint_view(config: &DutA2aConfig, status: &A2aStatus) -> A2aEndpointView {
    match status {
        A2aStatus::Running { endpoint } => {
            let base = endpoint.trim_end_matches('/');
            A2aEndpointView {
                card_url: Some(format!("{base}/.well-known/agent-card.json")),
                status_text: format!("运行中 · {endpoint}"),
                failed: false,
            }
        }
        A2aStatus::Starting => A2aEndpointView {
            card_url: None,
            status_text: "启动中…".into(),
            failed: false,
        },
        A2aStatus::Failed { reason } => A2aEndpointView {
            card_url: None,
            status_text: format!("启动失败：{reason}"),
            failed: true,
        },
        A2aStatus::NeedsProductSignoff { reason } => A2aEndpointView {
            card_url: None,
            status_text: format!("绑定被拒绝（需产品签字）：{reason}"),
            failed: true,
        },
        A2aStatus::PortConflict => A2aEndpointView {
            card_url: None,
            status_text: format!("端口 {} 冲突或被其他子系统占用，未绑定", config.port),
            failed: true,
        },
        A2aStatus::Disabled => A2aEndpointView {
            card_url: None,
            status_text: "未运行".into(),
            failed: false,
        },
    }
}

// ---------------------------------------------------------------------------
// A2A port validation
// ---------------------------------------------------------------------------

/// Validate an A2A port for save/enable (W4a): port 0 is never bindable and
/// ports reserved by other subsystems (MCP http port, MCP listeners, other
/// DUT A2A ports) must be refused before they turn into runtime `PortConflict`.
pub fn validate_a2a_port(port: u16, reserved: &[u16]) -> Result<(), String> {
    if port == 0 {
        return Err("A2A 端口不能为 0（0 不会绑定任何监听）".into());
    }
    if reserved.contains(&port) {
        return Err(format!(
            "端口 {port} 已被 MCP/A2A 其他端点占用，请换一个端口"
        ));
    }
    Ok(())
}

/// Ports reserved by other subsystems, from settings + the A2A ports already
/// claimed by DUTs (pure; the bin layer feeds `duts`-derived ports in).
///
/// Order-stable, deduplicated: MCP HTTP port first, then listener ports, then
/// the extra A2A ports.
pub fn reserved_ports_from(
    settings: &duthub_core::SettingsFile,
    extra_a2a_ports: impl IntoIterator<Item = u16>,
) -> Vec<u16> {
    let mut reserved = vec![settings.agent_access.http_port];
    reserved.extend(settings.agent_access.listeners.iter().map(|l| l.port));
    reserved.extend(extra_a2a_ports);
    let mut seen = std::collections::HashSet::new();
    reserved.retain(|port| seen.insert(*port));
    reserved
}

// ---------------------------------------------------------------------------
// A2A export payload (secret scan, fail closed)
// ---------------------------------------------------------------------------

/// Build the peers section of `exports/a2a_server.json` and refuse (fail
/// closed) to return anything the secret scanner flags — e.g. a card URL that
/// smuggles `?token=…` must never reach an export file.
pub fn a2a_export_payload(peers: &[duthub_core::A2aPeer]) -> Result<serde_json::Value, String> {
    let peers_json: Vec<serde_json::Value> = peers
        .iter()
        .map(|peer| {
            serde_json::json!({
                "id": peer.id,
                "name": peer.name,
                "agentCardUrl": peer.card_url,
            })
        })
        .collect();
    let payload = serde_json::Value::Array(peers_json);
    if duthub_core::keys::export_contains_secret(&payload) {
        return Err(
            "导出已拒绝（fail closed）：peers 中疑似包含密钥字段（如 token），请清理远端 Agent URL 后重试"
                .into(),
        );
    }
    Ok(payload)
}

// ---------------------------------------------------------------------------
// DUT removal: explicit cleanup order + pending-cleanup journal
// ---------------------------------------------------------------------------

/// One executed cleanup step, in execution order inside [`RemoveOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupStep {
    StripMcp,
    PersistRemoval,
    StopAndJoin,
    DeleteA2aToken,
    DeleteAgentDir,
    /// Agent registry deletion: keyring env values under `acp/<id>/env/<KEY>`
    /// that failed to delete. `PendingCleanupEntry::env_keys` carries the keys;
    /// the entry's `dut_id` field names the *Agent* id for this step.
    DeleteAgentEnvKeys,
}

impl CleanupStep {
    /// Stable key persisted inside `pending-cleanup.json` (never renumber).
    pub fn key(self) -> &'static str {
        match self {
            CleanupStep::StripMcp => "strip_mcp",
            CleanupStep::PersistRemoval => "persist_removal",
            CleanupStep::StopAndJoin => "stop_and_join",
            CleanupStep::DeleteA2aToken => "delete_a2a_token",
            CleanupStep::DeleteAgentDir => "delete_agent_dir",
            CleanupStep::DeleteAgentEnvKeys => "delete_agent_env_keys",
        }
    }
}

/// Result of [`run_remove_dut`]: which steps ran, in order, and whether the
/// cleanup can honestly claim to be complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub dut_id: String,
    /// Executed steps in order (a step appears only after it succeeded or was
    /// recorded as best-effort).
    pub steps: Vec<CleanupStep>,
    /// Best-effort step failures; non-empty ⇒ `complete == false`.
    pub warnings: Vec<String>,
    /// True only when every step finished without a recorded failure.
    pub complete: bool,
}

/// The removal executor's seams. The bin implements this for `Workbench`;
/// tests drive it with an in-memory mock to pin the ordering contract.
pub trait DutCleanupOps {
    /// Remove the DUT-scoped MCP listener + credential from persisted
    /// settings. `false` = strip failed (orphan endpoint risk) and the whole
    /// removal must abort. No listener configured counts as success.
    fn strip_mcp(&mut self, dut_id: &str) -> bool;

    /// Persist the DUT removal to durable storage. Must run before any
    /// runtime teardown, so a failed stop can never resurrect the DUT on
    /// restart (the record is already gone).
    fn persist_removal(&mut self, dut_id: &str) -> Result<(), String>;

    /// Stop per-DUT actors (agent session, serial session, A2A listener) and
    /// wait for them to wind down before the on-disk identity is deleted.
    fn stop_and_join(&mut self, dut_id: &str);

    /// Delete the A2A keyring token (`duthub/a2a/<dut_id>`); idempotent.
    fn delete_a2a_token(&mut self, dut_id: &str) -> Result<(), String>;

    /// Delete the per-DUT agent directory; idempotent — a missing directory
    /// is `Ok`, never an error.
    fn delete_agent_dir(&mut self, dut_id: &str) -> Result<(), String>;

    /// Record a failed best-effort step for retry at next startup and raise
    /// an operator-visible warning. Never pretend the step succeeded.
    fn record_pending_cleanup(&mut self, dut_id: &str, step: CleanupStep, reason: &str);
}

/// Run the DUT removal in the fixed order:
/// strip MCP → persist removal → stop & join → delete A2A token → delete dir.
///
/// - `strip_mcp == false` ⇒ `Err` and nothing else ran.
/// - `persist_removal` error ⇒ `Err` **before** any actor is stopped (the DUT
///   keeps running exactly as it was).
/// - Token/dir deletion failures are best-effort: they write a
///   `pending-cleanup.json` entry via [`DutCleanupOps::record_pending_cleanup`],
///   append a warning and leave `complete == false` — the caller must not
///   claim a full cleanup.
pub fn run_remove_dut(ops: &mut dyn DutCleanupOps, dut_id: &str) -> Result<RemoveOutcome, String> {
    let mut outcome = RemoveOutcome {
        dut_id: dut_id.to_owned(),
        steps: Vec::new(),
        warnings: Vec::new(),
        complete: true,
    };
    if !ops.strip_mcp(dut_id) {
        return Err("清理该 DUT 的 MCP 端点失败，已取消删除".into());
    }
    outcome.steps.push(CleanupStep::StripMcp);
    ops.persist_removal(dut_id)
        .map_err(|error| format!("保存删除结果失败，已取消剩余清理（DUT 未停止）: {error}"))?;
    outcome.steps.push(CleanupStep::PersistRemoval);
    ops.stop_and_join(dut_id);
    outcome.steps.push(CleanupStep::StopAndJoin);
    if let Err(error) = ops.delete_a2a_token(dut_id) {
        outcome.complete = false;
        outcome
            .warnings
            .push(format!("A2A Token 删除失败: {error}"));
        ops.record_pending_cleanup(dut_id, CleanupStep::DeleteA2aToken, &error);
    }
    outcome.steps.push(CleanupStep::DeleteA2aToken);
    if let Err(error) = ops.delete_agent_dir(dut_id) {
        outcome.complete = false;
        outcome
            .warnings
            .push(format!("Agent 目录删除失败: {error}"));
        ops.record_pending_cleanup(dut_id, CleanupStep::DeleteAgentDir, &error);
    }
    outcome.steps.push(CleanupStep::DeleteAgentDir);
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// pending-cleanup journal
// ---------------------------------------------------------------------------

/// One recorded cleanup failure, retried at next startup.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingCleanupEntry {
    pub dut_id: String,
    pub step: String,
    pub reason: String,
    pub recorded_at_unix_ms: u64,
    /// Only for `delete_agent_env_keys` entries: the env keys whose keyring
    /// deletion failed. Empty (and omitted on disk) for DUT-scoped steps, so
    /// journals written by older builds parse unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_keys: Vec<String>,
}

/// `<work_dir>/agent/pending-cleanup.json`.
pub fn pending_cleanup_path(work_dir: &Path) -> PathBuf {
    work_dir.join("agent").join("pending-cleanup.json")
}

/// Load the journal; a missing file is an empty list (first-run semantics).
pub fn load_pending_cleanup(work_dir: &Path) -> Vec<PendingCleanupEntry> {
    let Ok(raw) = std::fs::read_to_string(pending_cleanup_path(work_dir)) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Replace the journal contents atomically (used after startup retries).
pub fn save_pending_cleanup(
    work_dir: &Path,
    entries: &[PendingCleanupEntry],
) -> Result<(), String> {
    let path = pending_cleanup_path(work_dir);
    duthub_core::store::save_json_atomic(&path, &entries)
        .map_err(|error| format!("写 {} 失败: {error}", path.display()))
}

/// Append one entry (deduplicated on `dut_id + step`) to the journal.
pub fn append_pending_cleanup(
    work_dir: &Path,
    dut_id: &str,
    step: CleanupStep,
    reason: &str,
) -> Result<(), String> {
    let mut entries = load_pending_cleanup(work_dir);
    if entries
        .iter()
        .any(|entry| entry.dut_id == dut_id && entry.step == step.key())
    {
        return Ok(());
    }
    entries.push(PendingCleanupEntry {
        dut_id: dut_id.to_owned(),
        step: step.key().to_owned(),
        reason: reason.to_owned(),
        recorded_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        env_keys: Vec::new(),
    });
    save_pending_cleanup(work_dir, &entries)
}

/// Append the env keys whose keyring deletion failed after an Agent registry
/// deletion (W4b). Entries are merged per agent: an existing
/// `delete_agent_env_keys` record for the same agent gains the missing keys,
/// never duplicated rows.
pub fn append_agent_env_pending_cleanup(
    work_dir: &Path,
    agent_id: &str,
    keys: &[String],
    reason: &str,
) -> Result<(), String> {
    let mut entries = load_pending_cleanup(work_dir);
    let recorded_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if let Some(entry) = entries.iter_mut().find(|entry| {
        entry.dut_id == agent_id && entry.step == CleanupStep::DeleteAgentEnvKeys.key()
    }) {
        for key in keys {
            if !entry.env_keys.contains(key) {
                entry.env_keys.push(key.clone());
            }
        }
        return save_pending_cleanup(work_dir, &entries);
    }
    entries.push(PendingCleanupEntry {
        dut_id: agent_id.to_owned(),
        step: CleanupStep::DeleteAgentEnvKeys.key().to_owned(),
        reason: reason.to_owned(),
        recorded_at_unix_ms: recorded_at,
        env_keys: keys.to_vec(),
    });
    save_pending_cleanup(work_dir, &entries)
}

/// Per-DUT agent directory for probing/deletion, **without creating it**
/// (`store::agent_dut_dir` would mkdir as a side effect; rendering a badge or
/// removing a DUT must not invent directories).
pub fn agent_dut_probe_path(work_dir: &Path, dut_id: &str) -> Option<PathBuf> {
    let key = duthub_core::store::agent_dut_key(dut_id)?;
    Some(work_dir.join("agent").join("dut").join(key))
}

/// Delete the per-DUT agent directory, idempotently: a missing directory is
/// success (`NotFound` ⇒ `Ok`); any other I/O failure is reported verbatim.
pub fn delete_agent_dir_idempotent(work_dir: &Path, dut_id: &str) -> Result<(), String> {
    let Some(path) = agent_dut_probe_path(work_dir, dut_id) else {
        return Err(format!("DUT ID {dut_id:?} 无法映射到 Agent 目录"));
    };
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("删除 {} 失败: {error}", path.display())),
    }
}

/// Retry every journal entry at startup. DUT-scoped steps retry both
/// idempotent deletions (a2a token + agent dir); `delete_agent_env_keys`
/// entries retry the per-key keyring deletions instead — their `dut_id`
/// field names an Agent, never a DUT, so the DUT steps must not run.
/// Fully-successful entries are removed from the journal; entries that
/// failed again are kept and returned as human-readable failure strings.
pub fn retry_pending_cleanup(work_dir: &Path) -> (usize, Vec<String>) {
    let entries = load_pending_cleanup(work_dir);
    if entries.is_empty() {
        return (0, Vec::new());
    }
    let mut completed = 0;
    let mut failures = Vec::new();
    let mut remaining = Vec::new();
    for entry in entries {
        if entry.step == CleanupStep::DeleteAgentEnvKeys.key() {
            if entry.env_keys.is_empty() {
                // Obsolete record with nothing left to delete: drop it.
                completed += 1;
                continue;
            }
            let agent_id = entry.dut_id.clone();
            let failed = delete_agent_env_entries_with(&agent_id, &entry.env_keys, |key| {
                duthub_core::agent_config::delete_agent_env(&agent_id, key)
                    .map_err(|error| error.to_string())
            });
            match failed {
                Ok(pending) if pending.is_empty() => completed += 1,
                Ok(pending) => {
                    failures.push(format!(
                        "Agent {} 的环境变量删除仍失败（下次启动继续重试）: {}",
                        entry.dut_id,
                        pending.join(", ")
                    ));
                    remaining.push(PendingCleanupEntry {
                        env_keys: pending,
                        ..entry
                    });
                }
                Err(error) => {
                    failures.push(format!(
                        "Agent {} 的环境变量清理失败（下次启动继续重试）: {error}",
                        entry.dut_id
                    ));
                    remaining.push(entry);
                }
            }
            continue;
        }
        let token_result = duthub_core::delete_a2a_token(&entry.dut_id);
        let dir_result = delete_agent_dir_idempotent(work_dir, &entry.dut_id);
        match (token_result, dir_result) {
            (Ok(()), Ok(())) => completed += 1,
            (token_result, dir_result) => {
                let reason = match (token_result, dir_result) {
                    (Err(token), Ok(())) => format!("A2A Token 删除失败: {token}"),
                    (Ok(()), Err(dir)) => dir,
                    (Err(token), Err(dir)) => format!("{dir}；A2A Token 删除失败: {token}"),
                    (Ok(()), Ok(())) => unreachable!("matched above"),
                };
                failures.push(format!("DUT {}（{}）: {reason}", entry.dut_id, entry.step));
                remaining.push(entry);
            }
        }
    }
    if let Err(error) = save_pending_cleanup(work_dir, &remaining) {
        failures.push(format!("pending-cleanup 回写失败: {error}"));
    }
    (completed, failures)
}

// ---------------------------------------------------------------------------
// W4b: Agent registry editor — args JSON / env keys / program path gate
// ---------------------------------------------------------------------------

/// Validated editor draft: everything `save_agent_registry` writes into
/// `agents.json` (values never — env values live in the OS keystore).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentEditorDraft {
    /// `None` = use duthub's built-in goose resolution (downloaded install >
    /// PATH, goose only); `Some` = absolute path to an external program.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
}

/// Render args as a JSON array for the editor input. Round-trips through
/// [`parse_args_json`], so arguments containing spaces or quotes survive
/// edit → save without whitespace splitting ever seeing them.
pub fn args_to_json(args: &[String]) -> String {
    serde_json::to_string(args).unwrap_or_else(|_| "[]".to_owned())
}

/// Parse the registry editor's args field: a JSON array of strings.
///
/// An empty (or whitespace-only) draft means "no arguments". Anything else
/// must be a JSON array whose elements are all strings — `Err` keeps the
/// editor open and the registry file untouched.
pub fn parse_args_json(raw: &str) -> Result<Vec<String>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| format!("参数必须是 JSON 数组，解析失败: {error}"))?;
    let Some(items) = value.as_array() else {
        return Err("参数必须是 JSON 字符串数组，如 [\"--acp\", \"--stdio\"]".into());
    };
    let mut args = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(format!("参数数组只能包含字符串: {item}"));
        };
        args.push(text.to_owned());
    }
    Ok(args)
}

/// Validate the env key list before it reaches `agents.json`: each key must
/// pass the core rules (naming + case-insensitive reserved-key rejection —
/// sandbox variables like `PATH`/`TEMP`/`GOOSE_MODE` are owned by duthub) and
/// duplicates are refused.
pub fn validate_env_keys(keys: &[String]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for key in keys {
        duthub_core::agent_config::validate_env_key(key).map_err(|error| error.to_string())?;
        let key = key.trim();
        if !seen.insert(key) {
            return Err(format!("环境变量键名重复: {key}"));
        }
    }
    Ok(())
}

/// Parse the editor's env keys draft (comma/space separated) through
/// [`validate_env_keys`].
pub fn parse_env_keys_draft(raw: &str) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    for token in raw.split(|character: char| character == ',' || character.is_whitespace()) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        keys.push(token.to_owned());
    }
    validate_env_keys(&keys)?;
    Ok(keys)
}

/// An external program must be an absolute path (Windows semantics: drive or
/// UNC form). A relative path would resolve against the per-session sandbox
/// cwd, so it is rejected before any hash probing.
pub fn validate_external_program_path(path: &str) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("外部程序路径不能为空".into());
    }
    if crate::agent_host::is_absolute_program(path) {
        return Ok(());
    }
    Err(format!("外部程序必须是绝对路径（盘符或 UNC）: {path}"))
}

/// The single save gate for the registry editor (W4b): program path, args
/// JSON and env keys all fail closed before `agents.json` is touched. An
/// empty program means "use duthub's built-in resolution" (downloaded install
/// shadows PATH) and is only valid for goose — the caller enforces that with
/// its own i18n message before calling here.
pub fn validate_agent_editor_draft(
    program: &str,
    args_raw: &str,
    env_keys_raw: &str,
) -> Result<AgentEditorDraft, String> {
    let program = program.trim();
    let program = if program.is_empty() {
        None
    } else {
        validate_external_program_path(program)?;
        Some(program.to_owned())
    };
    let args = parse_args_json(args_raw)?;
    let env_keys = parse_env_keys_draft(env_keys_raw)?;
    Ok(AgentEditorDraft {
        program,
        args,
        env_keys,
    })
}

// ---------------------------------------------------------------------------
// W4b: env value editor — keyring read state machine
// ---------------------------------------------------------------------------

/// Read state of one env key in the OS keystore, derived from a keyring
/// lookup. The clear value never reaches the UI: only this state summary
/// does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValueState {
    /// The keystore holds a non-empty value (masked; typing overwrites).
    Stored,
    /// No entry yet: the registry references the key but no value is saved.
    NotSet,
    /// The keystore lookup itself failed (platform error).
    LookupFailed(String),
    /// The keystore read is still running on the background executor (W4c);
    /// the UI shows "reading…" until the generation-checked result lands.
    Pending,
}

/// Map a keyring read result onto the editor state machine.
pub fn env_value_source(read: &Result<Option<String>, String>) -> EnvValueState {
    match read {
        // An empty value counts as "not set": launch treats it the same way.
        Ok(Some(value)) if !value.trim().is_empty() => EnvValueState::Stored,
        Ok(_) => EnvValueState::NotSet,
        Err(error) => EnvValueState::LookupFailed(error.clone()),
    }
}

// ---------------------------------------------------------------------------
// W4b: agent deletion — keyring cleanup with a pending-cleanup journal
// ---------------------------------------------------------------------------

/// Best-effort deletion of every `acp/<agent id>/env/<KEY>` keyring entry for
/// one agent (W4b registry deletion). Every key is attempted even if an
/// earlier one fails; the returned list holds the keys that still need
/// cleanup (empty = fully cleaned) so the caller can journal them. `Err`
/// means nothing was attempted at all (invalid agent id).
pub fn delete_agent_env_entries(agent: &AgentDefinition) -> Result<Vec<String>, String> {
    let agent_id = agent.id.clone();
    delete_agent_env_entries_with(&agent_id, &agent.env_keys, |key| {
        duthub_core::agent_config::delete_agent_env(&agent_id, key)
            .map_err(|error| error.to_string())
    })
}

/// Injectable [`delete_agent_env_entries`]: `delete` receives the env key and
/// reports one deletion outcome. Tests pin the ordering/failure contract with
/// an in-memory store.
pub fn delete_agent_env_entries_with(
    agent_id: &str,
    keys: &[String],
    mut delete: impl FnMut(&str) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    if agent_id.trim().is_empty() {
        return Err("Agent id 为空，无法定位 keyring 条目".into());
    }
    let mut pending = Vec::new();
    for key in keys {
        // Invalid keys (including empty ones) cannot be addressed and stay
        // pending instead of aborting the remaining deletions.
        if delete(key.trim()).is_err() {
            pending.push(key.trim().to_owned());
        }
    }
    Ok(pending)
}

// ---------------------------------------------------------------------------
// W4b: registry toggle guard + running-session selector lock
// ---------------------------------------------------------------------------

/// Toggle guard (W4b): enabling is always allowed; disabling must not leave
/// the registry without a startable Agent and must never disable the default
/// goose entry (per-DUT selections fall back to it).
pub fn validate_agent_toggle(file: &AgentsFile, agent_id: &str) -> Result<(), String> {
    let Some(agent) = file.agent(agent_id) else {
        return Err("Agent 不在注册表中（可能已删除）".into());
    };
    if !agent.enabled {
        return Ok(());
    }
    if agent.is_goose() {
        return Err(
            "默认 goose 不能停用（未选择的 DUT 会回退 goose）；可编辑程序路径或删除其他条目".into(),
        );
    }
    let enabled = file.agents.iter().filter(|agent| agent.enabled).count();
    if enabled <= 1 {
        return Err("不能停用最后一个启用的 Agent；请先启用另一个 Agent".into());
    }
    Ok(())
}

/// Selector lock rule: any live (non-terminal) session state disables the
/// per-DUT Agent dropdown — a change would not affect the running process.
/// A missing session renders as `Stopped` (terminal), so the selector stays
/// editable.
pub fn running_agent_selector_disabled(status: &SessionStatus) -> bool {
    !status.is_terminal()
}

/// One-line display of the *actually launched* agent for the running panel.
/// Shows the launch spec's agent id (never the pending dropdown selection);
/// `None` means the worker has not recorded a spec yet (still starting).
pub fn running_launch_line(launch: Option<&AgentLaunchSpec>) -> String {
    match launch {
        Some(spec) => {
            let source = if spec.embedded {
                "内嵌".to_owned()
            } else {
                spec.program.display().to_string()
            };
            format!("实际启动 agent：{} · {}", spec.agent_id, source)
        }
        None => "实际启动 agent：尚未记录（启动中…）".into(),
    }
}

// ---------------------------------------------------------------------------
// OS isolation badge
// ---------------------------------------------------------------------------

/// Which fact source backs the `os_sandboxed` trust claim.
///
/// ADR-12: [`duthub_core::probe_os_isolation`] only *detects capability* — it
/// reads back the directory DACL and creates a probe restricted token, which
/// says nothing about the token a launched Agent actually runs under. Treating
/// that probe as a sandbox proof is exactly the overclaim ADR-12 rejected.
///
/// Only a `duthub_sandbox::SandboxStatus` whose enforcement primitives were
/// established by real calls (`SandboxStatus::agent_isolation()`: restricted
/// token + Job limits + read denial) may pass
/// [`SandboxBoundaryProof::VerifiedByBackend`]. Until the desktop wires that
/// probe, every call site passes [`SandboxBoundaryProof::Unverified`], so the
/// `os_sandboxed` level always renders as 「未验证/未启用」.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBoundaryProof {
    /// A sandbox backend verified the primitives that form the boundary.
    VerifiedByBackend,
    /// No backend proof was consulted (or it reported the primitives
    /// unverified/unavailable), so the label must not claim isolation.
    Unverified,
}

/// Effective trust label for a configured registry trust level.
///
/// `os_sandboxed` is shown as 「OS 沙箱（已验证）」 only with a backend boundary
/// proof. Without one it downgrades to 「未验证/未启用」 — the same string never
/// mixes a "sandbox is on" claim with an "unverified" caveat, and it does not
/// echo the raw `os_sandboxed` token either. Every other level is unaffected.
pub fn effective_trust_label(trust_level: &str, proof: SandboxBoundaryProof) -> &'static str {
    match trust_level.trim() {
        "os_sandboxed" if proof == SandboxBoundaryProof::VerifiedByBackend => "OS 沙箱（已验证）",
        "os_sandboxed" => "未验证/未启用（沙箱边界未经后端证实）",
        "verified_readonly" => "已验证只读",
        _ => "MCP 接入",
    }
}

/// Badge text built strictly from probe facts (W2b `OsIsolationStatus`):
/// ACL hardened or not + reason, restricted-token availability, and the
/// explicit "AppContainer not attempted" note. Never mentions a sandbox that
/// the probe did not prove.
pub fn os_isolation_badge_text(status: &duthub_sandbox::OsIsolationStatus) -> String {
    if status.unsupported {
        return "OS 隔离未验证（当前平台无探测实现）".into();
    }
    let acl = if status.private_dir_acl {
        "目录 ACL 已加固".to_owned()
    } else {
        let reason = if status.detail.is_empty() {
            "原因未知".to_owned()
        } else {
            status.detail.clone()
        };
        format!("目录 ACL 未加固（{reason}）")
    };
    let token = if status.restricted_token {
        "受限令牌可用"
    } else {
        "受限令牌不可用"
    };
    format!("{acl} · {token} · AppContainer 未尝试")
}

// ---------------------------------------------------------------------------
// W4c: transcript follow / agent tag / prompt send-clear contract
// ---------------------------------------------------------------------------

/// Whether the transcript viewport should stick to the bottom this frame.
///
/// `total` is the session's monotonic `SessionView::total_entries()` counter
/// (it keeps growing past the 2000-entry memory cap, unlike
/// `transcript.len()`), and `seen` is the value the UI consumed on the
/// previous frame. Follow only when new entries arrived; when the user
/// scrolled up to read history the caller's near-bottom check keeps the
/// viewport where it is.
pub fn should_follow(seen: u64, total: u64) -> bool {
    total > seen
}

/// Decide whether the deferred prompt-clear may wipe the editor: only when
/// the editor still holds exactly the sent text. Anything typed (or pasted)
/// between enqueue and the deferred clear must survive.
pub fn should_clear_prompt(current_value: &str, sent: &str) -> bool {
    current_value == sent
}

/// Outcome of one prompt send attempt, decided purely so tests can pin the
/// draft-retention contract without a GPUI workbench. Display strings live in
/// the bin layer (this lib target has no locale system) — `reason` carries
/// the host error verbatim as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSendOutcome {
    /// The host accepted the prompt: the editor may be cleared (deferred,
    /// value-matched per [`should_clear_prompt`]).
    Enqueued,
    /// The host refused the prompt: keep the draft, and record the attempted
    /// text in the transcript so the failure is visible and retryable.
    Rejected {
        /// The host's refusal reason (Display of the error, as data).
        reason: String,
    },
}

/// Map an enqueue result onto the send outcome.
pub fn prompt_send_outcome(enqueue: Result<(), impl std::fmt::Display>) -> PromptSendOutcome {
    match enqueue {
        Ok(()) => PromptSendOutcome::Enqueued,
        Err(error) => PromptSendOutcome::Rejected {
            reason: error.to_string(),
        },
    }
}

/// Speaker tag for a transcript row: the *actually launched* agent name wins
/// (the running process, never a pending dropdown selection); before launch
/// the registry name of the per-DUT selection is used; a missing registry
/// entry falls back to a neutral tag so the row is never unlabelled.
pub fn transcript_agent_tag(
    launch: Option<&AgentLaunchSpec>,
    selected_name: Option<&str>,
) -> String {
    if let Some(spec) = launch {
        let name = spec.agent_name.trim();
        if !name.is_empty() {
            return name.to_owned();
        }
        return spec.agent_id.clone();
    }
    match selected_name.map(str::trim) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => "AGENT".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// W4c: Authenticode badge (program source vs external program)
// ---------------------------------------------------------------------------

/// Which program's provenance the Authenticode badge describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramSource {
    /// Legacy variant: the build-time embedded goose that duthub used to ship.
    ///
    /// Nothing is embedded any more (2026-11-24, goose is downloaded through the
    /// tool-downloads page), so no product path selects this for a *session* —
    /// only the Licenses page still renders it to state that goose is not
    /// bundled. Kept as a variant because `pages.rs` names it.
    Embedded,
    /// A program duthub did not verify: a configured external program, a
    /// downloaded install or a PATH binary. duthub never runs a signature-chain
    /// check, so the badge must say 「未验证」 (unverified) — never 「未签名」,
    /// which would claim the *absence* of a signature had been proven.
    External,
}

/// Badge projection for the program source + recorded signature string.
/// The bin layer maps each variant onto its localized text — external
/// programs must render as 「未验证」 (unverified), never 「未签名」.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticodeBadge {
    /// External program: no signature-chain verification was performed.
    ExternalUnverified,
    /// Embedded goose whose manifest records `NotSigned`.
    EmbeddedNotSigned,
    /// Embedded goose with a manifest-recorded status string.
    EmbeddedStatus(String),
}

/// Project the source + signature onto the badge variant.
pub fn authenticode_badge(source: ProgramSource, signature: &str) -> AuthenticodeBadge {
    match source {
        ProgramSource::External => AuthenticodeBadge::ExternalUnverified,
        ProgramSource::Embedded if signature.eq_ignore_ascii_case("NotSigned") => {
            AuthenticodeBadge::EmbeddedNotSigned
        }
        ProgramSource::Embedded => AuthenticodeBadge::EmbeddedStatus(signature.to_owned()),
    }
}

/// Resolve the badge source for a DUT session: the launch spec wins once a
/// process has been recorded; before launch there is no verified program.
///
/// With the embedded build gone, a session runs either a configured external
/// program or the downloaded/PATH goose — in every case duthub did not verify a
/// signature chain, so the honest projection is [`ProgramSource::External`].
/// `_selected_has_external_program` is retained for source compatibility with
/// `pages.rs`, which still passes the per-DUT selection.
pub fn program_source(
    launch: Option<&AgentLaunchSpec>,
    _selected_has_external_program: bool,
) -> ProgramSource {
    match launch {
        // Defensive: a hand-built spec that still claims `embedded` keeps the
        // manifest-style projection instead of silently reading as unverified.
        Some(spec) if spec.embedded => ProgramSource::Embedded,
        _ => ProgramSource::External,
    }
}

// ---------------------------------------------------------------------------
// W4c: Inspector projections (audit filter + permission-denial entries)
// ---------------------------------------------------------------------------

/// Inspector audit filter: keep only `tool.call` events issued under the
/// running session's credential. A session without a credential id never
/// inherits unattributed events (GUI writes, stdio admin) — an empty list is
/// the honest empty state.
pub fn inspector_audit_events(
    recent: &[AuditEvent],
    credential_id: Option<&str>,
) -> Vec<AuditEvent> {
    let Some(credential) = credential_id else {
        return Vec::new();
    };
    recent
        .iter()
        .filter(|event| {
            event.action == "tool.call" && event.credential_id.as_deref() == Some(credential)
        })
        .cloned()
        .collect()
}

/// Transcript System entries that record a permission denial. The host
/// worker writes `AcpEvent::PermissionDenied` as 「已拒绝权限请求：{title}」;
/// the Inspector lists them as the session's permission-denial evidence.
pub fn permission_denial_entries(entries: &[TranscriptEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| {
            entry.kind == TranscriptKind::System && entry.text.starts_with("已拒绝权限请求")
        })
        .map(|entry| entry.text.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Client config snippets (per-DUT MCP listener + A2A discovery)
// ---------------------------------------------------------------------------

/// `.mcp.json` fragment for one per-DUT listener. The Bearer token is embedded
/// verbatim: this text exists to be pasted into a client config, and the panel
/// already shows the same token for copying.
pub fn mcp_client_config(server_name: &str, url: &str, token: &str) -> String {
    let value = serde_json::json!({
        "mcpServers": {
            server_name: {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {token}") },
            },
        },
    });
    let mut text = serde_json::to_string_pretty(&value)
        .expect("MCP client config is a JSON object with string leaves");
    text.push('\n');
    text
}

/// A2A client config: the discovery URL plus the Bearer header. The card URL
/// is only produced while the per-DUT listener is actually running.
pub fn a2a_client_config(card_url: &str, token: &str) -> String {
    let value = serde_json::json!({
        "agentCardUrl": card_url,
        "headers": { "Authorization": format!("Bearer {token}") },
    });
    let mut text = serde_json::to_string_pretty(&value)
        .expect("A2A client config is a JSON object with string leaves");
    text.push('\n');
    text
}

/// Stable client-config key for one DUT: `duthub-<slug>` where the slug is the
/// stable id lowercased with non-alphanumerics folded to single dashes.
pub fn mcp_server_name(id: &str) -> String {
    let mut slug = String::new();
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "duthub".to_string()
    } else {
        format!("duthub-{slug}")
    }
}
