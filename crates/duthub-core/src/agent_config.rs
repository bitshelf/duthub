//! External Agent registry and its per-DUT bindings.
//!
//! Store the versioned registry under the work directory.
//! Persist it atomically and back up an invalid file before returning the parse error.
//! Initialize a missing registry with the default goose entry.
//! Store environment variable names only.
//! Their values come from the system credential store under `acp/<id>/env/<KEY>`.
//! Store DUT choices separately in `agent-bindings.json`.
//! This preserves the existing DUT configuration schema.
//!
//! This module handles data and persistence. The desktop probes executable metadata.
//! `agent_host::registry`。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const AGENTS_FILE_VERSION: u32 = 1;
pub const AGENT_BINDINGS_FILE_VERSION: u32 = 1;
pub const DEFAULT_AGENT_ID: &str = "goose";
pub const AGENTS_FILE_NAME: &str = "agents.json";
pub const AGENT_BINDINGS_FILE_NAME: &str = "agent-bindings.json";

/// Sandbox/isolation variables duthub owns for every ACP child. `envKeys` may
/// never assign these: they would let a registry entry override the per-session
/// TEMP/TMP redirect, the inherited system PATH, or goose's managed mode/paths.
pub const RESERVED_ENV_KEYS: &[&str] = &[
    "TEMP",
    "TMP",
    "PATH",
    "SystemRoot",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
    "GOOSE_PATH_ROOT",
    "GOOSE_MODE",
    "GOOSE_DISABLE_KEYRING",
];

/// Case-insensitive membership test for [`RESERVED_ENV_KEYS`].
#[must_use]
pub fn is_reserved_env_key(key: &str) -> bool {
    let key = key.trim();
    RESERVED_ENV_KEYS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(key))
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentConfigError {
    #[error("Agent 配置 I/O 失败: {0}")]
    Io(String),
    #[error("Agent 配置损坏: {0}")]
    Parse(String),
    #[error("Agent 配置无效: {0}")]
    Invalid(String),
}

/// Transport used to launch an Agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Acp,
    Cli,
}

impl AgentKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Acp => "ACP",
            AgentKind::Cli => "CLI",
        }
    }
}

/// Program metadata for one Agent. Credentials are stored separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefinition {
    pub id: String,
    pub name: String,
    pub kind: AgentKind,
    /// A missing goose executable path uses the installed tool. External paths must be absolute.
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variable key names only; secret values live in keyring `acp/<id>/env/<KEY>`.
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Profile identifier when `kind == AgentKind::Cli` (e.g., `claude-code`).
    #[serde(default)]
    pub cli_profile: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Authentication metadata (e.g., `inherit` or `api_key`).
    #[serde(default = "default_auth_mode")]
    pub auth_mode: String,
    /// Trust tier metadata (`mcp_only`, `verified_readonly`, `os_sandboxed`).
    #[serde(default = "default_trust_level")]
    pub trust_level: String,
    /// Pinned lowercase hex SHA-256 of the external executable; verified before spawn.
    #[serde(default)]
    pub expected_sha256: Option<String>,
}

fn default_enabled() -> bool {
    true
}

fn default_auth_mode() -> String {
    "inherit".into()
}

fn default_trust_level() -> String {
    "mcp_only".into()
}

impl AgentDefinition {
    /// The default goose entry resolves to an installed executable or one on PATH.
    #[must_use]
    pub fn goose() -> Self {
        Self {
            id: DEFAULT_AGENT_ID.into(),
            name: "goose".into(),
            kind: AgentKind::Acp,
            program: None,
            args: Vec::new(),
            env_keys: Vec::new(),
            cli_profile: None,
            enabled: true,
            auth_mode: "inherit".into(),
            trust_level: "mcp_only".into(),
            expected_sha256: None,
        }
    }

    #[must_use]
    pub fn is_goose(&self) -> bool {
        self.id == DEFAULT_AGENT_ID
    }

    /// External executable path. An empty string is treated as missing.
    pub fn program_path(&self) -> Option<&str> {
        self.program
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
    }
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self::goose()
    }
}

/// Root object of the Agent registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentsFile {
    pub version: u32,
    #[serde(default)]
    pub agents: Vec<AgentDefinition>,
}

impl Default for AgentsFile {
    fn default() -> Self {
        Self {
            version: AGENTS_FILE_VERSION,
            agents: vec![AgentDefinition::goose()],
        }
    }
}

