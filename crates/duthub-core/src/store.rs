//! settings.json 原子读写（`docs/SECURITY.md` #9：临时文件 + fsync + rename）。
//!
//! 配置目录（settings.json / duts.json）固定在可执行文件同级的 `.duthub/`。
//! 工作目录独立解析，承载日志、采集和运行时快照。

use std::fs::OpenOptions;
use std::io::Write;
use std::mem::DropGuard;
use std::path::{Path, PathBuf};

use crate::error::{DeviceError, DeviceResult};
use crate::model::{
    Appearance, DutsFile, FlashToolPreset, SerialSettings, SettingsFile, TerminalSettings,
    ToolsSettings,
};
pub use crate::util::{sha256_file, sha256_hex};

const CONFIG_DIR: &str = ".duthub";

/// 可执行文件所在目录（路径解析锚点）。
pub fn exe_dir() -> DeviceResult<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| DeviceError::Invalid(e.to_string()))?;
    exe.parent()
        .ok_or(DeviceError::NotFound)
        .map(Path::to_path_buf)
}

/// The explicit workspace also owns configuration and downloaded tools.
pub fn data_dir() -> DeviceResult<PathBuf> {
    let anchor = exe_dir()?;
    let root = explicit_workspace(&anchor, std::env::args_os().skip(1))?.unwrap_or(anchor);
    Ok(config_dir_in(&root))
}

/// Resolve an explicitly supplied workspace before consulting any settings.
pub fn explicit_workspace(
    anchor: &Path,
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> DeviceResult<Option<PathBuf>> {
    let mut args = args.into_iter();
    let mut root = None;
    while let Some(arg) = args.next() {
        if arg != "--workdir" {
            continue;
        }
        if root.is_some() {
            return Err(DeviceError::Invalid("Duplicate --workdir".into()));
        }
        let value = args
            .next()
            .filter(|v| {
                let text = v.to_string_lossy();
                !text.trim().is_empty() && !text.starts_with("--")
            })
            .ok_or_else(|| DeviceError::Invalid("--workdir requires a directory".into()))?;
        root = Some(anchor.join(value));
    }
    Ok(root)
}
#[must_use]
pub fn config_dir_in(exe_dir: &Path) -> PathBuf {
    exe_dir.join(CONFIG_DIR)
}

/// 创建配置目录并补齐缺失的默认文件，保留已存在的配置。
pub fn initialize_config_dir(exe_dir: &Path) -> DeviceResult<PathBuf> {
    ensure_plain_child_dir(exe_dir, CONFIG_DIR)?;
    let dir = config_dir_in(exe_dir);
    create_default_config(&dir, &settings_path(&dir), &default_settings())?;
    let mut duts = default_duts();
    duts.relay_boards.push(crate::model::RelayBoard {
        id: "ch340-relay".into(),
        name: "ch340-relay".into(),
        host: "local".into(),
        node: String::new(),
        driver: "ch340".into(),
        channel_count: 4,
    });
    create_default_config(&dir, &duts_path(&dir), &duts)?;
    Ok(dir)
}

fn create_default_config(
    dir: &Path,
    path: &Path,
    value: &impl serde::Serialize,
) -> DeviceResult<()> {
    use std::io::ErrorKind;

    let create = || -> std::io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut pending = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut pending, value)?;
        pending.as_file().sync_all()?;
        match pending.persist_noclobber(path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.error),
        }
    };
    create()
        .map_err(|error| DeviceError::Invalid(format!("无法初始化 {}: {error}", path.display())))
}

/// 默认运行产物目录 = exe 同级 `duthub-data/`。
#[must_use]
pub fn default_work_dir_in(exe_dir: &Path) -> PathBuf {
    exe_dir.join("duthub-data")
}

/// Pure portable workspace root resolution: CLI `--workdir` > `workspaceDir` > `exe_dir`.
pub fn resolve_portable_work_dir_in(
    exe_dir: &Path,
    settings_dir: Option<&str>,
    cli_dir: Option<&str>,
) -> PathBuf {
    [cli_dir, settings_dir]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|path| !path.is_empty()).map_or_else(|| exe_dir.to_path_buf(), |path| exe_dir.join(path))
}

