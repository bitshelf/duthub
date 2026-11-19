//! 会话日志管道（M1.5，`docs/SECURITY.md` #8）。
//!
//! - 文本流：utf8-lossy 行，append-only，按大小轮换（`<session>.log` → `.1.log` → …）。
//! - 原始字节流：`<session>.bin` 追加，**永不**按行重组/转码，保证 panic/oops 证据不被
//!   ANSI/编码处理污染。二进制流不轮换内联截断，超过 `raw_cap` 起新序号文件。
//! - 写入目录由调用方给定（产品用解析后工作目录 `logs/`，见
//!   `duthub_core::store::resolve_work_dir_in`）。

use std::fs::OpenOptions;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use duthub_core::{DeviceError, DeviceResult};

const TEXT_ROTATE_BYTES: u64 = 2 * 1024 * 1024;
const RAW_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ROTATIONS: u32 = 20;
type LogFile = BufWriter<Box<dyn Write + Send>>;

pub struct SessionLogWriter {
    dir: PathBuf,
    session_id: String,
    text: Option<LogFile>,
    raw: Option<LogFile>,
    text_bytes: u64,
    raw_bytes: u64,
    text_rotation: u32,
    raw_rotation: u32,
    text_rotate_bytes: u64,
    raw_rotate_bytes: u64,
    saving: Option<Arc<AtomicBool>>,
    portable: Option<PortableArchive>,
}

struct PortableArchive {
    root: PathBuf,
    dut_id: String,
    name: String,
    quota: Arc<duthub_core::dut_quota::DutQuota>,
}

impl PortableArchive {
    fn next_segment(&self) -> std::io::Result<(LogFile, LogFile)> {
        use duthub_core::dut_artifacts::{
            OpenOptions, log_sequence, open_dut_for_write, open_logs_for_write,
        };

        let (name, owner) = open_dut_for_write(&self.root, &self.dut_id, &self.name)?;
        let logs = open_logs_for_write(&owner)?;
        let mut index = logs
            .open_with(
                ".log-sequence",
                OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true),
            )?
            .into_std();
        index.lock()?;
        let mut last = [0; 8];
        let mut sequence = match index.read_exact(&mut last) {
            Ok(()) => u64::from_le_bytes(last),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => logs
                .entries()?
                .flatten()
                .filter_map(|entry| entry.file_name().to_str().and_then(log_sequence))
                .max()
                .unwrap_or(0),
            Err(error) => return Err(error),
        };
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        loop {
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("日志序号用尽"))?;
            let stem = format!("logs/{name}-{sequence:03}_{stamp}");
            let text_path = format!("{stem}.txt");
            let raw_path = format!("{stem}.bin");
            let text = match self.quota.create(&self.dut_id, &owner, &text_path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let raw = match self.quota.create(&self.dut_id, &owner, &raw_path) {
                Ok(raw) => raw,
                Err(error) => {
                    drop(text);
                    let _ = owner.remove_file(&text_path);
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        continue;
                    }
                    return Err(error);
                }
            };
            index.seek(SeekFrom::Start(0))?;
            index.write_all(&sequence.to_le_bytes())?;
            return Ok((
                BufWriter::new(Box::new(text)),
                BufWriter::new(Box::new(raw)),
            ));
        }
    }
}

impl SessionLogWriter {
    pub fn new(dir: &Path, session_id: &str) -> DeviceResult<Self> {
        Self::with_limits(dir, session_id, TEXT_ROTATE_BYTES, RAW_ROTATE_BYTES)
    }

