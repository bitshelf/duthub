//! 会话日志管道（M1.5，`docs/SECURITY.md` #8）。
//!
//! - 文本流：utf8-lossy 行，append-only，按大小轮换（`<session>.log` → `.1.log` → …）。
//! - 原始字节流：`<session>.bin` 追加，**永不**按行重组/转码，保证 panic/oops 证据不被
//!   ANSI/编码处理污染。二进制流不轮换内联截断，超过 `raw_cap` 起新序号文件。
//! - 结构化记录流（`.rec`，第三条流）：zerocopy 定长记录头 + 载荷，逐条 flush，
//!   格式与读取见 `crate::rec` 与 `docs/reference/REC_LOG_FORMAT.md`。`.log`/`.bin` 仍是
//!   审计权威；`.rec` 是带类型/时间戳的伴生流（终端日志视图数据源）。
//! - 写入目录由调用方给定（产品用解析后工作目录 `logs/`，见
//!   `duthub_core::store::resolve_work_dir_in`）。

use std::fs::OpenOptions;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use duthub_core::{DeviceError, DeviceResult};

use crate::rec::{self, RecKind};

const TEXT_ROTATE_BYTES: u64 = 2 * 1024 * 1024;
const RAW_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ROTATIONS: u32 = 20;
type LogFile = BufWriter<Box<dyn Write + Send>>;

/// One of the three per-session log streams (`.log` / `.bin` / `.rec`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Text,
    Raw,
    Rec,
}

impl Stream {
    fn extension(self) -> &'static str {
        match self {
            Stream::Text => "log",
            Stream::Raw => "bin",
            Stream::Rec => "rec",
        }
    }
}

pub struct SessionLogWriter {
    dir: PathBuf,
    session_id: String,
    text: Option<LogFile>,
    raw: Option<LogFile>,
    rec: Option<LogFile>,
    text_bytes: u64,
    raw_bytes: u64,
    rec_bytes: u64,
    text_rotation: u32,
    raw_rotation: u32,
    rec_rotation: u32,
    text_rotate_bytes: u64,
    raw_rotate_bytes: u64,
    saving: Option<Arc<AtomicBool>>,
    portable: Option<PortableArchive>,
    boot_reference: Option<crate::boot_reference::BootReference>,
    split_logs: bool,
    /// Reusable encode buffer for `.rec` records (one allocation, reused).
    rec_scratch: Vec<u8>,
    /// 会话日志开关（串口设置页）：false = .log/.bin/.rec 停写（总线事件不受影响）。
    /// 默认 true——实时落盘是产品红线，关闭是用户显式选择。
    enabled: bool,
}

struct PortableArchive {
    root: PathBuf,
    dut_id: String,
    name: String,
    quota: Arc<duthub_core::dut_quota::DutQuota>,
}

impl PortableArchive {
    fn continuous(&self) -> std::io::Result<(LogFile, LogFile, LogFile)> {
        let (_, owner) =
            duthub_core::dut_artifacts::open_dut_for_write(&self.root, &self.dut_id, &self.name)?;
        let text = self.quota.append_log(
            &self.dut_id,
            &owner,
            duthub_core::log_archive::CONTINUOUS_SERIAL_LOG,
        )?;
        let raw = self.quota.append_log(&self.dut_id, &owner, "serial.bin")?;
        let mut rec = self.quota.append_log(&self.dut_id, &owner, "serial.rec")?;
        if rec.is_empty()? {
            rec::write_preamble_new(&mut rec)?;
        }
        Ok((
            BufWriter::new(Box::new(text)),
            BufWriter::new(Box::new(raw)),
            BufWriter::new(Box::new(rec)),
        ))
    }