/// Resolve the desktop's trusted artifact root. Settings and startup options
/// are local operator inputs, separate from the read-only MCP interface.
pub fn ensure_portable_work_dir(
    exe_dir: &Path,
    settings_dir: Option<&str>,
    cli_dir: Option<&str>,
) -> DeviceResult<(PathBuf, Option<String>)> {
    let selected = resolve_portable_work_dir_in(exe_dir, settings_dir, cli_dir);
    if dir_writable(&selected) {
        let root = std::fs::canonicalize(&selected)
            .map_err(|error| DeviceError::Invalid(error.to_string()))?;
        ensure_portable_child(&root, "duthub-data")?;
        return Ok((root, None));
    }
    Err(DeviceError::Invalid(format!(
        "Workspace is not writable: {}",
        selected.display()
    )))
}

pub fn ensure_portable_child(root: &Path, component: &str) -> DeviceResult<()> {
    let component = crate::dut_artifacts::name_component(component)
        .map_err(|error| DeviceError::Invalid(error.to_string()))?;
    ensure_plain_child_dir(root, component)
}

fn ensure_plain_child_dir(root: &Path, component: &str) -> DeviceResult<()> {
    use cap_std::{ambient_authority, fs::Dir};
    use std::io::ErrorKind;

    let validate = || -> std::io::Result<()> {
        let root = Dir::open_ambient_dir(root, ambient_authority())?;
        match root.create_dir(component) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = root.symlink_metadata(component)?;
        if !metadata.is_dir() || !crate::dut_artifacts::plain(&metadata) {
            return Err(ErrorKind::PermissionDenied.into());
        }
        root.open_dir(component)?;
        Ok(())
    };
    validate()
        .map_err(|error| DeviceError::Invalid(format!("目录 {component} 不安全或不可用: {error}")))
}

/// 逐级创建并校验 `root` 下的普通目录链（cap-std 能力边界；junction/symlink
/// 一律拒绝）。返回最终目录的普通路径。
pub fn ensure_agent_dir(root: &Path, components: &[&str]) -> DeviceResult<PathBuf> {
    let mut current = root.to_path_buf();
    for component in components {
        let name = crate::dut_artifacts::name_component(component).map_err(|error| {
            DeviceError::Invalid(format!("目录 {component} 不能作为路径段: {error}"))
        })?;
        ensure_plain_child_dir(&current, name)?;
        current.push(name);
    }
    Ok(current)
}

/// DUT 稳定 id → 可安全用作目录名的键（UTF-8 字节的十六进制，避免路径字符、
/// 保留名与大小写差异）。空/全空白 → `None`（调用方必须显式拒绝，不能回退到
/// 固定占位名——多台空 id 会互相覆盖）。
#[must_use]
pub fn agent_dut_key(dut_id: &str) -> Option<String> {
    use std::fmt::Write as _;

    let trimmed = dut_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut key = String::with_capacity(trimmed.len() * 2);
    for byte in trimmed.as_bytes() {
        let _ = write!(key, "{byte:02x}");
    }
    Some(key)
}

/// Agent 工作目录中的每 DUT 目录：`<work_dir>/agent/dut/<key>`。
pub fn agent_dut_dir(work_dir: &Path, dut_id: &str) -> DeviceResult<PathBuf> {
    let key =
        agent_dut_key(dut_id).ok_or_else(|| DeviceError::Invalid("DUT id 不能为空".into()))?;
    ensure_agent_dir(work_dir, &["agent", "dut", &key])
}

/// 去掉 Windows verbatim 前缀（`\\?\C:\...` → `C:\...`；`\\?\UNC\server\share`
/// → `\\server\share`），便于把路径写入日志/展示与比较。
#[must_use]
pub fn plain_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