    /// 自定义轮换阈值（测试用小阈值快速验证轮换路径）。
    pub fn with_limits(
        dir: &Path,
        session_id: &str,
        text_rotate_bytes: u64,
        raw_rotate_bytes: u64,
    ) -> DeviceResult<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| DeviceError::Invalid(format!("创建日志目录失败: {e}")))?;
        let mut writer = Self {
            dir: dir.to_path_buf(),
            session_id: session_id.to_string(),
            text: None,
            raw: None,
            text_bytes: 0,
            raw_bytes: 0,
            text_rotation: 0,
            raw_rotation: 0,
            text_rotate_bytes,
            raw_rotate_bytes,
            saving: None,
            portable: None,
        };
        writer.text = Some(writer.open_file(false)?);
        writer.raw = Some(writer.open_file(true)?);
        Ok(writer)
    }

    pub fn archive(root: &Path, dut_id: &str, name: &str, saving: Arc<AtomicBool>) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let prefix = duthub_core::log_archive::archive_prefix(dut_id);
        let process = std::process::id();
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self {
            dir: root.join(duthub_core::log_archive::dut_folder(name)),
            session_id: format!("{prefix}{started:020}-{process:010}-{sequence:010}"),
            text: None,
            raw: None,
            text_bytes: 0,
            raw_bytes: 0,
            text_rotation: 0,
            raw_rotation: 0,
            text_rotate_bytes: u64::MAX,
            raw_rotate_bytes: u64::MAX,
            saving: Some(saving),
            portable: None,
        }
    }

    pub fn portable_archive(
        root: &Path,
        dut_id: &str,
        name: &str,
        saving: Arc<AtomicBool>,
        quota: Arc<duthub_core::dut_quota::DutQuota>,
    ) -> Self {
        let mut writer = Self::archive(root, dut_id, name, saving);
        writer.portable = Some(PortableArchive {
            root: root.to_path_buf(),
            dut_id: dut_id.to_owned(),
            name: name.to_owned(),
            quota,
        });
        writer
    }

    fn ensure_writing(&mut self) -> DeviceResult<bool> {
        if let Some(saving) = &self.saving {
            if !saving.load(Ordering::Acquire) {
                if self.text.is_some() {
                    self.finish()?;
                    self.text_rotation += 1;
                    self.raw_rotation += 1;
                    self.text_bytes = 0;
                    self.raw_bytes = 0;
                }
                return Ok(false);
            }
            if self.text.is_none() {
                let (text, raw) = if let Some(portable) = &self.portable {
                    portable
                        .next_segment()
                        .map_err(|error| DeviceError::Invalid(error.to_string()))?
                } else {
                    std::fs::create_dir_all(&self.dir)
                        .map_err(|e| DeviceError::Invalid(format!("创建日志目录失败: {e}")))?;
                    (self.open_file(false)?, self.open_file(true)?)
                };
                self.text = Some(text);
                self.raw = Some(raw);
            }
        }
        Ok(true)
    }

    fn stem(&self, raw: bool) -> String {
        if raw {
            format!("{}.bin", self.session_id)
        } else {
            format!("{}.log", self.session_id)
        }
    }

    fn rotated_name(&self, raw: bool, index: u32) -> String {
        if raw {
            format!("{}.bin.{index}", self.session_id)
        } else {
            format!("{}.log.{index}", self.session_id)
        }
    }

    fn open_file(&self, raw: bool) -> DeviceResult<LogFile> {
        let rotation = if raw {
            self.raw_rotation
        } else {
            self.text_rotation
        };
        self.open_at(raw, rotation)
    }

    fn open_at(&self, raw: bool, rotation: u32) -> DeviceResult<LogFile> {
        let name = if self.saving.is_some() {
            let extension = if raw { "bin" } else { "log" };
            format!("{}-{rotation:010}.{extension}", self.session_id)
        } else if rotation == 0 {
            // 首段使用主名（.log / .bin）
            self.stem(raw)
        } else {
            self.rotated_name(raw, rotation)
        };
        let path = self.dir.join(name);
        // 首段（index 0）覆盖旧残留；轮换段追加
        let file = if self.saving.is_some() {
            OpenOptions::new().create(true).append(true).open(&path)
        } else if rotation == 0 {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
        } else {
            OpenOptions::new().create(true).append(true).open(&path)
        }
        .map_err(|e| DeviceError::Invalid(format!("打开日志失败 {}: {e}", path.display())))?;
        Ok(BufWriter::new(Box::new(file)))
    }

    /// 开始新的文本与原始日志段。两个新句柄均打开后再切换写入目标。
    pub fn split(&mut self) -> DeviceResult<()> {
        if self.saving.is_some() {
            if self.text.is_some() {
                self.finish()?;
                self.text_rotation += 1;
                self.raw_rotation += 1;
                self.text_bytes = 0;
                self.raw_bytes = 0;
            }
            return Ok(());
        }
        if self.text_rotation >= MAX_ROTATIONS || self.raw_rotation >= MAX_ROTATIONS {
            return Err(DeviceError::Invalid("会话日志分段数已达上限".into()));
        }
        let text = self.open_at(false, self.text_rotation + 1)?;
        let raw = self.open_at(true, self.raw_rotation + 1)?;
        self.text = Some(text);
        self.raw = Some(raw);
        self.text_rotation += 1;
        self.raw_rotation += 1;
        self.text_bytes = 0;
        self.raw_bytes = 0;
        Ok(())
    }

    fn rotate(&mut self, raw: bool) -> DeviceResult<()> {
        if raw {
            if self.raw_rotation >= MAX_ROTATIONS {
                return Err(DeviceError::Invalid("原始日志轮换超限".into()));
            }
            self.raw_rotation += 1;
            self.raw = Some(self.open_file(true)?);
            self.raw_bytes = 0;
        } else {
            if self.text_rotation >= MAX_ROTATIONS {
                return Err(DeviceError::Invalid("文本日志轮换超限".into()));
            }
            self.text_rotation += 1;
            self.text = Some(self.open_file(false)?);
            self.text_bytes = 0;
        }
        Ok(())
    }

    /// 追加一行文本与对应的原始字节（原始流先写，防止文本处理崩溃丢证据）。
    pub fn append(&mut self, raw_bytes: &[u8], text_line: &str) -> DeviceResult<()> {
        self.ensure_segment_space(raw_bytes.len() + text_line.len() + 1)?;
        if !self.ensure_writing()? {
            return Ok(());
        }
        {
            let raw = self.raw.as_mut().expect("raw 日志已初始化");
            raw.write_all(raw_bytes)
                .and_then(|_| raw.flush())
                .map_err(|e| DeviceError::Invalid(format!("写原始日志失败: {e}")))?;
            self.raw_bytes += raw_bytes.len() as u64;
            if self.raw_bytes >= self.raw_rotate_bytes {
                self.rotate(true)?;
            }
        }
        self.write_text(text_line)
    }

    pub fn append_text(&mut self, text_line: &str) -> DeviceResult<()> {
        self.ensure_segment_space(text_line.len() + 1)?;
        if !self.ensure_writing()? {
            return Ok(());
        }
        self.write_text(text_line)
    }

    fn ensure_segment_space(&mut self, pending: usize) -> DeviceResult<()> {
        if let Some(portable) = &self.portable {
            let limit = (portable.quota.limit() / 8).min(16 * 1024 * 1024);
            if self.text_bytes + self.raw_bytes > 0
                && self.text_bytes + self.raw_bytes + pending as u64 > limit
            {
                self.split()?;
            }
        }
        Ok(())
    }

    pub fn note(&mut self, line: &str) -> DeviceResult<()> {
        if self.saving.is_some() {
            self.append_text(line)
        } else {
            self.append(line.as_bytes(), line)
        }
    }

    fn write_text(&mut self, text_line: &str) -> DeviceResult<()> {
        let text = self.text.as_mut().expect("text 日志已初始化");
        text.write_all(text_line.as_bytes())
            .and_then(|_| text.write_all(b"\n"))
            .and_then(|_| text.flush())
            .map_err(|e| DeviceError::Invalid(format!("写文本日志失败: {e}")))?;
        self.text_bytes += text_line.len() as u64 + 1;
        if self.text_bytes >= self.text_rotate_bytes {
            self.rotate(false)?;
        }
        Ok(())
    }

    pub fn mark(&mut self, line: &str) -> DeviceResult<()> {
        if self.saving.is_some() {
            self.split()?;
            self.append_text(line)
        } else {
            self.append(line.as_bytes(), line)?;
            self.split()
        }
    }

    /// 仅追加原始字节流（键入字节审计：.bin 记真字节，.log 文本流不动）。
    pub fn append_raw(&mut self, raw_bytes: &[u8]) -> DeviceResult<()> {
        if !self.ensure_writing()? {
            return Ok(());
        }
        let raw = self.raw.as_mut().expect("raw 日志已初始化");
        raw.write_all(raw_bytes)
            .and_then(|_| raw.flush())
            .map_err(|e| DeviceError::Invalid(format!("写原始日志失败: {e}")))?;
        self.raw_bytes += raw_bytes.len() as u64;
        if self.raw_bytes >= self.raw_rotate_bytes {
            self.rotate(true)?;
        }
        Ok(())
    }

    /// 会话关闭：flush 并关闭两个流。
    pub fn finish(&mut self) -> DeviceResult<()> {
        if let Some(mut text) = self.text.take() {
            text.flush()
                .map_err(|e| DeviceError::Invalid(e.to_string()))?;
        }
        if let Some(mut raw) = self.raw.take() {
            raw.flush()
                .map_err(|e| DeviceError::Invalid(e.to_string()))?;
        }
        Ok(())
    }
}