    fn next_segment(&self) -> std::io::Result<(LogFile, LogFile, LogFile)> {
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
            let rec_path = format!("{stem}.rec");
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
            let mut rec = match self.quota.create(&self.dut_id, &owner, &rec_path) {
                Ok(rec) => rec,
                Err(error) => {
                    drop(text);
                    drop(raw);
                    let _ = owner.remove_file(&text_path);
                    let _ = owner.remove_file(&raw_path);
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        continue;
                    }
                    return Err(error);
                }
            };
            if let Err(error) = rec::write_preamble_new(&mut rec) {
                drop(text);
                drop(raw);
                drop(rec);
                let _ = owner.remove_file(&text_path);
                let _ = owner.remove_file(&raw_path);
                let _ = owner.remove_file(&rec_path);
                return Err(error);
            }
            index.seek(SeekFrom::Start(0))?;
            index.write_all(&sequence.to_le_bytes())?;
            return Ok((
                BufWriter::new(Box::new(text)),
                BufWriter::new(Box::new(raw)),
                BufWriter::new(Box::new(rec)),
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
            rec: None,
            text_bytes: 0,
            raw_bytes: 0,
            rec_bytes: 0,
            text_rotation: 0,
            raw_rotation: 0,
            rec_rotation: 0,
            text_rotate_bytes,
            raw_rotate_bytes,
            saving: None,
            portable: None,
            boot_reference: None,
            split_logs: true,
            rec_scratch: Vec::new(),
            enabled: true,
        };
        writer.text = Some(writer.open_file(Stream::Text)?);
        writer.raw = Some(writer.open_file(Stream::Raw)?);
        writer.rec = Some(writer.open_file(Stream::Rec)?);
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
            rec: None,
            text_bytes: 0,
            raw_bytes: 0,
            rec_bytes: 0,
            text_rotation: 0,
            raw_rotation: 0,
            rec_rotation: 0,
            text_rotate_bytes: u64::MAX,
            raw_rotate_bytes: u64::MAX,
            saving: Some(saving),
            portable: None,
            boot_reference: None,
            split_logs: true,
            rec_scratch: Vec::new(),
            enabled: true,
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

    /// 会话日志开关（串口设置页）：关 = .log/.bin 停写，总线事件照常。
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn set_boot_reference(
        &mut self,
        reference: Option<crate::boot_reference::BootReference>,
    ) {
        if self.split_logs {
            self.boot_reference = reference;
        }
    }

    pub(crate) fn set_log_mode(
        &mut self,
        split: bool,
        reference: Option<crate::boot_reference::BootReference>,
    ) -> DeviceResult<()> {
        if self.split_logs == split
            && match (&self.boot_reference, &reference) {
                (None, None) => true,
                (Some(current), Some(next)) => current.same_anchor(next),
                _ => false,
            }
        {
            return Ok(());
        }
        self.flush_rx_prefix()?;
        if self.split_logs != split && self.portable.is_some() {
            self.finish()?;
            self.text_bytes = 0;
            self.raw_bytes = 0;
            self.rec_bytes = 0;
        }
        self.split_logs = split;
        self.boot_reference = if split { reference } else { None };
        Ok(())
    }

    pub(crate) fn append_rx(&mut self, bytes: Vec<u8>, text: &str) -> DeviceResult<()> {
        let Some(reference) = self.boot_reference.as_mut() else {
            return self.write_rx(&bytes, text);
        };
        let (data, boundaries) = reference.partition(bytes);
        let mut start = 0;
        let mut split_error = None;
        for boundary in boundaries {
            self.write_rx_bytes(&data[start..boundary])?;
            if self.raw_bytes > 0
                && let Err(error) = self.split()
            {
                split_error.get_or_insert(error);
            }
            start = boundary;
        }
        self.write_rx_bytes(&data[start..])?;
        split_error.map_or(Ok(()), Err)
    }

    pub(crate) fn flush_rx_prefix(&mut self) -> DeviceResult<()> {
        if let Some(reference) = &mut self.boot_reference {
            let pending = reference.take_pending();
            self.write_rx_bytes(&pending)?;
        }
        Ok(())
    }

    pub(crate) fn power_mark(&mut self, line: &str) -> DeviceResult<()> {
        self.flush_rx_prefix()?;
        if let Some(reference) = &mut self.boot_reference {
            // Some firmware starts printing immediately after a shell prompt
            // that never ended its line.
            reference.expect_new_line();
            self.append_text(line)?;
            let _ = self.append_rec(RecKind::Mark, line.as_bytes());
            Ok(())
        } else {
            let _ = self.append_rec(RecKind::Mark, line.as_bytes());
            self.mark(line)
        }
    }

    fn write_rx_bytes(&mut self, bytes: &[u8]) -> DeviceResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.write_rx(bytes, &String::from_utf8_lossy(bytes))
    }

    fn write_rx(&mut self, bytes: &[u8], text: &str) -> DeviceResult<()> {
        let result = self.append(bytes, text);
        let _ = self.append_rec(RecKind::Raw, bytes);
        result
    }

    fn ensure_writing(&mut self) -> DeviceResult<bool> {
        if !self.enabled {
            return Ok(false);
        }
        if let Some(saving) = &self.saving {
            if !saving.load(Ordering::Acquire) {
                if self.text.is_some() {
                    self.finish()?;
                    self.text_rotation += 1;
                    self.raw_rotation += 1;
                    self.rec_rotation += 1;
                    self.text_bytes = 0;
                    self.raw_bytes = 0;
                    self.rec_bytes = 0;
                }
                return Ok(false);
            }
            if self.text.is_none() {
                let (text, raw, rec) = if let Some(portable) = &self.portable {
                    (if self.split_logs {
                        portable.next_segment()
                    } else {
                        portable.continuous()
                    })
                    .map_err(|error| DeviceError::Invalid(error.to_string()))?
                } else {
                    std::fs::create_dir_all(&self.dir)
                        .map_err(|e| DeviceError::Invalid(format!("创建日志目录失败: {e}")))?;
                    (
                        self.open_file(Stream::Text)?,
                        self.open_file(Stream::Raw)?,
                        self.open_file(Stream::Rec)?,
                    )
                };
                self.text = Some(text);
                self.raw = Some(raw);
                self.rec = Some(rec);
            }
        }
        Ok(true)
    }

    fn stem(&self, stream: Stream) -> String {
        format!("{}.{}", self.session_id, stream.extension())
    }

    fn rotated_name(&self, stream: Stream, index: u32) -> String {
        format!("{}.{}.{index}", self.session_id, stream.extension())
    }

    fn rotation_of(&self, stream: Stream) -> u32 {
        match stream {
            Stream::Text => self.text_rotation,
            Stream::Raw => self.raw_rotation,
            Stream::Rec => self.rec_rotation,
        }
    }

    fn open_file(&self, stream: Stream) -> DeviceResult<LogFile> {
        self.open_at(stream, self.rotation_of(stream))
    }