impl AgentsFile {
    #[must_use]
    pub fn agent(&self, id: &str) -> Option<&AgentDefinition> {
        let id = id.trim();
        self.agents.iter().find(|agent| agent.id == id)
    }

    pub fn agent_mut(&mut self, id: &str) -> Option<&mut AgentDefinition> {
        let id = id.trim();
        self.agents.iter_mut().find(|agent| agent.id == id)
    }

    #[must_use]
    pub fn goose(&self) -> Option<&AgentDefinition> {
        self.agent(DEFAULT_AGENT_ID)
    }

    pub fn goose_mut(&mut self) -> Option<&mut AgentDefinition> {
        self.agent_mut(DEFAULT_AGENT_ID)
    }

    /// List launchable ACP entries for the desktop selector.
    pub fn acp_agents(&self) -> impl Iterator<Item = &AgentDefinition> {
        self.agents
            .iter()
            .filter(|agent| agent.enabled && agent.kind == AgentKind::Acp)
    }
}

/// Build the default registry with its goose entry.
#[must_use]
pub fn default_agents() -> AgentsFile {
    AgentsFile::default()
}

#[must_use]
pub fn agents_path(work_dir: &Path) -> PathBuf {
    work_dir.join(AGENTS_FILE_NAME)
}

#[must_use]
pub fn agent_bindings_path(work_dir: &Path) -> PathBuf {
    work_dir.join(AGENT_BINDINGS_FILE_NAME)
}

/// Load the registry, initialize a missing file, and report corrupt files after backing them up.
pub fn load_agents(work_dir: &Path) -> Result<AgentsFile, AgentConfigError> {
    let path = agents_path(work_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file = default_agents();
            save_agents_atomic(work_dir, &file)?;
            return Ok(file);
        }
        Err(error) => return Err(AgentConfigError::Io(error.to_string())),
    };
    match serde_json::from_str::<AgentsFile>(&raw) {
        Ok(file) => {
            validate_agents(&file)?;
            Ok(file)
        }
        Err(error) => {
            let backup = backup_corrupt(&path);
            Err(AgentConfigError::Parse(format!(
                "{error}（已备份为 {}）",
                backup.display()
            )))
        }
    }
}

/// Validate the registry and save it atomically.
pub fn save_agents_atomic(work_dir: &Path, file: &AgentsFile) -> Result<(), AgentConfigError> {
    validate_agents(file)?;
    save_json_atomic(work_dir, &agents_path(work_dir), file)
}

fn validate_agents(file: &AgentsFile) -> Result<(), AgentConfigError> {
    if file.version != AGENTS_FILE_VERSION {
        return Err(AgentConfigError::Invalid(format!(
            "不支持的 agents.json 版本 {}（期望 {}）",
            file.version, AGENTS_FILE_VERSION
        )));
    }
    if file.agents.is_empty() {
        return Err(AgentConfigError::Invalid(
            "agents 至少需要保留一个条目（默认 goose）".into(),
        ));
    }
    let mut ids: HashSet<&str> = HashSet::new();
    for agent in &file.agents {
        validate_agent_id(&agent.id)?;
        if !ids.insert(agent.id.as_str()) {
            return Err(AgentConfigError::Invalid(format!(
                "Agent id 重复: {}",
                agent.id
            )));
        }
        if agent.name.trim().is_empty() {
            return Err(AgentConfigError::Invalid(format!(
                "Agent {} 的名称不能为空",
                agent.id
            )));
        }
        if let Some(program) = agent.program.as_deref()
            && program.trim().is_empty()
        {
            return Err(AgentConfigError::Invalid(format!(
                "Agent {} 的 program 为空串；留空请使用 null",
                agent.id
            )));
        }
        if agent.program_path().is_none() && !agent.is_goose() && agent.kind == AgentKind::Acp {
            return Err(AgentConfigError::Invalid(format!(
                "Agent {} 不是 goose，必须提供外部程序路径",
                agent.id
            )));
        }
        for arg in &agent.args {
            if arg.contains('\0') {
                return Err(AgentConfigError::Invalid(format!(
                    "Agent {} 的 args 含 NUL 字符",
                    agent.id
                )));
            }
        }
        for key in &agent.env_keys {
            validate_env_key(key)?;
        }
        if let Some(expected) = agent.expected_sha256.as_deref() {
            validate_sha256(expected)?;
        }
    }
    Ok(())
}

