//! settings.json 原子读写（`docs/SECURITY.md` #9：临时文件 + fsync + rename）。
//!
//! 配置目录（settings.json / duts.json）固定在可执行文件同级的 `.duthub/`。
//! 工作目录独立解析，承载日志、采集和运行时快照。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem::DropGuard;
use std::path::{Path, PathBuf};

use crate::error::{DeviceError, DeviceResult};
use crate::model::{
    Appearance, DutsFile, FlashToolPreset, SettingsFile, TerminalSettings, ToolsSettings,
};

const CONFIG_DIR: &str = ".duthub";

/// 可执行文件所在目录（路径解析锚点）。
pub fn exe_dir() -> DeviceResult<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| DeviceError::Invalid(e.to_string()))?;
    exe.parent()
        .ok_or(DeviceError::NotFound)
        .map(Path::to_path_buf)
}

/// 配置目录（bootstrap）：settings.json / duts.json 固定 exe 同级 `.duthub/`。
pub fn data_dir() -> DeviceResult<PathBuf> {
    Ok(config_dir_in(&exe_dir()?))
}

pub fn config_dir_in(exe_dir: &Path) -> PathBuf {
    exe_dir.join(CONFIG_DIR)
}

/// 创建配置目录并补齐缺失的默认文件，保留已存在的配置。
pub fn initialize_config_dir(exe_dir: &Path) -> DeviceResult<PathBuf> {
    ensure_plain_child_dir(exe_dir, CONFIG_DIR)?;
    let dir = config_dir_in(exe_dir);
    create_default_config(&dir, &settings_path(&dir), &default_settings())?;
    create_default_config(&dir, &duts_path(&dir), &default_duts())?;
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
pub fn default_work_dir_in(exe_dir: &Path) -> PathBuf {
    exe_dir.join("duthub-data")
}

/// Resolve the desktop's trusted artifact root. Settings and startup options
/// are local operator inputs, separate from the read-only MCP interface.
pub fn ensure_portable_work_dir(
    exe_dir: &Path,
    settings_dir: Option<&str>,
    cli_dir: Option<&str>,
) -> DeviceResult<(PathBuf, Option<String>)> {
    let selected = [cli_dir, settings_dir]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|path| !path.is_empty())
        .map(|path| exe_dir.join(path))
        .unwrap_or_else(|| exe_dir.to_path_buf());
    if dir_writable(&selected) {
        let root = std::fs::canonicalize(&selected)
            .map_err(|error| DeviceError::Invalid(error.to_string()))?;
        ensure_portable_child(&root, "duthub-data")?;
        return Ok((root, None));
    }
    ensure_portable_child(exe_dir, "duthub-data")?;
    Ok((
        exe_dir.to_path_buf(),
        Some(format!(
            "工作目录不可用，已使用程序同级目录: {}",
            selected.display()
        )),
    ))
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

/// 工作目录解析（纯函数，不触盘）。优先级：`settings_dir`（settings.json
/// `workspaceDir`）高于 `cli_dir`（CLI `--workdir`），两者都未配置时取默认
/// exe 同级 `duthub-data/`。支持绝对路径；相对路径按 `exe_dir` 解析；
/// 空白视为未配置。
pub fn resolve_work_dir_in(
    exe_dir: &Path,
    settings_dir: Option<&str>,
    cli_dir: Option<&str>,
) -> PathBuf {
    let configured = settings_dir
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| cli_dir.map(str::trim).filter(|s| !s.is_empty()));
    match configured {
        None => default_work_dir_in(exe_dir),
        Some(raw) => {
            let path = Path::new(raw);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                exe_dir.join(path)
            }
        }
    }
}

/// 解析并准备工作目录：`create_dir_all` + 可写探针；所选目录不可用（创建失败或
/// 不可写）时回退默认 exe 同级目录并返回原因文案（供启动 toast）。
/// 返回 `(生效目录, 回退原因)`。
pub fn ensure_work_dir_in(
    exe_dir: &Path,
    settings_dir: Option<&str>,
    cli_dir: Option<&str>,
) -> (PathBuf, Option<String>) {
    let default = default_work_dir_in(exe_dir);
    let resolved = resolve_work_dir_in(exe_dir, settings_dir, cli_dir);
    if resolved == default || dir_writable(&resolved) {
        let _ = std::fs::create_dir_all(&default);
        return (resolved, None);
    }
    let _ = std::fs::create_dir_all(&default);
    let reason = format!(
        "工作目录不可用（无法创建或不可写）：{}，已回退默认 {}",
        resolved.display(),
        default.display()
    );
    (default, Some(reason))
}