    fn open_at(&self, stream: Stream, rotation: u32) -> DeviceResult<LogFile> {
        let name = if self.saving.is_some() {
            format!("{}-{rotation:010}.{}", self.session_id, stream.extension())
        } else if rotation == 0 {
            // 首段使用主名（.log / .bin / .rec）
            self.stem(stream)
        } else {
            self.rotated_name(stream, rotation)
        };
        let path = self.dir.join(name);
        // 首段（index 0）覆盖旧残留；轮换段追加
        let mut file = if self.saving.is_some() {
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
        // `.rec` segments start with the format preamble; only when the file is
        // still empty (a re-opened segment keeps its existing preamble).
        if stream == Stream::Rec {
            rec::write_preamble(&mut file).map_err(|e| {
                DeviceError::Invalid(format!("写 .rec 头失败 {}: {e}", path.display()))
            })?;
        }
        Ok(BufWriter::new(Box::new(file)))
    }

    /// 开始新的文本与原始日志段。两个新句柄均打开后再切换写入目标。
    pub fn split(&mut self) -> DeviceResult<()> {
        if !self.split_logs {
            return Ok(());
        }
        if self.saving.is_some() {
            if self.text.is_some() {
                self.finish()?;
                self.text_rotation += 1;
                self.raw_rotation += 1;
                self.rec_rotation += 1;
                self.text_bytes = 0;
                self.raw_bytes = 0;
                self.rec_bytes = 0;
            }
            return Ok(());
        }
        if self.text_rotation >= MAX_ROTATIONS
            || self.raw_rotation >= MAX_ROTATIONS
            || self.rec_rotation >= MAX_ROTATIONS
        {
            return Err(DeviceError::Invalid("会话日志分段数已达上限".into()));
        }
        let text = self.open_at(Stream::Text, self.text_rotation + 1)?;
        let raw = self.open_at(Stream::Raw, self.raw_rotation + 1)?;
        let rec = self.open_at(Stream::Rec, self.rec_rotation + 1)?;
        self.text = Some(text);
        self.raw = Some(raw);
        self.rec = Some(rec);
        self.text_rotation += 1;
        self.raw_rotation += 1;
        self.rec_rotation += 1;
        self.text_bytes = 0;
        self.raw_bytes = 0;
        self.rec_bytes = 0;
        Ok(())
    }

    fn rotate(&mut self, stream: Stream) -> DeviceResult<()> {
        match stream {
            Stream::Raw => {
                if self.raw_rotation >= MAX_ROTATIONS {
                    return Err(DeviceError::Invalid("原始日志轮换超限".into()));
                }
                self.raw_rotation += 1;
                self.raw = Some(self.open_file(Stream::Raw)?);
                self.raw_bytes = 0;
            }
            Stream::Rec => {
                if self.rec_rotation >= MAX_ROTATIONS {
                    return Err(DeviceError::Invalid("记录日志轮换超限".into()));
                }
                self.rec_rotation += 1;
                self.rec = Some(self.open_file(Stream::Rec)?);
                self.rec_bytes = 0;
            }
            Stream::Text => {
                if self.text_rotation >= MAX_ROTATIONS {
                    return Err(DeviceError::Invalid("文本日志轮换超限".into()));
                }
                self.text_rotation += 1;
                self.text = Some(self.open_file(Stream::Text)?);
                self.text_bytes = 0;
            }
        }
        Ok(())
    }

    /// Append one record to the `.rec` stream. This is the companion stream of
    /// `.log`/`.bin` (same writer thread, per-entry flush); failures are
    /// reported to the caller but never roll back the audit streams, which
    /// stay authoritative.
    pub fn append_rec(&mut self, kind: RecKind, payload: &[u8]) -> DeviceResult<()> {
        if !self.ensure_writing()? {
            return Ok(());
        }
        rec::encode_record(kind, rec::now_unix_ns(), payload, &mut self.rec_scratch);
        let Some(rec) = self.rec.as_mut() else {
            return Err(DeviceError::Invalid("记录日志未初始化".into()));
        };
        rec.write_all(&self.rec_scratch)
            .and_then(|()| rec.flush())
            .map_err(|e| DeviceError::Invalid(format!("写记录日志失败: {e}")))?;
        self.rec_bytes += self.rec_scratch.len() as u64;
        if self.split_logs && self.rec_bytes >= self.raw_rotate_bytes {
            self.rotate(Stream::Rec)?;
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
            if self.split_logs && self.raw_bytes >= self.raw_rotate_bytes {
                self.rotate(Stream::Raw)?;
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
        if self.split_logs
            && self.boot_reference.is_none()
            && let Some(portable) = &self.portable
        {
            let limit = (portable.quota.limit() / 8).min(16 * 1024 * 1024);
            let used = self.text_bytes + self.raw_bytes + self.rec_bytes;
            if used > 0 && used + pending as u64 > limit {
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
        if self.split_logs && self.text_bytes >= self.text_rotate_bytes {
            self.rotate(Stream::Text)?;
        }
        Ok(())
    }

    pub fn mark(&mut self, line: &str) -> DeviceResult<()> {
        if !self.split_logs || self.boot_reference.is_some() {
            return self.append_text(line);
        }
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
        if self.split_logs && self.raw_bytes >= self.raw_rotate_bytes {
            self.rotate(Stream::Raw)?;
        }
        Ok(())
    }

    /// 会话关闭：flush 并关闭三条流。
    pub fn finish(&mut self) -> DeviceResult<()> {
        if let Some(mut text) = self.text.take() {
            text.flush()
                .map_err(|e| DeviceError::Invalid(e.to_string()))?;
        }
        if let Some(mut raw) = self.raw.take() {
            raw.flush()
                .map_err(|e| DeviceError::Invalid(e.to_string()))?;
        }
        if let Some(mut rec) = self.rec.take() {
            rec.flush()
                .map_err(|e| DeviceError::Invalid(e.to_string()))?;
        }
        Ok(())
    }
}

/// 列出某会话已产生的日志段（测试与 UI 检查用）。
///
/// Only the `.log`/`.bin` stream families: the `.rec` structured stream has
/// its own listing (`rec::list_rec_segments`) and must not leak into this
/// result (existing callers/tests assert the exact log/bin pair set).
pub fn list_segments(dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    let prefix = format!("{session_id}.");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let family = suffix.split('.').next().unwrap_or_default();
        if matches!(family, "log" | "bin") {
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

    /// 普通模式按时间顺序收集某一流的所有段：主段（.log/.bin）最旧在前，
    /// 之后 .1/.2/… 依次更晚（rotate/split 都是 rotation+1 开新文件）。
    fn chronological_segments(dir: &Path, session_id: &str, raw: bool) -> Vec<PathBuf> {
        let extension = if raw { "bin" } else { "log" };
        let mut found = Vec::new();
        let first = dir.join(format!("{session_id}.{extension}"));
        if first.exists() {
            found.push(first);
        }
        for index in 1..=MAX_ROTATIONS {
            let path = dir.join(format!("{session_id}.{extension}.{index}"));
            if !path.exists() {
                break;
            }
            found.push(path);
        }
        found
    }

    /// LOG-1 逐条实时落盘（AGENTS.md 坑 #47「逐条 flush 语义不变、实时落盘、
    /// 零丢失」红线）：append/append_raw/note/mark 都是 write_all + flush 后才
    /// 返回（BufWriter 只作缓冲不攒批），任一时刻读盘文件必已含此前所有已确认
    /// 消息，不缓冲到会话结束。每步写完立即 std::fs::read 断言，全程不手动
    /// flush/drop——攒批实现必炸。末尾用宽松时限上界锁机制（200 条 0 间隔
    /// append 远低于 10s），只防攒批/逐条卡死，不写窄时序断言。
    #[test]
    fn log_writes_are_visible_on_disk_immediately() {
        let dir = test_dir("live-flush");
        let mut writer = SessionLogWriter::new(&dir, "sess-live").unwrap();
        // append：返回后文本与原始字节立刻可见
        writer.append(b"rx-1\r\n", "rx-1").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-live.log")).unwrap(),
            "rx-1\n"
        );
        assert_eq!(
            std::fs::read(dir.join("sess-live.bin")).unwrap(),
            b"rx-1\r\n"
        );
        // append_raw：返回后 .bin 立刻增长
        writer.append_raw(b"typed\r\n").unwrap();
        assert_eq!(
            std::fs::read(dir.join("sess-live.bin")).unwrap(),
            b"rx-1\r\ntyped\r\n"
        );
        // note：注记行立刻可见
        writer.note("[note] hello").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-live.log")).unwrap(),
            "rx-1\n[note] hello\n"
        );
        // mark：标记行立刻可见（本测试只锁 flush 时机；切段行为由
        // mark_splits_pair_and_preserves_previous_segment 锁）
        writer.mark("[mark] seg-a").unwrap();
        assert!(
            std::fs::read_to_string(dir.join("sess-live.log"))
                .unwrap()
                .ends_with("[mark] seg-a\n")
        );
        // 宽松上界：200 条 append 完成 + 不调用 finish/drop，磁盘必已含全部行
        let started = std::time::Instant::now();
        for i in 0..200u32 {
            let line = format!("line-{i}");
            writer.append(line.as_bytes(), &line).unwrap();
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "逐条落盘不得攒批或卡死：{:?}",
            started.elapsed()
        );
        let tail = std::fs::read_to_string(dir.join("sess-live.log.1")).unwrap();
        for i in 0..200u32 {
            assert!(tail.contains(&format!("line-{i}\n")), "缺 line-{i}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-8：`.rec` 结构化记录流逐条 flush 与 .log/.bin 同语义——每次
    /// append_rec 返回后读盘必须能零拷贝解析出此前全部记录（不攒批），
    /// 载荷逐字节一致且含多字节 UTF-8。
    #[test]
    fn rec_records_are_visible_on_disk_immediately() {
        let dir = test_dir("rec-live-flush");
        let mut writer = SessionLogWriter::new(&dir, "sess-rec-live").unwrap();
        writer.append_rec(RecKind::Raw, b"rx-1\r\n").unwrap();
        writer
            .append_rec(RecKind::Note, "[note] 中文注记".as_bytes())
            .unwrap();
        // 不 flush/drop，直接读盘解析
        let raw = std::fs::read(dir.join("sess-rec-live.rec")).unwrap();
        let batch = crate::rec::parse_records(&raw);
        assert_eq!(batch.records.len(), 2, "逐条 flush：两条记录立即可解析");
        assert_eq!(batch.records[0].kind, RecKind::Raw);
        assert_eq!(batch.records[0].payload, b"rx-1\r\n");
        assert_eq!(batch.records[1].kind, RecKind::Note);
        assert_eq!(batch.records[1].payload, "[note] 中文注记".as_bytes());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-9：`.rec` 大小轮换与 split 联动——小阈值触发真实轮换路径（与
    /// .bin 同阈值），轮换段按主段 → .1 时间序拼接等于完整记录流；
    /// `split()` 三条流一起切段，`.rec` 段与 .log/.bin 段序对齐。
    #[test]
    fn rec_rotates_and_splits_with_pair() {
        let dir = test_dir("rec-rotate");
        let mut writer = SessionLogWriter::with_limits(&dir, "sess-rec-rot", 64, 64).unwrap();
        for i in 0..5u32 {
            writer
                .append_rec(RecKind::Raw, format!("record-{i:02}").as_bytes())
                .unwrap();
        }
        writer.split().unwrap();
        writer.append_rec(RecKind::Mark, b"after-split").unwrap();
        writer.finish().unwrap();
        // 按段序拼接：主段 + 所有轮换段（旋转计数兼容 rec 自己的阈值轮换）
        let mut paths = vec![dir.join("sess-rec-rot.rec")];
        for rotation in 1..=MAX_ROTATIONS {
            let path = dir.join(format!("sess-rec-rot.rec.{rotation}"));
            if path.exists() {
                paths.push(path);
            } else {
                break;
            }
        }
        assert!(paths.len() >= 2, "小阈值必须触发 .rec 轮换");
        // 按段逐个解析：每段带独立 preamble，原始字节直接拼接会被第二段的
        // preamble 卡停——这也是终端日志视图按段读取的原因
        let all: Vec<(RecKind, Vec<u8>)> = paths
            .iter()
            .flat_map(|path| {
                let bytes = std::fs::read(path).unwrap();
                crate::rec::parse_records(&bytes)
                    .records
                    .iter()
                    .map(|record| (record.kind, record.payload.to_vec()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let kinds: Vec<RecKind> = all.iter().map(|(kind, _)| *kind).collect();
        let payloads: Vec<&[u8]> = all.iter().map(|(_, payload)| payload.as_slice()).collect();
        assert_eq!(
            payloads,
            vec![
                b"record-00".as_slice(),
                b"record-01".as_slice(),
                b"record-02".as_slice(),
                b"record-03".as_slice(),
                b"record-04".as_slice(),
                b"after-split",
            ],
            "分段拼接必须等于完整记录流（kinds: {kinds:?}）"
        );
        assert_eq!(*kinds.last().unwrap(), RecKind::Mark);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-10：`.rec` 跟随会话日志开关——停写后 append_rec 不产生任何字节，
    /// 恢复后继续追加（普通模式句柄保留，内容接续不重开）。
    #[test]
    fn rec_toggle_follows_session_logging() {
        let dir = test_dir("rec-toggle");
        let mut writer = SessionLogWriter::new(&dir, "sess-rec-toggle").unwrap();
        writer.append_rec(RecKind::Raw, b"before").unwrap();
        writer.set_enabled(false);
        writer.append_rec(RecKind::Raw, b"muted").unwrap();
        let raw = std::fs::read(dir.join("sess-rec-toggle.rec")).unwrap();
        let batch = crate::rec::parse_records(&raw);
        assert_eq!(batch.records.len(), 1, "关闭期间必须停写");
        assert_eq!(batch.records[0].payload, b"before");
        writer.set_enabled(true);
        writer.append_rec(RecKind::Raw, b"after").unwrap();
        let raw = std::fs::read(dir.join("sess-rec-toggle.rec")).unwrap();
        let batch = crate::rec::parse_records(&raw);
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.records[1].payload, b"after");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-2 双流语义分离：.log 记文本行（逐行 \n 结尾），.bin 记真实线上字节
    /// 不转码不重组（含非法 UTF-8/NUL/\r\n 原样保真）；append_raw（键入字节
    /// 审计）只进 .bin，.log 不动。[TX] 注记换行转义为字面 \r/\n 属 demo.rs
    /// single_line_tx（P2-034），由既有测试 tx_annotation_is_single_line_for_
    /// multiline_text 锁定——本测试锁 logs.rs 侧的双流分离与字节保真。
    #[test]
    fn append_raw_is_bin_only_and_bin_keeps_verbatim_bytes() {
        let dir = test_dir("pair-split");
        let mut writer = SessionLogWriter::new(&dir, "sess-pair").unwrap();
        writer
            .append(b"root@dev:~# uname\r\n", "root@dev:~# uname")
            .unwrap();
        // 键入字节审计：含非法 UTF-8 与 NUL 的线上字节只允许进 .bin
        writer.append_raw(b"\xff\xfe\x00whoami\r\n").unwrap();
        // .bin 逐字节保真：行尾 \r\n 原样、非法字节原样，无转码无重组
        assert_eq!(
            std::fs::read(dir.join("sess-pair.bin")).unwrap(),
            b"root@dev:~# uname\r\n\xff\xfe\x00whoami\r\n"
        );
        // .log 只含文本行：raw-only 字节不得漏进文本流
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-pair.log")).unwrap(),
            "root@dev:~# uname\n"
        );
        // 多字节 UTF-8 文本行原样进 .log；.bin 同步记线上字节（含行尾）
        writer.append("终端 ok\r\n".as_bytes(), "终端 ok").unwrap();
        assert!(
            std::fs::read_to_string(dir.join("sess-pair.log"))
                .unwrap()
                .contains("终端 ok\n")
        );
        assert!(
            std::fs::read(dir.join("sess-pair.bin"))
                .unwrap()
                .ends_with("终端 ok\r\n".as_bytes())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-3 普通模式按会话分文件互不混入：同一目录下 <session>.log/.bin 只装
    /// 本会话内容，list_segments 只列本会话前缀（`{session_id}.` 开头）的段，
    /// 相近前缀（sess-alicia）与他会话文件一律不进列表。
    #[test]
    fn sessions_in_shared_dir_stay_separate_and_list_filters_prefix() {
        let dir = test_dir("shared-dir");
        let mut alice = SessionLogWriter::new(&dir, "sess-alice").unwrap();
        let mut bob = SessionLogWriter::new(&dir, "sess-bob").unwrap();
        alice.append(b"alice-1", "alice-1").unwrap();
        bob.append(b"bob-1", "bob-1").unwrap();
        alice.append(b"alice-2", "alice-2").unwrap();
        alice.finish().unwrap();
        bob.finish().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-alice.log")).unwrap(),
            "alice-1\nalice-2\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-bob.log")).unwrap(),
            "bob-1\n"
        );
        assert_eq!(
            std::fs::read(dir.join("sess-alice.bin")).unwrap(),
            b"alice-1alice-2"
        );
        // 异会话/相近前缀文件不进本会话段列表
        std::fs::write(dir.join("sess-bob.log.1"), "no\n").unwrap();
        std::fs::write(dir.join("sess-alicia.log"), "no\n").unwrap();
        std::fs::write(dir.join("other.log"), "no\n").unwrap();
        let segments = list_segments(&dir, "sess-alice");
        assert_eq!(segments.len(), 2, "只列本会话前缀：{segments:?}");
        assert!(segments.iter().all(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("sess-alice.")
        }));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-3 大小阈值轮换（产品默认文本 2MB/原始 8MB，测试用 with_limits 配
    /// 小阈值走真实轮换路径）：文本流与原始流各自独立轮换；旧段内容原样保留
    /// 且在前——按主段 → .1 → .2 时间序拼接必须等于完整流（不丢不乱，边界
    /// 位置不锁死，将来改轮换策略测试仍成立）。
    #[test]
    fn rotated_segments_preserve_text_and_raw_continuity() {
        let dir = test_dir("rotate-continuity");
        let mut writer = SessionLogWriter::with_limits(&dir, "sess-cont", 96, 64).unwrap();
        let mut expected_text = String::new();
        let mut expected_raw = Vec::new();
        for i in 0..12u32 {
            let line = format!("[{i:02}] line payload {i:02}");
            writer
                .append(format!("{line}\r\n").as_bytes(), &line)
                .unwrap();
            expected_text.push_str(&line);
            expected_text.push('\n');
            expected_raw.extend_from_slice(format!("{line}\r\n").as_bytes());
        }
        writer.finish().unwrap();
        let text_segments = chronological_segments(&dir, "sess-cont", false);
        let raw_segments = chronological_segments(&dir, "sess-cont", true);
        assert!(text_segments.len() > 1, "文本流应轮换：{text_segments:?}");
        assert!(raw_segments.len() > 1, "原始流应轮换：{raw_segments:?}");
        let mut text_all = String::new();
        for path in &text_segments {
            text_all.push_str(&std::fs::read_to_string(path).unwrap());
        }
        assert_eq!(text_all, expected_text, "文本流拼接必须连续");
        let mut raw_all = Vec::new();
        for path in &raw_segments {
            raw_all.extend(std::fs::read(path).unwrap());
        }
        assert_eq!(raw_all, expected_raw, "原始流拼接必须连续");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-3 归档（portable）模式按 DUT 名称目录 + 递增序号命名，并按配额阈值
    /// 自动分段（ensure_segment_space：段内累计超过 limit/8 即切新段）：全部
    /// 行按写入顺序分布在各段中不丢不乱。跨重启续号由既有测试
    /// portable_log_numbers_continue_across_splits_and_reopen 锁定。
    #[test]
    fn portable_archive_splits_on_size_threshold_and_keeps_order() {
        let root = test_dir("portable-size");
        // portable 的 next_segment 经 open_dut_for_write 需要已存在的根目录
        std::fs::create_dir_all(&root).unwrap();
        let saving = Arc::new(AtomicBool::new(true));
        // 配额 64KB → 单段阈值 8KB；30 × ~573B 文本必触发自动切段
        let quota = duthub_core::dut_quota::DutQuota::new(64 * 1024);
        let mut log = SessionLogWriter::portable_archive(&root, "sz", "SIZE-DUT", saving, quota);
        let mut lines = Vec::new();
        for i in 0..30u32 {
            let line = format!("seg-line-{i:02} {}", "x".repeat(560));
            log.note(&line).unwrap();
            lines.push(line);
        }
        let archive = root.join("duthub-data");
        let segments = duthub_core::log_archive::list(&archive, "sz", 0, 20);
        let ids: Vec<&str> = segments.iter().map(|s| s.id.as_str()).collect();
        assert!(segments.len() >= 2, "应触发自动分段：{ids:?}");
        // list 最新在前 → 反转为时间序拼接 == 全部行按写入顺序
        let mut all_text = String::new();
        for segment in segments.iter().rev() {
            all_text.push_str(
                &duthub_core::log_archive::read(&archive, "sz", &segment.id, 0)
                    .unwrap()
                    .text,
            );
        }
        let mut expected = String::new();
        for line in &lines {
            expected.push_str(line);
            expected.push('\n');
        }
        assert_eq!(all_text, expected, "分段后行序必须连续");
        drop(log);
        std::fs::remove_dir_all(root).ok();
    }

    /// LOG-4 会话日志开关即时生效且语义明确（串口设置页，普通模式）：
    /// 默认开（实时落盘是产品红线，关闭是用户显式选择）；关 = .log/.bin 停写
    /// 且文件内容逐字节不变；开 = 同文件续写（不开新段）。
    /// 「总线事件/MCP serial_read 不受影响」属 SessionCtx/驱动层
    /// （demo.rs emit_raw 总线发布、lib.rs set_session_logging），不在本文件
    /// 可测范围；新会话按构造值初始化的接线在 lib.rs 会话创建处。
    #[test]
    fn session_toggle_stops_file_writes_and_resumes_same_file() {
        let dir = test_dir("toggle");
        let mut writer = SessionLogWriter::new(&dir, "sess-toggle").unwrap();
        assert!(writer.enabled(), "新会话默认开启");
        writer.append(b"before-off", "before-off").unwrap();
        writer.append_raw(b"raw-before").unwrap();
        let text_before = std::fs::read_to_string(dir.join("sess-toggle.log")).unwrap();
        let raw_before = std::fs::read(dir.join("sess-toggle.bin")).unwrap();

        writer.set_enabled(false);
        assert!(!writer.enabled());
        // 关闭期间各类写入全部不落盘，文件内容一字节不变
        writer.append(b"while-off", "while-off").unwrap();
        writer.append_raw(b"raw-off").unwrap();
        writer.note("[note] while-off").unwrap();
        writer.append_text("[text] while-off").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-toggle.log")).unwrap(),
            text_before
        );
        assert_eq!(
            std::fs::read(dir.join("sess-toggle.bin")).unwrap(),
            raw_before
        );

        // 重新开启：同文件续写（普通模式不开新段），已确认内容原样保留
        writer.set_enabled(true);
        writer.append(b"after-on", "after-on").unwrap();
        writer.append_raw(b"raw-on").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-toggle.log")).unwrap(),
            "before-off\nafter-on\n"
        );
        let mut raw_after = raw_before.clone();
        raw_after.extend_from_slice(b"after-onraw-on");
        assert_eq!(
            std::fs::read(dir.join("sess-toggle.bin")).unwrap(),
            raw_after
        );
        assert_eq!(
            chronological_segments(&dir, "sess-toggle", false).len(),
            1,
            "普通模式开关切换不得开新段"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-4 归档模式 saving 开关：关 = 当前段 finish 收尾、停写且已确认内容
    /// 原样保留；开 = 开新段续写（portable 走 next_segment 递增序号），已确认
    /// 内容不因开关切换丢失。关闭期间入队的消息被拒写（开关后的输出才落盘）
    /// ——写线程队列的「先入队消息不丢」由 demo.rs 写线程测试锁。
    #[test]
    fn archive_saving_toggle_finishes_segment_and_resumes_new_segment() {
        let root = test_dir("portable-saving");
        // portable 的 next_segment 经 open_dut_for_write 需要已存在的根目录
        std::fs::create_dir_all(&root).unwrap();
        let saving = Arc::new(AtomicBool::new(true));
        let quota = duthub_core::dut_quota::DutQuota::new(1024 * 1024);
        let mut log =
            SessionLogWriter::portable_archive(&root, "sv", "SAVING-DUT", saving.clone(), quota);
        log.note("seg-a-1").unwrap();
        log.note("seg-a-2").unwrap();
        let archive = root.join("duthub-data");
        let first = duthub_core::log_archive::list(&archive, "sv", 0, 10);
        assert_eq!(first.len(), 1);
        let first_id = first[0].id.clone();

        // 关闭：停写，当前段收尾（内容原样保留）
        saving.store(false, Ordering::Release);
        log.note("while-off").unwrap();
        log.mark("while-off").unwrap();
        assert_eq!(
            duthub_core::log_archive::list(&archive, "sv", 0, 10).len(),
            1
        );
        assert_eq!(
            duthub_core::log_archive::read(&archive, "sv", &first_id, 0)
                .unwrap()
                .text,
            "seg-a-1\nseg-a-2\n"
        );

        // 重新开启：新段续号（002），旧段不变
        saving.store(true, Ordering::Release);
        log.note("seg-b-1").unwrap();
        let segments = duthub_core::log_archive::list(&archive, "sv", 0, 10);
        let ids: Vec<&str> = segments.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(segments.len(), 2, "开启后应开新段：{ids:?}");
        assert_ne!(segments[0].id, first_id);
        assert!(
            segments[0].id.contains("-002_"),
            "重开应递增序号：{}",
            segments[0].id
        );
        assert_eq!(
            duthub_core::log_archive::read(&archive, "sv", &segments[0].id, 0)
                .unwrap()
                .text,
            "seg-b-1\n"
        );
        assert_eq!(
            duthub_core::log_archive::read(&archive, "sv", &first_id, 0)
                .unwrap()
                .text,
            "seg-a-1\nseg-a-2\n"
        );
        drop(log);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn continuous_mode_appends_across_sessions_and_keeps_numbered_history_readable() {
        use duthub_core::log_archive;

        let root = test_dir("continuous-serial");
        std::fs::create_dir_all(&root).unwrap();
        let quota = duthub_core::dut_quota::DutQuota::new(1024 * 1024);
        let writer = || {
            SessionLogWriter::portable_archive(
                &root,
                "continuous",
                "CONTINUOUS-DUT",
                Arc::new(AtomicBool::new(true)),
                quota.clone(),
            )
        };
        let mut first = writer();
        first.set_log_mode(false, None).unwrap();
        first
            .append_rx(b"first session\n".to_vec(), "first session")
            .unwrap();
        first.power_mark("[mark] Power ON").unwrap();
        first.mark("[mark] Agent").unwrap();
        first
            .append_rx(b"after markers\n".to_vec(), "after markers")
            .unwrap();
        first.finish().unwrap();

        let mut second = writer();
        second.set_log_mode(false, None).unwrap();
        second
            .append_rx(b"reopened\n".to_vec(), "reopened")
            .unwrap();
        second.set_log_mode(true, None).unwrap();
        second
            .append_rx(b"split mode\n".to_vec(), "split mode")
            .unwrap();
        second.set_log_mode(false, None).unwrap();
        second
            .append_rx(b"back to continuous\n".to_vec(), "back to continuous")
            .unwrap();
        second.finish().unwrap();
        let data = root.join("duthub-data");
        let segments = log_archive::list(&data, "continuous", 0, 10);
        assert_eq!(segments.len(), 2);
        let continuous = segments
            .iter()
            .find(|segment| segment.id.ends_with("/serial.txt"))
            .unwrap();
        let text = log_archive::read(&data, "continuous", &continuous.id, 0)
            .unwrap()
            .text;
        assert_eq!(
            text,
            "first session\n[mark] Power ON\n[mark] Agent\nafter markers\nreopened\nback to continuous\n"
        );
        let raw = std::fs::read(root.join("CONTINUOUS-DUT/logs/serial.bin")).unwrap();
        assert_eq!(
            raw,
            b"first session\nafter markers\nreopened\nback to continuous\n"
        );
        assert!(root.join("CONTINUOUS-DUT/logs/serial.rec").is_file());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn boot_reference_splits_serial_files_at_the_observed_prefix() {
        let root = test_dir("boot-reference");
        std::fs::create_dir_all(&root).unwrap();
        let mut log = SessionLogWriter::portable_archive(
            &root,
            "boot-dut",
            "BOOT-DUT",
            Arc::new(AtomicBool::new(true)),
            duthub_core::dut_quota::DutQuota::new(1024 * 1024),
        );
        log.set_boot_reference(crate::boot_reference::BootReference::from_capture(
            b"DDR Version 1\nU-Boot SPL\n",
        ));
        log.append_rx(b"DDR Version 1\nRunning\n".to_vec(), "")
            .unwrap();
        log.power_mark("[mark] Power off").unwrap();
        log.power_mark("[mark] Power on").unwrap();
        log.append_rx(b"Shutting down\nDDR Ver".to_vec(), "")
            .unwrap();
        log.append_rx(b"sion 2\nU-Boot SPL\n".to_vec(), "").unwrap();
        log.flush_rx_prefix().unwrap();
        log.finish().unwrap();

        let segments = duthub_core::log_archive::list(&root.join("duthub-data"), "boot-dut", 0, 10);
        assert_eq!(segments.len(), 2);
        for (segment, expected) in segments.iter().rev().zip([
            b"DDR Version 1\nRunning\nShutting down\n".as_slice(),
            b"DDR Version 2\nU-Boot SPL\n".as_slice(),
        ]) {
            let path = root.join(segment.id.strip_prefix("dut/").unwrap());
            assert_eq!(std::fs::read(path.with_extension("bin")).unwrap(), expected);
            assert!(path.with_extension("rec").is_file());
            let readable = std::fs::read_to_string(path).unwrap();
            assert!(readable.starts_with("DDR Version"));
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// LOG-5 mark 分段标记切文件：标记行本身落当前（旧）段并实时可见，随后
    /// 输出落新段（.log.1/.bin.1），旧段内容原样保留——「分段必须发生在控制
    /// 指令前：供电操作先落段再派发」（红线）。mark 同步返回成败；供电边界经
    /// 写线程 MarkSync ack 同步执行（排空此前队列后才 mark）的部分在
    /// demo.rs/lib.rs，不在本文件可测范围。bare split 的段落保留由既有测试
    /// explicit_split_keeps_previous_pair_and_writes_to_new_pair 锁定。
    #[test]
    fn mark_splits_pair_and_preserves_previous_segment() {
        let dir = test_dir("mark-split");
        let mut writer = SessionLogWriter::new(&dir, "sess-mark").unwrap();
        writer.append(b"boot ok\r\n", "boot ok").unwrap();
        writer.mark("[mark] power cycle").unwrap();
        writer.append(b"post\r\n", "post").unwrap();
        writer.finish().unwrap();
        // 旧段：设备输出 + 标记行（mark 行是本机注记，raw 记标记文本本身）
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-mark.log")).unwrap(),
            "boot ok\n[mark] power cycle\n"
        );
        assert_eq!(
            std::fs::read(dir.join("sess-mark.bin")).unwrap(),
            b"boot ok\r\n[mark] power cycle"
        );
        // 新段：mark 之后的输出
        assert_eq!(
            std::fs::read_to_string(dir.join("sess-mark.log.1")).unwrap(),
            "post\n"
        );
        assert_eq!(
            std::fs::read(dir.join("sess-mark.bin.1")).unwrap(),
            b"post\r\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LOG-5 分段失败同步返回错误：段数达到上限后 split 返回 Err 而非无限增长
    /// ——该 Err 是写线程 MarkSync ack 携带值，log_control_boundary（lib.rs）
    /// 据此拦截控制派发并广播错误注记（那半段在 lib.rs/demo.rs）。本测试锁
    /// Err 的产生与同步返回。
    #[test]
    fn split_reports_error_after_rotation_cap() {
        let dir = test_dir("split-cap");
        let mut writer = SessionLogWriter::new(&dir, "sess-cap").unwrap();
        for _ in 0..MAX_ROTATIONS {
            writer.split().unwrap();
        }
        let error = writer.split().unwrap_err();
        assert!(error.to_string().contains("分段"), "应报分段上限：{error}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