pub fn validate_agent_id(id: &str) -> Result<(), AgentConfigError> {
    let id = id.trim();
    if id.is_empty() || id.len() > 64 {
        return Err(AgentConfigError::Invalid(
            "Agent id 必须为 1-64 个字符".into(),
        ));
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AgentConfigError::Invalid(format!(
            "Agent id 只能包含 ASCII 字母、数字、-、_、.: {id}"
        )));
    }
    Ok(())
}

pub fn validate_env_key(key: &str) -> Result<(), AgentConfigError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(AgentConfigError::Invalid("envKeys 含空键名".into()));
    }
    let mut bytes = key.bytes();
    let first = bytes.next().unwrap_or(b'_');
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(AgentConfigError::Invalid(format!(
            "envKeys 键名必须是 [A-Za-z_][A-Za-z0-9_]*: {key}"
        )));
    }
    if is_reserved_env_key(key) {
        return Err(AgentConfigError::Invalid(format!(
            "envKeys 不能包含 duthub 保留变量 {key}（沙箱/隔离变量由 duthub 管理）"
        )));
    }
    Ok(())
}

fn validate_sha256(expected: &str) -> Result<(), AgentConfigError> {
    let expected = expected.trim();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AgentConfigError::Invalid(
            "expectedSha256 必须是 64 位十六进制".into(),
        ));
    }
    Ok(())
}

/// Credential entry name under the duthub service.
#[must_use]
pub fn agent_env_entry(agent_id: &str, key: &str) -> String {
    format!("acp/{agent_id}/env/{key}")
}

/// Read Agent environment values using an injected credential lookup.
pub fn load_agent_env_with<F>(
    agent: &AgentDefinition,
    lookup: F,
) -> Result<Vec<(String, String)>, AgentConfigError>
where
    F: Fn(&str) -> Result<Option<String>, String>,
{
    let mut values = Vec::with_capacity(agent.env_keys.len());
    let mut seen: HashSet<&str> = HashSet::new();
    for key in &agent.env_keys {
        let key = key.trim();
        validate_env_key(key)?;
        if !seen.insert(key) {
            return Err(AgentConfigError::Invalid(format!(
                "Agent {} 的 envKeys 重复: {key}",
                agent.id
            )));
        }
        let entry = agent_env_entry(&agent.id, key);
        match lookup(&entry) {
            Ok(Some(value)) if !value.trim().is_empty() => {
                values.push((key.to_owned(), value));
            }
            Ok(_) => {
                return Err(AgentConfigError::Invalid(format!(
                    "环境变量 {key} 未在系统凭据库中保存（条目 {entry}）"
                )));
            }
            Err(error) => {
                return Err(AgentConfigError::Invalid(format!(
                    "读取环境变量 {key} 失败（条目 {entry}）: {error}"
                )));
            }
        }
    }
    Ok(values)
}

/// Read Agent environment variable values from the OS credential store.
pub fn load_agent_env(agent: &AgentDefinition) -> Result<Vec<(String, String)>, AgentConfigError> {
    load_agent_env_with(agent, crate::keys::get_raw_entry)
}

/// Read a single Agent environment variable from the OS credential store.
pub fn get_agent_env(agent_id: &str, key: &str) -> Result<Option<String>, AgentConfigError> {
    get_agent_env_with(agent_id, key, crate::keys::get_raw_entry)
}

/// Look up a single environment value using an injected reader.
pub fn get_agent_env_with<F>(
    agent_id: &str,
    key: &str,
    lookup: F,
) -> Result<Option<String>, AgentConfigError>
where
    F: Fn(&str) -> Result<Option<String>, String>,
{
    validate_agent_id(agent_id)?;
    validate_env_key(key)?;
    let agent_id = agent_id.trim();
    let key = key.trim();
    let entry = agent_env_entry(agent_id, key);
    lookup(&entry).map_err(|error| {
        AgentConfigError::Invalid(format!("读取环境变量 {key} 失败（条目 {entry}）: {error}"))
    })
}

/// Write a single Agent environment variable into the OS credential store.
pub fn set_agent_env(agent_id: &str, key: &str, value: &str) -> Result<(), AgentConfigError> {
    set_agent_env_with(agent_id, key, value, crate::keys::set_raw_entry)
}