/// `ensure_work_dir_in` 的 exe 锚点封装；exe 路径不可得时锚点退化为当前目录。
pub fn ensure_work_dir(
    settings_dir: Option<&str>,
    cli_dir: Option<&str>,
) -> (PathBuf, Option<String>) {
    let anchor = exe_dir().unwrap_or_else(|_| PathBuf::from("."));
    ensure_work_dir_in(&anchor, settings_dir, cli_dir)
}

fn settings_path(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

/// 出厂默认 settings（theme dark / language zh-CN / platform windows）。
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
        flash_tools: Vec::new(),
        agent: Default::default(),
        tools: ToolsSettings {
            adb_executable: "adb".into(),
            hdc_executable: "hdc".into(),
            capture: Default::default(),
        },
        a2a: None,
        workspace_dir: None,
        dut_storage_gib: None,
        alerts: Default::default(),
        agent_access: Default::default(),
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
            Ok(settings)
        }
        Err(err) => {
            let backup = path.with_extension(format!(
                "corrupt-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ));
            let _ = std::fs::rename(&path, &backup);
            Err(DeviceError::Invalid(format!(
                "settings.json 损坏（已备份为 {}）：{err}",
                backup.display()
            )))
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

/// 原子保存：同目录临时文件 + fsync + rename。
pub fn save_settings_atomic(dir: &Path, settings: &SettingsFile) -> DeviceResult<()> {
    save_config_atomic(dir, &settings_path(dir), "settings", settings)
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
            .map(|d| d.as_nanos())
            .unwrap_or(0)
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
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
    };
    written.map_err(|e| DeviceError::Invalid(e.to_string()))?;
    std::fs::rename(pending.as_path(), path)
        .map_err(|e| DeviceError::Invalid(format!("{name} 原子重命名失败: {e}")))?;
    DropGuard::dismiss(pending);
    Ok(())
}

/// 便捷封装：把 adb 路径与首个烧录预设写入 settings。
pub fn persist_tool_settings(
    dir: &Path,
    adb_executable: &str,
    flash: Option<(&str, &str)>,
) -> DeviceResult<()> {
    let mut settings = load_settings(dir)?;
    settings.tools.adb_executable = adb_executable.trim().to_string();
    if let Some((exe, args)) = flash {
        let name = exe.trim().to_string();
        settings.flash_tools = vec![FlashToolPreset {
            id: String::new(), // 保存前归一化为稳定 id
            name: name.clone(),
            executable: exe.trim().to_string(),
            test_args: args.trim().to_string(),
        }];
        normalize_preset_ids(&mut settings.flash_tools);
    }
    save_settings_atomic(dir, &settings)
}

/// 只读探测：目录是否可写（供 UI 提示），探测文件即建即删。
pub fn dir_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    let ok = File::create(&probe)
        .map(|f| f.sync_all().is_ok())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&probe);
    ok
}

fn duts_path(dir: &Path) -> PathBuf {
    dir.join("duts.json")
}