fn settings_path(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

/// 出厂默认 settings（theme dark / language zh-CN / platform windows）。
#[must_use]
pub fn default_settings() -> SettingsFile {
    SettingsFile {
        version: 1,
        appearance: Appearance {
            theme: "dark".into(),
            language: "zh-CN".into(),
            platform: "windows".into(),
        },
        terminal: TerminalSettings {
            jump_to_latest: true,
            show_command_bar: true,
            tab_numbers: false,
            tab_status_icons: true,
            include_ansi: false,
            ..TerminalSettings::default()
        },
        commands: Vec::new(),
        flash_tools: vec![FlashToolPreset {
            id: "preset-upgrade_tool".into(),
            name: "upgrade_tool".into(),
            executable: "upgrade_tool".into(),
            test_args: "--help".into(),
        }],
        agent: crate::model::AgentSettings::default(),
        tools: ToolsSettings {
            adb_executable: "adb".into(),
            hdc_executable: "hdc".into(),
            capture: crate::model::CaptureSettings::default(),
            download: crate::model::ToolDownloadSettings::default(),
        },
        a2a: None,
        workspace_dir: None,
        dut_storage_gib: None,
        alerts: crate::model::GlobalAlertSettings::default(),
        agent_access: crate::agent_access::AgentAccessSettings::default(),
        serial: SerialSettings {
            highlight_rules: crate::model::default_highlight_rules(),
            ..SerialSettings::default()
        },
    }
}

/// 加载 settings；文件缺失返回默认；损坏则改名备份后返回默认（不覆盖原文件）。
///
/// 加载后对烧录预设做 id 归一化（旧配置无 `id` → 按名称生成稳定 id）。
pub fn load_settings(dir: &Path) -> DeviceResult<SettingsFile> {
    let path = settings_path(dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(default_settings()),
        Err(e) => return Err(DeviceError::Invalid(e.to_string())),
    };
    match serde_json::from_str::<SettingsFile>(&raw) {
        Ok(mut settings) => {
            normalize_preset_ids(&mut settings.flash_tools);
            seed_default_highlight_rules(&mut settings);
            Ok(settings)
        }
        Err(err) => {
            let backup = crate::util::backup_corrupt(&path);
            Err(DeviceError::Invalid(format!(
                "settings.json 损坏（已备份为 {}）：{err}",
                backup.display()
            )))
        }
    }
}

/// Seed the built-in prompt rules when none are configured (fresh install, an
/// old settings.json without the serial section, or a cleared list). The seeded
/// entries are ordinary rules: the user can disable, edit or delete them.
fn seed_default_highlight_rules(settings: &mut SettingsFile) {
    if settings.serial.highlight_rules.is_empty() {
        settings.serial.highlight_rules = crate::model::default_highlight_rules();
    } else {
        use crate::model::PromptRule;
        for rule in &mut settings.serial.highlight_rules {
            if rule.regex == PromptRule::OLD_ANDROID_HOST_PATTERN {
                rule.regex = PromptRule::ANDROID_HOST_PATTERN.into();
            }
        }
    }
}

/// 为烧录预设补齐稳定 `id`（slug + 去重），保证同一预设跨启动 id 不变。
pub fn normalize_preset_ids(presets: &mut [FlashToolPreset]) {
    for preset in presets.iter_mut() {
        if preset.id.is_empty() {
            preset.id = preset_slug(&preset.name);
        }
    }
    // 去重：同名/同 slug 时追加序号（稳定顺序 = 列表顺序）
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for preset in presets.iter_mut() {
        let mut candidate = preset.id.clone();
        let mut n = 1;
        while !seen.insert(candidate.clone()) {
            n += 1;
            candidate = format!("{}-{n}", preset.id);
        }
        preset.id = candidate;
    }
}

fn preset_slug(name: &str) -> String {
    let slug: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "preset".into()
    } else {
        format!("preset-{slug}")
    }
}

/// Atomically save `settings.json` (temp + fsync + rename).
pub fn save_settings_atomic(dir: &Path, settings: &SettingsFile) -> DeviceResult<()> {
    save_config_atomic(dir, &settings_path(dir), "settings", settings)
}

/// Persist MCP Bearer tokens into the OS keyring (`duthub/mcp/<id>`) and save
/// `settings.json` without any plaintext Bearer tokens on disk.
pub fn save_settings_secured(dir: &Path, settings: &SettingsFile) -> DeviceResult<()> {
    let mut sanitized = settings.clone();
    for cred in &mut sanitized.agent_access.agents {
        if !cred.token.is_empty() {
            crate::keys::set_mcp_token(&cred.id, &cred.token)?;
            cred.token.clear();
        }
    }
    save_settings_atomic(dir, &sanitized)
}

/// Structured warning emitted while hydrating or migrating MCP Bearer tokens
/// between `settings.json` and the OS keyring. Callers localize these variants
/// in the UI layer rather than relying on hardcoded strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTokenHydrateWarning {
    MigrateFailed {
        credential_id: String,
        error: String,
    },
    MissingInKeyring {
        credential_id: String,
    },
    ReadFailed {
        credential_id: String,
        error: String,
    },
    RewriteFailed {
        error: String,
    },
}