/// Save an environment value using an injected writer.
pub fn set_agent_env_with<F>(
    agent_id: &str,
    key: &str,
    value: &str,
    store: F,
) -> Result<(), AgentConfigError>
where
    F: Fn(&str, &str) -> Result<(), String>,
{
    validate_agent_id(agent_id)?;
    validate_env_key(key)?;
    if value.trim().is_empty() {
        return Err(AgentConfigError::Invalid(format!(
            "环境变量 {key} 的值不能为空；如需删除请使用删除操作"
        )));
    }
    if value.contains('\0') {
        return Err(AgentConfigError::Invalid(format!(
            "环境变量 {key} 的值包含 NUL 字符"
        )));
    }
    let agent_id = agent_id.trim();
    let key = key.trim();
    let entry = agent_env_entry(agent_id, key);
    store(&entry, value).map_err(|error| {
        AgentConfigError::Invalid(format!("写入环境变量 {key} 失败（条目 {entry}）: {error}"))
    })
}

/// Delete a single Agent environment variable from the OS credential store (idempotent).
pub fn delete_agent_env(agent_id: &str, key: &str) -> Result<(), AgentConfigError> {
    delete_agent_env_with(agent_id, key, crate::keys::delete_raw_entry)
}

/// Delete an environment credential using an injected remover.
pub fn delete_agent_env_with<F>(
    agent_id: &str,
    key: &str,
    delete: F,
) -> Result<(), AgentConfigError>
where
    F: Fn(&str) -> Result<(), String>,
{
    validate_agent_id(agent_id)?;
    validate_env_key(key)?;
    let agent_id = agent_id.trim();
    let key = key.trim();
    let entry = agent_env_entry(agent_id, key);
    delete(&entry).map_err(|error| {
        AgentConfigError::Invalid(format!("删除环境变量 {key} 失败（条目 {entry}）: {error}"))
    })
}

/// Per-DUT selection of an Agent ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBindingsFile {
    pub version: u32,
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    enabled: BTreeMap<String, bool>,
}

impl Default for AgentBindingsFile {
    fn default() -> Self {
        Self {
            version: AGENT_BINDINGS_FILE_VERSION,
            bindings: BTreeMap::new(),
            enabled: BTreeMap::new(),
        }
    }
}

impl AgentBindingsFile {
    /// Return this DUT's Agent ID or the default goose entry.
    #[must_use]
    pub fn agent_id_for(&self, dut_id: &str) -> String {
        self.bindings
            .get(dut_id.trim())
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .unwrap_or(DEFAULT_AGENT_ID)
            .to_owned()
    }

    pub fn set_agent_id(&mut self, dut_id: &str, agent_id: &str) {
        let dut_id = dut_id.trim();
        if dut_id.is_empty() {
            return;
        }
        let agent_id = agent_id.trim();
        if agent_id.is_empty() || agent_id == DEFAULT_AGENT_ID {
            self.bindings.remove(dut_id);
        } else {
            self.bindings.insert(dut_id.to_owned(), agent_id.to_owned());
        }
    }

    #[must_use]
    pub fn is_enabled(&self, dut_id: &str) -> bool {
        let dut_id = dut_id.trim();
        self.enabled.get(dut_id).copied().unwrap_or(false)
    }

    pub fn set_enabled(&mut self, dut_id: &str, enabled: bool) {
        let dut_id = dut_id.trim();
        if !dut_id.is_empty() {
            if enabled {
                self.enabled.insert(dut_id.to_owned(), true);
            } else {
                self.enabled.remove(dut_id);
            }
        }
    }

    pub fn remove_dut(&mut self, dut_id: &str) -> bool {
        let dut_id = dut_id.trim();
        let binding = self.bindings.remove(dut_id);
        let enabled = self.enabled.remove(dut_id);
        binding.is_some() || enabled.is_some()
    }

    fn validate(&self) -> Result<(), AgentConfigError> {
        if self.version != AGENT_BINDINGS_FILE_VERSION {
            return Err(AgentConfigError::Invalid(format!(
                "不支持的 agent-bindings.json 版本 {}（期望 {}）",
                self.version, AGENT_BINDINGS_FILE_VERSION
            )));
        }
        for (dut_id, agent_id) in &self.bindings {
            if dut_id.trim().is_empty() {
                return Err(AgentConfigError::Invalid(
                    "agent-bindings.json 含空 DUT id".into(),
                ));
            }
            validate_agent_id(agent_id)?;
        }
        Ok(())
    }
}