/// 新配置从空的 DUT 列表开始。
pub fn default_duts() -> DutsFile {
    DutsFile {
        version: 1,
        hosts: Vec::new(),
        relay_boards: Vec::new(),
        alerts_by_dut: Default::default(),
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
            let backup = path.with_extension(format!(
                "corrupt-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ));
            let _ = std::fs::rename(&path, &backup);
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
    fn initializes_default_config_beside_executable() {
        let exe_dir = tempfile::tempdir().unwrap();
        let dir = initialize_config_dir(exe_dir.path()).unwrap();
        assert_eq!(dir, exe_dir.path().join(".duthub"));
        assert!(settings_path(&dir).is_file());
        assert_eq!(load_settings(&dir).unwrap(), default_settings());
        assert_eq!(load_duts(&dir).unwrap(), Some(default_duts()));
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
            .filter_map(|e| e.ok())
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
            },
            adb: Default::default(),
            relay: crate::model::RelayConfig::Disabled,
            mcp: crate::model::McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8787/mcp".into(),
            },
            flash: Default::default(),
            media: crate::model::DutMediaConfig {
                hdmi: crate::model::CaptureInput {
                    enabled: None,
                    video: "UGREEN 35866".into(),
                    audio: "数字输入 (UGREEN 35866)".into(),
                    video_size: "1920x1080".into(),
                    frame_rate: "60".into(),
                    port: "8554".into(),
                },
                camera: crate::model::CaptureInput {
                    enabled: None,
                    video: "Integrated Camera".into(),
                    audio: String::new(),
                    video_size: "1280x720".into(),
                    frame_rate: "30".into(),
                    port: "0".into(),
                },
            },
            heartbeat: Default::default(),
        };
        let plain_dut = crate::model::DutConfig {
            id: "dut-plain".into(),
            name: "普通样机".into(),
            serial: crate::model::SerialConfig {
                host: "local".into(),
                port: "COM4".into(),
                baud: 115_200,
                format: "8N1".into(),
            },
            adb: Default::default(),
            relay: crate::model::RelayConfig::Disabled,
            mcp: crate::model::McpConfig::StreamableHttp {
                url: "http://127.0.0.1:8787/mcp".into(),
            },
            flash: Default::default(),
            media: Default::default(),
            heartbeat: Default::default(),
        };
        let duts = crate::model::DutsFile {
            version: 1,
            hosts: vec![],
            relay_boards: vec![],
            alerts_by_dut: Default::default(),
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
        assert_eq!(loaded.duts[1].media, Default::default());
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
            .filter_map(|e| e.ok())
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

    // ------------------------- 工作目录解析 -------------------------

    /// Windows 绝对路径样本（测试平台为 win32；`is_absolute` 判定成立）。
    const ABS_SETTINGS: &str = "C:/work/wd-settings";
    const ABS_CLI: &str = "C:/work/wd-cli";

    #[test]
    fn work_dir_priority_settings_over_cli_over_default() {
        let exe = Path::new("C:/app/bin");
        let default = exe.join("duthub-data");
        // settings.json 配置 > CLI --workdir
        assert_eq!(
            resolve_work_dir_in(exe, Some(ABS_SETTINGS), Some(ABS_CLI)),
            PathBuf::from(ABS_SETTINGS)
        );
        // CLI > 默认（settings 未配置）
        assert_eq!(
            resolve_work_dir_in(exe, None, Some(ABS_CLI)),
            PathBuf::from(ABS_CLI)
        );
        // 空白/空串视为未配置（settings 空白 → 落到 CLI）
        assert_eq!(
            resolve_work_dir_in(exe, Some("   "), Some(ABS_CLI)),
            PathBuf::from(ABS_CLI)
        );
        // 都未配置 → 默认 exe 同级 duthub-data
        assert_eq!(resolve_work_dir_in(exe, None, None), default);
        assert_eq!(resolve_work_dir_in(exe, Some(""), Some("  ")), default);
    }

    #[test]
    fn work_dir_relative_anchored_at_exe_dir_absolute_passthrough() {
        let exe = Path::new("C:/app/bin");
        // 相对路径按 exe 所在目录解析
        assert_eq!(resolve_work_dir_in(exe, Some("wd"), None), exe.join("wd"));
        assert_eq!(
            resolve_work_dir_in(exe, None, Some("wd/sub")),
            exe.join("wd").join("sub")
        );
        // 首尾空白在解析前裁剪
        assert_eq!(
            resolve_work_dir_in(exe, Some("  wd  "), None),
            exe.join("wd")
        );
        // 绝对路径原样使用（含反斜杠 Windows 形态）
        assert_eq!(
            resolve_work_dir_in(exe, Some("D:\\abs\\wd"), None),
            PathBuf::from("D:\\abs\\wd")
        );
        // settings 里的相对路径优先于 CLI 绝对路径（优先级与形态正交）
        assert_eq!(
            resolve_work_dir_in(exe, Some("wd"), Some(ABS_CLI)),
            exe.join("wd")
        );
    }

    #[test]
    fn ensure_work_dir_creates_dir_and_reports_no_fallback() {
        let root = temp_dir("ensure-ok");
        let exe = root.join("bin");
        std::fs::create_dir_all(&exe).unwrap();
        let (work_dir, fallback) = ensure_work_dir_in(&exe, Some("wd/created"), None);
        assert_eq!(work_dir, exe.join("wd").join("created"));
        assert!(work_dir.is_dir(), "目录应已创建");
        assert_eq!(fallback, None, "可用目录不应回退");
        std::fs::remove_dir_all(&root).ok();
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
    fn ensure_work_dir_falls_back_to_default_when_unusable() {
        let root = temp_dir("ensure-fallback");
        let exe = root.join("bin");
        std::fs::create_dir_all(&exe).unwrap();
        // 占位文件阻断目录创建：create_dir_all 必然失败 → 回退默认
        let blocked = root.join("blocked");
        std::fs::write(&blocked, b"not a dir").unwrap();
        let (work_dir, fallback) =
            ensure_work_dir_in(&exe, Some(&blocked.display().to_string()), None);
        assert_eq!(work_dir, exe.join("duthub-data"), "必须回退默认目录");
        assert!(work_dir.is_dir(), "默认目录应已创建");
        let message = fallback.expect("回退必须携带原因");
        assert!(
            message.contains(&blocked.display().to_string()),
            "{message}"
        );
        assert!(message.contains("duthub-data"), "{message}");
        std::fs::remove_dir_all(&root).ok();
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
}