/// Hydrate MCP Bearer tokens from the OS keyring on startup, migrating any
/// legacy plaintext tokens out of `settings.json` into the keyring and
/// rewriting `settings.json` atomically so plaintext tokens do not linger on disk.
pub fn hydrate_mcp_tokens(dir: &Path, settings: &mut SettingsFile) -> Vec<McpTokenHydrateWarning> {
    let mut warnings = Vec::new();
    let mut migrated = false;
    for cred in &mut settings.agent_access.agents {
        if cred.token.is_empty() {
            match crate::keys::get_mcp_token(&cred.id) {
                Ok(Some(token)) => cred.token = token,
                Ok(None) => warnings.push(McpTokenHydrateWarning::MissingInKeyring {
                    credential_id: cred.id.clone(),
                }),
                Err(err) => warnings.push(McpTokenHydrateWarning::ReadFailed {
                    credential_id: cred.id.clone(),
                    error: err.to_string(),
                }),
            }
        } else {
            match crate::keys::set_mcp_token(&cred.id, &cred.token) {
                Ok(()) => migrated = true,
                Err(err) => warnings.push(McpTokenHydrateWarning::MigrateFailed {
                    credential_id: cred.id.clone(),
                    error: err.to_string(),
                }),
            }
        }
    }
    if migrated && let Err(err) = save_settings_secured(dir, settings) {
        warnings.push(McpTokenHydrateWarning::RewriteFailed {
            error: err.to_string(),
        });
    }
    warnings
}

/// Export settings without the bearer credentials stored in the live configuration.
pub fn export_settings_atomic(dir: &Path, settings: &SettingsFile) -> DeviceResult<()> {
    let mut exported = settings.clone();
    exported.agent_access.agents.clear();
    save_settings_atomic(dir, &exported)
}

/// Atomically replace a JSON file using a temporary file in the same directory.
pub fn save_json_atomic(path: &Path, value: &impl serde::Serialize) -> DeviceResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("json");
    save_config_atomic(parent, path, name, value)
}

fn save_config_atomic(
    dir: &Path,
    path: &Path,
    name: &str,
    value: &impl serde::Serialize,
) -> DeviceResult<()> {
    std::fs::create_dir_all(dir).map_err(|e| DeviceError::Invalid(e.to_string()))?;
    let encoded =
        serde_json::to_string_pretty(value).map_err(|e| DeviceError::Invalid(e.to_string()))?;
    let temp = dir.join(format!(
        ".{name}-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| DeviceError::Invalid(e.to_string()))?;
    let pending = DropGuard::new(temp, |temp| {
        let _ = std::fs::remove_file(temp);
    });
    // Windows 必须先关闭文件句柄，再重命名或在失败时清理。
    let written = {
        let mut file = file;
        file.write_all(encoded.as_bytes())
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
    };
    written.map_err(|e| DeviceError::Invalid(e.to_string()))?;
    std::fs::rename(pending.as_path(), path)
        .map_err(|e| DeviceError::Invalid(format!("{name} 原子重命名失败: {e}")))?;
    DropGuard::dismiss(pending);
    Ok(())
}

/// Probe whether `dir` can be created and written to using an auto-deleted
/// unique temporary file (safe under concurrent callers).
#[must_use]
pub fn dir_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    tempfile::NamedTempFile::new_in(dir)
        .and_then(|file| file.as_file().sync_all())
        .is_ok()
}

fn duts_path(dir: &Path) -> PathBuf {
    dir.join("duts.json")
}

/// 新配置从空的 DUT 列表开始。
#[must_use]
pub fn default_duts() -> DutsFile {
    DutsFile {
        version: 1,
        hosts: Vec::new(),
        relay_boards: Vec::new(),
        alerts_by_dut: std::collections::BTreeMap::new(),
        garbled_by_dut: std::collections::BTreeMap::new(),
        duts: Vec::new(),
    }
}

/// 加载 duts.json；缺失 → `None`；损坏 → 改名备份后返回错误。
pub fn load_duts(dir: &Path) -> DeviceResult<Option<crate::model::DutsFile>> {
    let path = duts_path(dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DeviceError::Invalid(e.to_string())),
    };
    match serde_json::from_str(&raw) {
        Ok(file) => Ok(Some(file)),
        Err(err) => {
            let backup = crate::util::backup_corrupt(&path);
            Err(DeviceError::Invalid(format!(
                "duts.json 损坏（已备份为 {}）：{err}",
                backup.display()
            )))
        }
    }
}