/// 列出某会话已产生的日志段（测试与 UI 检查用）。
pub fn list_segments(dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&format!("{session_id}.")) {
            found.push(entry.path());
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试临时目录放在项目 target/ 内（遵守“只写项目目录”约束）。
    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/logs")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn portable_log_numbers_continue_across_splits_and_reopen() {
        let root = test_dir("portable-log-sequence");
        std::fs::create_dir_all(&root).unwrap();
        let enabled = Arc::new(AtomicBool::new(true));
        let quota = duthub_core::dut_quota::DutQuota::new(1024 * 1024);
        for value in 1..=3 {
            let mut log = SessionLogWriter::portable_archive(
                &root,
                "a",
                "RK3576",
                enabled.clone(),
                quota.clone(),
            );
            log.append_text(&format!("part {value}")).unwrap();
            log.split().unwrap();
        }
        let logs = duthub_core::log_archive::list(&root.join("duthub-data"), "a", 0, 20);
        assert_eq!(logs.len(), 3);
        for (segment, number) in logs.iter().zip([3, 2, 1]) {
            assert!(
                segment
                    .id
                    .starts_with(&format!("dut/RK3576/logs/RK3576-{number:03}_"))
            );
            assert!(segment.id.ends_with(".txt"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dut_archive_is_live_split_and_discoverable_after_reopen() {
        let root = test_dir("dut-archive");
        let dir = root.join("logs");
        let enabled = Arc::new(AtomicBool::new(true));
        let mut log = SessionLogWriter::archive(&dir, "board-a", "开发板 A", enabled.clone());
        log.append(b"before\r\n", "before").unwrap();
        let first = duthub_core::log_archive::list(&root, "board-a", 0, 10);
        assert_eq!(first.len(), 1);
        assert!(first[0].id.starts_with("开发板 A/"));
        assert_eq!(
            duthub_core::log_archive::read(&root, "board-a", &first[0].id, 0)
                .unwrap()
                .text,
            "before\n"
        );
        for (label, raw) in [
            ("Power OFF", "shutdown"),
            ("Power ON", "boot"),
            ("Agent", "ready"),
        ] {
            log.mark(&format!("[mark] {label}")).unwrap();
            log.append(raw.as_bytes(), raw).unwrap();
            let segments = duthub_core::log_archive::list(&root, "board-a", 0, 10);
            let current = &segments[0].id;
            let page = duthub_core::log_archive::read(&root, "board-a", current, 0).unwrap();
            assert_eq!(page.text, format!("[mark] {label}\n{raw}\n"));
            assert_eq!(
                std::fs::read(dir.join(current).with_extension("bin")).unwrap(),
                raw.as_bytes()
            );
        }
        assert_eq!(
            duthub_core::log_archive::list(&root, "board-a", 0, 10).len(),
            4
        );

        enabled.store(false, Ordering::Release);
        log.append(b"offline", "offline").unwrap();
        log.mark("disabled").unwrap();
        assert_eq!(
            duthub_core::log_archive::list(&root, "board-a", 0, 10).len(),
            4
        );
        enabled.store(true, Ordering::Release);
        log.append(b"resumed", "resumed").unwrap();
        drop(log);
        let mut renamed = SessionLogWriter::archive(&dir, "board-a", "新名称", enabled.clone());
        renamed.append(b"reopened", "reopened").unwrap();
        let mut other = SessionLogWriter::archive(&dir, "board-b", "新名称", enabled);
        other.append(b"other", "other").unwrap();
        let segments = duthub_core::log_archive::list(&root, "board-a", 0, 10);
        assert_eq!(segments.len(), 6);
        assert!(segments[0].id.starts_with("新名称/"));
        assert!(duthub_core::log_archive::read(&root, "board-b", &segments[0].id, 0).is_err());
        assert!(duthub_core::log_archive::read(&root, "board-a", "../settings.json", 0).is_err());
    }

    #[test]
    fn writes_text_and_raw_pairs() {
        let dir = test_dir("pairs");
        let mut writer = SessionLogWriter::new(&dir, "sess-a").unwrap();
        writer
            .append(b"root@rk3576:~# uname\r\n", "root@rk3576:~# uname")
            .unwrap();
        writer
            .append(b"Linux rk3576 6.1.118\n", "Linux rk3576 6.1.118")
            .unwrap();
        writer.finish().unwrap();
        let text = std::fs::read_to_string(dir.join("sess-a.log")).unwrap();
        assert_eq!(text.lines().count(), 2);
        let raw = std::fs::read(dir.join("sess-a.bin")).unwrap();
        assert_eq!(raw, b"root@rk3576:~# uname\r\nLinux rk3576 6.1.118\n");
    }

    #[test]
    fn explicit_split_keeps_previous_pair_and_writes_to_new_pair() {
        let dir = test_dir("split");
        let mut writer = SessionLogWriter::new(&dir, "sess-split").unwrap();
        writer.append(b"before", "[mark] before reboot").unwrap();
        writer.split().unwrap();
        writer.append(b"after", "boot received").unwrap();
        writer.finish().unwrap();
        assert_eq!(
            std::fs::read(dir.join("sess-split.bin")).unwrap(),
            b"before"
        );
        assert_eq!(
            std::fs::read(dir.join("sess-split.bin.1")).unwrap(),
            b"after"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-split.log.1")).unwrap(),
            "boot received\n"
        );
    }

    #[test]
    fn rotates_when_threshold_reached() {
        let dir = test_dir("rotate");
        // 小阈值触发真实轮换路径：每 64 字节换段
        let mut writer = SessionLogWriter::with_limits(&dir, "sess-b", 64, 64).unwrap();
        let line = "x".repeat(30);
        for i in 0..5 {
            writer
                .append(format!("[{i}]{line}").as_bytes(), &format!("[{i}]{line}"))
                .unwrap();
        }
        writer.finish().unwrap();
        // 5 条 × 31B ≈ 155B ≥ 64×2 → 至少轮换 2 段
        let segments = list_segments(&dir, "sess-b");
        assert!(
            segments.len() >= 3,
            "应产生主段 + 轮换段，实际 {segments:?}"
        );
        // 轮换段内容连续（旧段在前）
        let seg0 = std::fs::read_to_string(dir.join("sess-b.log.1")).unwrap();
        assert!(seg0.lines().count() >= 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotated_segments_are_listed() {
        let dir = test_dir("list");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sess-c.log"), "1\n").unwrap();
        std::fs::write(dir.join("sess-c.log.1"), "0\n").unwrap();
        std::fs::write(dir.join("sess-c.bin"), "raw").unwrap();
        std::fs::write(dir.join("other.log"), "no").unwrap();
        let segments = list_segments(&dir, "sess-c");
        assert_eq!(segments.len(), 3);
        assert!(segments[0].ends_with("sess-c.bin"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