/// Load the bindings. A missing file uses defaults and a corrupt file is backed up.
pub fn load_agent_bindings(work_dir: &Path) -> Result<AgentBindingsFile, AgentConfigError> {
    let path = agent_bindings_path(work_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentBindingsFile::default());
        }
        Err(error) => return Err(AgentConfigError::Io(error.to_string())),
    };
    match serde_json::from_str::<AgentBindingsFile>(&raw) {
        Ok(file) => {
            file.validate()?;
            Ok(file)
        }
        Err(error) => {
            let backup = backup_corrupt(&path);
            Err(AgentConfigError::Parse(format!(
                "{error}（已备份为 {}）",
                backup.display()
            )))
        }
    }
}

pub fn save_agent_bindings_atomic(
    work_dir: &Path,
    file: &AgentBindingsFile,
) -> Result<(), AgentConfigError> {
    file.validate()?;
    save_json_atomic(work_dir, &agent_bindings_path(work_dir), file)
}

fn save_json_atomic<T: Serialize>(
    _work_dir: &Path,
    path: &Path,
    value: &T,
) -> Result<(), AgentConfigError> {
    crate::store::save_json_atomic(path, value)
        .map_err(|error| AgentConfigError::Io(error.to_string()))
}

fn backup_corrupt(path: &Path) -> PathBuf {
    crate::util::backup_corrupt(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "duthub-agents-config-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_registry_generates_default_goose_and_is_readable() {
        let dir = temp_dir("missing");
        let file = load_agents(&dir).unwrap();
        assert_eq!(file, default_agents());
        assert_eq!(file.agents.len(), 1);
        assert!(file.goose().is_some());
        assert!(file.goose().unwrap().program.is_none());
        assert!(
            agents_path(&dir).is_file(),
            "agents.json must be generated when missing"
        );
        assert_eq!(load_agents(&dir).unwrap(), default_agents());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_keeps_metadata_and_never_stores_env_values() {
        let dir = temp_dir("roundtrip");
        let mut file = default_agents();
        file.agents.push(AgentDefinition {
            id: "acme".into(),
            name: "Acme ACP".into(),
            kind: AgentKind::Acp,
            program: Some("C:/tools/acme.exe".into()),
            args: vec!["--acp".into(), "--stdio".into()],
            env_keys: vec!["ACME_API_KEY".into()],
            cli_profile: None,
            enabled: true,
            auth_mode: "inherit".into(),
            trust_level: "mcp_only".into(),
            expected_sha256: Some("a".repeat(64)),
        });
        file.agents.push(AgentDefinition {
            id: "claude".into(),
            name: "Claude Code".into(),
            kind: AgentKind::Cli,
            program: None,
            args: vec!["-p".into()],
            env_keys: Vec::new(),
            cli_profile: Some("claude-code".into()),
            enabled: false,
            auth_mode: "api_key".into(),
            trust_level: "mcp_only".into(),
            expected_sha256: None,
        });
        save_agents_atomic(&dir, &file).unwrap();
        let raw = std::fs::read_to_string(agents_path(&dir)).unwrap();
        assert!(raw.contains("\"envKeys\""));
        assert!(raw.contains("\"cliProfile\""));
        assert!(raw.contains("\"expectedSha256\""));
        assert!(!raw.contains("sk-secret-value"), "{raw}");
        assert!(!raw.contains("\"token\""), "{raw}");
        assert!(!raw.contains("\"apiKey\""), "{raw}");
        assert_eq!(load_agents(&dir).unwrap(), file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_registry_is_backed_up_and_fails_closed() {
        let dir = temp_dir("corrupt");
        std::fs::write(agents_path(&dir), "{not valid json").unwrap();
        let error = load_agents(&dir).unwrap_err();
        assert!(matches!(error, AgentConfigError::Parse(_)), "{error}");
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("agents.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "corrupt file must be backed up");
        assert!(
            !agents_path(&dir).exists(),
            "corrupt file must be moved aside"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ids_and_external_non_goose_without_program_are_rejected() {
        let dir = temp_dir("invalid");
        let mut file = default_agents();
        file.agents.push(AgentDefinition::goose());
        assert!(save_agents_atomic(&dir, &file).is_err());

        let mut file = default_agents();
        file.agents.push(AgentDefinition {
            id: "acme".into(),
            name: "Acme".into(),
            kind: AgentKind::Acp,
            program: None,
            args: Vec::new(),
            env_keys: Vec::new(),
            cli_profile: None,
            enabled: true,
            auth_mode: "inherit".into(),
            trust_level: "mcp_only".into(),
            expected_sha256: None,
        });
        assert!(save_agents_atomic(&dir, &file).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_values_come_from_keyring_lookup_not_registry() {
        let mut agent = AgentDefinition::goose();
        agent.env_keys = vec!["ACME_API_KEY".into()];
        let entry = agent_env_entry("goose", "ACME_API_KEY");
        assert_eq!(entry, "acp/goose/env/ACME_API_KEY");
        let values = load_agent_env_with(&agent, |key| {
            assert_eq!(key, entry);
            Ok(Some("top-secret".into()))
        })
        .unwrap();
        assert_eq!(values, [("ACME_API_KEY".to_owned(), "top-secret".into())]);

        let error = load_agent_env_with(&agent, |_| Ok(None)).unwrap_err();
        assert!(
            error.to_string().contains("未在系统凭据库中保存"),
            "{error}"
        );
    }

    #[test]
    fn reserved_env_keys_are_rejected_case_insensitively() {
        for key in [
            "TEMP",
            "tmp",
            "Path",
            "SystemRoot",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "HOME",
            "HOMEDRIVE",
            "HOMEPATH",
            "GOOSE_PATH_ROOT",
            "GOOSE_MODE",
            "GOOSE_DISABLE_KEYRING",
        ] {
            assert!(is_reserved_env_key(key), "{key}");
            assert!(
                validate_env_key(key).is_err(),
                "reserved env key {key} must be rejected"
            );
        }
        assert!(validate_env_key("ACME_API_KEY").is_ok());
        assert!(!is_reserved_env_key("ACME_API_KEY"));
    }

    #[test]
    fn injectable_env_store_sets_gets_and_deletes_with_validation() {
        let store = std::sync::Mutex::new(std::collections::HashMap::<String, String>::new());
        set_agent_env_with("acme", "ACME_API_KEY", "top-secret", |entry, value| {
            store
                .lock()
                .unwrap()
                .insert(entry.to_owned(), value.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            store.lock().unwrap().get("acp/acme/env/ACME_API_KEY"),
            Some(&"top-secret".to_owned())
        );

        let value = get_agent_env_with("acme", "ACME_API_KEY", |entry| {
            Ok(store.lock().unwrap().get(entry).cloned())
        })
        .unwrap();
        assert_eq!(value.as_deref(), Some("top-secret"));

        delete_agent_env_with("acme", "ACME_API_KEY", |entry| {
            store.lock().unwrap().remove(entry);
            Ok(())
        })
        .unwrap();
        assert!(store.lock().unwrap().is_empty());

        // Reject reserved keys and empty values before calling the credential store.
        let called = std::sync::atomic::AtomicBool::new(false);
        assert!(
            set_agent_env_with("acme", "PATH", "x", |_, _| {
                called.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })
            .is_err()
        );
        assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
        assert!(set_agent_env_with("acme", "ACME_KEY", "   ", |_, _| Ok(())).is_err());
        assert!(get_agent_env_with("acme", "GOOSE_MODE", |_| Ok(None)).is_err());
    }

    #[test]
    fn per_dut_bindings_roundtrip_and_default_to_goose() {
        let dir = temp_dir("bindings");
        let mut bindings = load_agent_bindings(&dir).unwrap();
        assert_eq!(bindings.agent_id_for("dut-a"), DEFAULT_AGENT_ID);
        bindings.set_agent_id("dut-a", "acme");
        bindings.set_agent_id("dut-b", "goose");
        save_agent_bindings_atomic(&dir, &bindings).unwrap();
        let reloaded = load_agent_bindings(&dir).unwrap();
        assert_eq!(reloaded.agent_id_for("dut-a"), "acme");
        assert_eq!(reloaded.agent_id_for("dut-b"), DEFAULT_AGENT_ID);
        assert_eq!(reloaded.agent_id_for("missing"), DEFAULT_AGENT_ID);
        // Omit an explicit binding for the default goose entry.
        assert!(!reloaded.bindings.contains_key("dut-b"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_bindings_fail_closed_with_backup() {
        let dir = temp_dir("bindings-corrupt");
        std::fs::write(agent_bindings_path(&dir), "{bad").unwrap();
        assert!(load_agent_bindings(&dir).is_err());
        let backups = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("agent-bindings.corrupt-")
            })
            .count();
        assert_eq!(backups, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