/// 原子保存 duts.json（temp + fsync + rename）。
pub fn save_duts_atomic(dir: &Path, duts: &crate::model::DutsFile) -> DeviceResult<()> {
    save_config_atomic(dir, &duts_path(dir), "duts", duts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "duthub-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn failed_config_save_cleans_up_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(duts_path(dir.path())).unwrap();
        assert!(save_duts_atomic(dir.path(), &default_duts()).is_err());
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["duts.json"]);
    }

    #[test]
    fn settings_export_omits_bearer_credentials_and_remains_importable() {
        use crate::agent_access::{AgentCredential, AgentListener};

        let dir = tempfile::tempdir().unwrap();
        let mut settings = default_settings();
        settings.agent_access.enabled = true;
        settings.agent_access.listeners.push(AgentListener {
            id: "lab".into(),
            port: 8798,
            dut_id: Some("dut-a".into()),
        });
        let token = "fixture-export-bearer";
        settings.agent_access.agents.push(AgentCredential {
            id: "fixture".into(),
            listener_id: Some("lab".into()),
            agent_name: "Fixture".into(),
            token: token.into(),
            enabled: true,
            grants: Vec::new(),
        });
        save_settings_atomic(dir.path(), &settings).unwrap();
        export_settings_atomic(dir.path(), &settings).unwrap();
        let raw = std::fs::read_to_string(settings_path(dir.path())).unwrap();
        let exported: SettingsFile = serde_json::from_str(&raw).unwrap();
        assert!(!raw.contains(token));
        assert_eq!(
            exported.agent_access.agents,
            Vec::<crate::agent_access::AgentCredential>::new()
        );
        assert!(exported.agent_access.enabled);
        assert_eq!(
            exported.agent_access.listeners,
            settings.agent_access.listeners
        );
        assert_eq!(settings.agent_access.agents[0].token, token);
    }

    #[test]
    fn initializes_default_config_beside_executable() {
        let exe_dir = tempfile::tempdir().unwrap();
        let dir = initialize_config_dir(exe_dir.path()).unwrap();
        assert_eq!(dir, exe_dir.path().join(".duthub"));
        assert!(settings_path(&dir).is_file());
        assert_eq!(load_settings(&dir).unwrap(), default_settings());
        let duts = load_duts(&dir).unwrap().unwrap();
        assert_eq!(duts.duts, Vec::<crate::model::DutConfig>::new());
        let [relay] = duts.relay_boards.as_slice() else {
            panic!("expected one default CH340 board")
        };
        assert_eq!(relay.name, "ch340-relay");
        assert_eq!(relay.driver, "ch340");
        assert_eq!(relay.channel_count, 4);
        assert_eq!(relay.node, "");
        assert!(!default_work_dir_in(exe_dir.path()).exists());
    }

    #[test]
    fn initialization_preserves_existing_settings_and_fills_missing_duts() {
        let exe_dir = tempfile::tempdir().unwrap();
        let dir = config_dir_in(exe_dir.path());
        let mut settings = default_settings();
        settings.appearance.theme = "light".into();
        save_settings_atomic(&dir, &settings).unwrap();
        let settings_file = settings_path(&dir);
        let original = std::fs::read(&settings_file).unwrap();

        initialize_config_dir(exe_dir.path()).unwrap();
        assert_eq!(std::fs::read(&settings_file).unwrap(), original);
        assert_eq!(load_settings(&dir).unwrap(), settings);
        let initial = load_duts(&dir).unwrap().unwrap();
        assert_eq!(initial.duts, Vec::<crate::model::DutConfig>::new());
        assert_eq!(initial.relay_boards.len(), 1);
        save_duts_atomic(&dir, &default_duts()).unwrap();
        initialize_config_dir(exe_dir.path()).unwrap();
        assert_eq!(load_duts(&dir).unwrap(), Some(default_duts()));
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = temp_dir("missing");
        let loaded = load_settings(&dir).unwrap();
        assert_eq!(loaded, default_settings());
        assert_eq!(loaded.tools.adb_executable, "adb");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_load_roundtrip_and_no_tmp_leftover() {
        let dir = temp_dir("roundtrip");
        let mut settings = default_settings();
        settings.tools.adb_executable = "D:/tools/adb.exe".into();
        save_settings_atomic(&dir, &settings).unwrap();
        let loaded = load_settings(&dir).unwrap();
        assert_eq!(loaded, settings);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留 .tmp");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duts_roundtrip_keeps_media_bindings() {
        let dir = temp_dir("duts-media");
        let media_dut = crate::model::DutConfig {
            id: "dut-media".into(),
            name: "媒体样机".into(),
            serial: crate::model::SerialConfig {
                host: "local".into(),
                port: "COM3".into(),
                baud: 115_200,
                format: "8N1".into(),
                reference_log: None,
                reference_similarity: None,
                split_logs: false,
            },
            adb: crate::model::AdbConfig::default(),
            relay: crate::model::RelayConfig::Disabled,
            mcp: crate::model::McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8787/mcp".into(),
            },
            a2a: crate::model::DutA2aConfig::default(),
            flash: crate::model::FlashConfig::default(),
            media: crate::model::DutMediaConfig {
                hdmi: crate::model::CaptureInput {
                    enabled: None,
                    video: "UGREEN 35866".into(),
                    audio: "数字输入 (UGREEN 35866)".into(),
                    video_size: "1920x1080".into(),
                    frame_rate: "60".into(),
                    port: "8554".into(),
                    ..Default::default()
                },
                camera: crate::model::CaptureInput {
                    enabled: None,
                    video: "Integrated Camera".into(),
                    audio: String::new(),
                    video_size: "1280x720".into(),
                    frame_rate: "30".into(),
                    port: "0".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            heartbeat: crate::model::HeartbeatConfig::default(),
        };
        let plain_dut = crate::model::DutConfig {
            id: "dut-plain".into(),
            name: "普通样机".into(),
            serial: crate::model::SerialConfig {
                host: "local".into(),
                port: "COM4".into(),
                baud: 115_200,
                format: "8N1".into(),
                reference_log: None,
                reference_similarity: None,
                split_logs: false,
            },
            adb: crate::model::AdbConfig::default(),
            relay: crate::model::RelayConfig::Disabled,
            mcp: crate::model::McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8787/mcp".into(),
            },
            a2a: crate::model::DutA2aConfig::default(),
            flash: crate::model::FlashConfig::default(),
            media: crate::model::DutMediaConfig::default(),
            heartbeat: crate::model::HeartbeatConfig::default(),
        };
        let duts = crate::model::DutsFile {
            version: 1,
            hosts: vec![],
            relay_boards: vec![],
            alerts_by_dut: std::collections::BTreeMap::new(),
            garbled_by_dut: std::collections::BTreeMap::new(),
            duts: vec![media_dut.clone(), plain_dut],
        };
        save_duts_atomic(&dir, &duts).unwrap();
        // 落盘格式：media 字段为 camelCase；未配置 media 的 DUT 整段省略该字段。
        let raw = std::fs::read_to_string(dir.join("duts.json")).unwrap();
        assert!(raw.contains("\"videoSize\": \"1920x1080\""), "{raw}");
        assert!(raw.contains("\"frameRate\": \"60\""), "{raw}");
        assert_eq!(
            raw.matches("\"media\"").count(),
            1,
            "未配置 media 的 DUT 不应序列化 media 字段: {raw}"
        );
        let loaded = load_duts(&dir).unwrap().expect("duts.json 应存在");
        assert_eq!(loaded.duts.len(), 2);
        assert_eq!(loaded.duts[0], media_dut, "media 绑定必须原样往返");
        assert_eq!(loaded.duts[1].media, crate::model::DutMediaConfig::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_is_backed_up_and_default_returned() {
        let dir = temp_dir("corrupt");
        std::fs::write(settings_path(&dir), "{not valid json").unwrap();
        let result = load_settings(&dir);
        assert!(result.is_err());
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().to_string_lossy().contains("corrupt"))
            .collect();
        assert_eq!(backups.len(), 1, "损坏文件必须保留备份");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preset_ids_normalized_stably_and_uniquely() {
        let dir = temp_dir("preset-id");
        let raw = serde_json::json!({
            "version": 1,
            "appearance": { "theme": "dark", "language": "zh-CN", "platform": "windows" },
            "flashTools": [
                { "name": "upgrade_tool", "executable": "upgrade_tool", "testArgs": "--help" },
                { "name": "upgrade_tool", "executable": "C:/tools/ut2/upgrade_tool.exe", "testArgs": "--version" },
                { "name": "瑞芯微 工具", "executable": "rkdevtool" }
            ]
        });
        std::fs::write(settings_path(&dir), serde_json::to_string(&raw).unwrap()).unwrap();
        let settings = load_settings(&dir).unwrap();
        let ids: Vec<&str> = settings.flash_tools.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids[0], "preset-upgrade_tool");
        assert_eq!(ids[1], "preset-upgrade_tool-2", "同名预设 id 必须去重");
        // 非 ASCII 名称 slug 化为占位（稳定）；与已有 id 去重
        assert_eq!(ids[2], "preset");
        // 归一化是稳定的：再次加载同文件 id 不变（旧文件无 id → slug 由 name 决定）
        let again = load_settings(&dir).unwrap();
        let ids2: Vec<&str> = again.flash_tools.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ids2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn portable_work_dir_uses_cli_then_settings_then_executable_directory() {
        let base = tempfile::tempdir().unwrap();
        let anchor = base.path();
        for (configured, startup, expected) in [
            (Some("saved"), Some("command-line"), "command-line"),
            (Some("saved"), Some("  "), "saved"),
            (None, None, ""),
        ] {
            let (root, warning) = ensure_portable_work_dir(anchor, configured, startup).unwrap();
            assert_eq!(root, std::fs::canonicalize(anchor.join(expected)).unwrap());
            assert!(warning.is_none());
            assert!(root.join("duthub-data").is_dir());
        }
    }

    #[test]
    fn explicit_workspace_is_checked_before_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path();
        let parse = |args: &[&str]| explicit_workspace(anchor, args.iter().map(Into::into));
        assert_eq!(
            parse(&["--workdir", "work"]).unwrap(),
            Some(anchor.join("work"))
        );
        for args in [
            &["--workdir"][..],
            &["--workdir", "--trace"],
            &["--workdir", " "],
            &["--workdir", "a", "--workdir", "b"],
        ] {
            assert!(parse(args).is_err());
        }
        std::fs::write(anchor.join("file"), "test").unwrap();
        assert!(ensure_portable_work_dir(anchor, None, Some("file")).is_err());
        assert!(!anchor.join("duthub-data").exists());
    }
    #[test]
    fn usb_tool_paths_are_saved_independently() {
        let dir = temp_dir("usb-tool-paths");
        let mut settings = default_settings();
        settings.tools.adb_executable = "C:/Android/platform-tools/adb.exe".into();
        settings.tools.hdc_executable = "D:/Harmony OS/SDK/hdc.exe".into();
        save_settings_atomic(&dir, &settings).unwrap();
        let loaded = load_settings(&dir).unwrap();
        assert_eq!(loaded.tools.adb_executable, settings.tools.adb_executable);
        assert_eq!(loaded.tools.hdc_executable, settings.tools.hdc_executable);
        std::fs::remove_file(settings_path(&dir)).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn workspace_dir_setting_backward_compatible_and_roundtrips() {
        let dir = temp_dir("workspace-dir");
        // 旧 settings.json 无 workspaceDir 字段 → None（向后兼容）
        let legacy = serde_json::json!({
            "version": 1,
            "appearance": { "theme": "dark", "language": "zh-CN", "platform": "windows" }
        });
        std::fs::write(settings_path(&dir), serde_json::to_string(&legacy).unwrap()).unwrap();
        let loaded = load_settings(&dir).unwrap();
        assert_eq!(
            loaded.workspace_dir, None,
            "旧文件缺字段必须反序列化为 None"
        );

        // 配置 workspaceDir → 往返保留
        let mut settings = default_settings();
        settings.workspace_dir = Some("D:/work/logs-root".into());
        save_settings_atomic(&dir, &settings).unwrap();
        let raw = std::fs::read_to_string(settings_path(&dir)).unwrap();
        assert!(
            raw.contains("\"workspaceDir\": \"D:/work/logs-root\""),
            "{raw}"
        );
        let reloaded = load_settings(&dir).unwrap();
        assert_eq!(reloaded.workspace_dir.as_deref(), Some("D:/work/logs-root"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------- Agent 路径 / 哈希助手 -------------------------

    #[test]
    fn agent_dut_key_rejects_blank_and_is_path_safe() {
        assert_eq!(agent_dut_key(""), None);
        assert_eq!(agent_dut_key("   \t\r\n"), None);
        // 稳定且与原始 id 一一对应；非 ASCII / 路径字符一律编码
        let key = agent_dut_key("dut-a").unwrap();
        assert_eq!(key, agent_dut_key("dut-a").unwrap());
        assert_ne!(key, agent_dut_key("dut-b").unwrap());
        for raw in ["../x", "C:/Windows", "CON", "样机 A", "dut:a"] {
            let key = agent_dut_key(raw).unwrap();
            assert!(
                key.bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "{raw} → {key}"
            );
        }
        // 首尾空白被裁剪，键与 trim 后的 id 一致
        assert_eq!(agent_dut_key("  dut-a  "), agent_dut_key("dut-a"));
    }

    #[test]
    fn agent_dut_dir_is_created_beneath_work_dir_and_rejects_unsafe_names() {
        let root = temp_dir("agent-dut-dir");
        let dir = agent_dut_dir(&root, "dut-a").unwrap();
        assert!(dir.is_dir(), "目录应已创建: {}", dir.display());
        assert!(dir.starts_with(&root));
        assert_eq!(dir, agent_dut_dir(&root, "dut-a").unwrap(), "重复调用幂等");
        assert!(agent_dut_dir(&root, "").is_err(), "空 DUT id 必须拒绝");
        assert!(agent_dut_dir(&root, "  ").is_err());
        // 路径段不能穿越（name_component 校验）
        assert!(ensure_agent_dir(&root, &[".."]).is_err());
        assert!(ensure_agent_dir(&root, &["C:", "x"]).is_err());
        // 已存在的普通文件不能当目录复用
        let blocked = root.join("blocked");
        std::fs::write(&blocked, b"not a dir").unwrap();
        assert!(ensure_agent_dir(&root, &["blocked"]).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn plain_path_strips_windows_verbatim_prefixes() {
        assert_eq!(
            plain_path(Path::new(r"\\?\C:\work\logs")),
            PathBuf::from(r"C:\work\logs")
        );
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\server\share\logs")),
            PathBuf::from(r"\\server\share\logs")
        );
        assert_eq!(
            plain_path(Path::new(r"C:\work\logs")),
            PathBuf::from(r"C:\work\logs")
        );
        assert_eq!(
            plain_path(Path::new("relative/path")),
            PathBuf::from("relative/path")
        );
    }

    #[test]
    fn sha256_helpers_match_known_vector_and_file_bytes() {
        // FIPS 180-2 已知向量：sha256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let dir = temp_dir("sha256-file");
        let path = dir.join("payload.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(b"abc"));
        assert!(sha256_file(&dir.join("missing.bin")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn executable_digest_reflects_current_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("agent/bin/goose/1.60.2")
            .join(sha256_hex(b"version-1"))
            .join("goose-acp.exe");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"version-1").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(b"version-1"));
        std::fs::write(&path, b"version-2").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(b"version-2"));
    }

    #[test]
    fn load_settings_seeds_default_highlight_rules_only_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        // 旧 settings.json 无 serial 段 → 播种内置默认规则（sermcp 语义）
        std::fs::write(
            settings_path(dir.path()),
            r#"{"version":1,"appearance":{"theme":"dark","language":"zh-CN","platform":"windows"}}"#,
        )
        .unwrap();
        let loaded = load_settings(dir.path()).unwrap();
        assert_eq!(
            loaded.serial.highlight_rules,
            crate::model::default_highlight_rules()
        );

        // 已有规则 → 原样保留，绝不覆盖用户手写规则
        let mut settings = loaded;
        settings.serial.highlight_rules = vec![crate::model::PromptRule {
            regex: "panic".into(),
            description: Some("Kernel panic".into()),
            color: "#ff0000".into(),
            enabled: true,
        }];
        save_settings_atomic(dir.path(), &settings).unwrap();
        let reloaded = load_settings(dir.path()).unwrap();
        assert_eq!(reloaded.serial.highlight_rules.len(), 1);
        assert_eq!(reloaded.serial.highlight_rules[0].regex, "panic");
        assert_eq!(
            reloaded.serial.highlight_rules[0].description.as_deref(),
            Some("Kernel panic")
        );
    }

    #[test]
    fn load_settings_updates_android_pattern_and_preserves_rule_preferences() {
        use crate::model::PromptRule;

        let dir = tempfile::tempdir().unwrap();
        let mut settings = default_settings();
        settings.serial.highlight_rules = vec![PromptRule {
            regex: PromptRule::OLD_ANDROID_HOST_PATTERN.into(),
            description: Some("My Android prompt".into()),
            color: "#123456".into(),
            enabled: false,
        }];
        save_settings_atomic(dir.path(), &settings).unwrap();
        settings.serial.highlight_rules[0].regex = PromptRule::ANDROID_HOST_PATTERN.into();

        let loaded = load_settings(dir.path()).unwrap();
        assert_eq!(
            loaded.serial.highlight_rules,
            settings.serial.highlight_rules
        );
    }
}
