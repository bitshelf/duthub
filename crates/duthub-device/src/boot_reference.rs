use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use duthub_core::ipc::{ChunkStyle, UiEvent};
use regex::Regex;

const MAX_REFERENCE_BYTES: u64 = 256 * 1024;
const MAX_ANCHOR_BYTES: usize = 256;
const LEARN_CYCLES: usize = 3;
const COMPARE_LINES: usize = 50;
const COMPARE_BYTES: usize = 8 * 1024;
const REFERENCE_LINES: usize = 100;
const MIN_LEARNED_LINES: usize = 5;
const POWER_OFF_DELAY: Duration = Duration::from_secs(3);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(25);

/// Learn from a GUI-confirmed power cycle. All actions still go through the
/// normal relay service so device ownership, approval and audit rules apply.
pub fn learn(
    device: &crate::DeviceService,
    control: &crate::control::ControlService,
    dut_id: &str,
    minimum_percent: u8,
) -> Result<(String, f64), String> {
    if !(1..=100).contains(&minimum_percent) {
        return Err("最低相似度必须是 1 到 100 之间的整数".into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|error| error.to_string())?;
    let mut samples = Vec::with_capacity(LEARN_CYCLES);
    for cycle in 1..=LEARN_CYCLES {
        if !device.real_session_opened(dut_id) {
            return Err("串口未连接或已断开".into());
        }
        control
            .execute_confirmed_power(dut_id, false)
            .map_err(|error| error.to_string())?;
        std::thread::sleep(POWER_OFF_DELAY);
        let mut events = device.subscribe();
        if let Err(error) = control.execute_confirmed_power(dut_id, true) {
            return match control.execute_confirmed_power(dut_id, true) {
                Ok(()) => Err(format!("第 {cycle} 轮上电失败，已恢复供电: {error}")),
                Err(recovery) => Err(format!(
                    "第 {cycle} 轮上电失败 ({error})，恢复供电也失败: {recovery}"
                )),
            };
        }
        let mut raw = String::new();
        let capture = async {
            while raw.len() < MAX_REFERENCE_BYTES as usize {
                let event = events.recv().await.map_err(|error| error.to_string())?;
                if event.dut_id.as_deref() != Some(dut_id) {
                    continue;
                }
                match event.event {
                    UiEvent::SerialChunk {
                        text,
                        style: ChunkStyle::Normal,
                    } => {
                        raw.push_str(&text);
                        if raw.lines().take(REFERENCE_LINES).count() == REFERENCE_LINES
                            || raw.lines().any(is_kernel_boundary)
                        {
                            break;
                        }
                    }
                    UiEvent::SerialDisconnected { reason } => return Err(reason),
                    _ => {}
                }
            }
            Ok::<_, String>(())
        };
        if let Ok(result) =
            runtime.block_on(async { tokio::time::timeout(CAPTURE_TIMEOUT, capture).await })
        {
            result?;
        }
        let sample = reference_window(&raw);
        if sample.lines().count() < MIN_LEARNED_LINES {
            return Err(format!("第 {cycle} 轮启动文本不足，无法计算相似度"));
        }
        samples.push(sample);
    }
    evaluate(samples, minimum_percent)
}

fn is_kernel_boundary(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    line.contains("starting kernel") || line.contains("start kernel")
}

fn reference_window(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.trim().split_terminator('\n').take(REFERENCE_LINES) {
        lines.push(line.trim_end_matches('\r'));
        if is_kernel_boundary(line) {
            break;
        }
    }
    lines.join("\n")
}

fn evaluate(samples: Vec<String>, minimum_percent: u8) -> Result<(String, f64), String> {
    if samples.len() < 2 {
        return Err("至少需要两轮有效采样".into());
    }
    let normalized: Vec<_> = samples.iter().map(|sample| normalize(sample)).collect();
    if normalized.iter().any(String::is_empty) {
        return Err("至少一轮启动文本为空".into());
    }
    let mut weakest = 1.0_f64;
    for (index, sample) in normalized.iter().enumerate() {
        for other in &normalized[index + 1..] {
            weakest = weakest.min(similarity(sample, other));
        }
    }
    if weakest < f64::from(minimum_percent) / 100.0 {
        return Err(format!(
            "最低相似度 {:.2}%，低于 {minimum_percent}%",
            weakest * 100.0
        ));
    }
    Ok((
        samples.into_iter().last().expect("validated sample count"),
        weakest,
    ))
}

fn normalize(sample: &str) -> String {
    static ANSI: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap());
    static VOLATILE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\[\s*\d+\.\d+\](?:\[\s*T\d+\])?|\b0x[0-9a-f]+\b|\b[0-9a-f]{8,}\b|\b\d+(?:\.\d+)?\b").unwrap()
    });
    let prefix = sample[..sample.floor_char_boundary(sample.len().min(COMPARE_BYTES))]
        .lines()
        .take(COMPARE_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let prefix = ANSI.replace_all(&prefix, "").replace('\0', "");
    let normalized = VOLATILE.replace_all(&prefix, "#");
    normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn similarity(left: &str, right: &str) -> f64 {
    use std::collections::HashSet;
    let set = |text: &str| {
        let bytes = text.as_bytes();
        bytes
            .windows(3)
            .map(|s| [s[0], s[1], s[2]])
            .collect::<HashSet<_>>()
    };
    let left_grams = set(left);
    let right_grams = set(right);
    let union = left_grams.union(&right_grams).count();
    let jaccard = if union == 0 {
        if left == right { 1.0 } else { 0.0 }
    } else {
        left_grams.intersection(&right_grams).count() as f64 / union as f64
    };
    0.6 * strsim::jaro_winkler(left, right) + 0.4 * jaccard
}

/// A boot prefix extracted from a DUT's known-good serial capture.
pub(crate) struct BootReference {
    anchor: Vec<u8>,
    matched: usize,
    line_start: bool,
}

impl BootReference {
    pub(crate) fn same_anchor(&self, other: &Self) -> bool {
        self.anchor == other.anchor
    }

    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        if !path.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a regular file",
            ));
        }
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_REFERENCE_BYTES)
            .read_to_end(&mut bytes)?;
        Self::from_capture(&bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "reference contains no boot text",
            )
        })
    }

    pub(crate) fn from_capture(bytes: &[u8]) -> Option<Self> {
        const EARLY_BOOT: [&str; 7] = [
            "BootROM",
            "Boot ROM",
            "DDR Version",
            "DDR Init",
            "U-Boot TPL",
            "U-Boot SPL",
            "SPL board init",
        ];
        let text = String::from_utf8_lossy(bytes);
        let mut first = None;
        let mut stage = None;
        for line in text.split(['\r', '\n']) {
            let line = line
                .trim_matches(|ch: char| ch.is_whitespace() || ch.is_control() || ch == '\u{feff}');
            if !line.chars().any(char::is_alphanumeric) {
                continue;
            }
            first.get_or_insert(line);
            if let Some(prefix) = EARLY_BOOT
                .into_iter()
                .find(|prefix| line.starts_with(prefix))
            {
                stage = Some(prefix);
                break;
            }
            if line.starts_with("U-Boot ") || line.starts_with("Linux version") {
                break;
            }
        }
        let anchor = stage.or(first)?;
        let mut end = anchor.len().min(MAX_ANCHOR_BYTES);
        while !anchor.is_char_boundary(end) {
            end -= 1;
        }
        let anchor = anchor.as_bytes()[..end].to_vec();
        Some(Self {
            anchor,
            matched: 0,
            line_start: true,
        })
    }

    /// Return the bytes ready for disk and the offsets at which new cycles begin.
    /// The only delayed bytes are a possible suffix of the boot prefix.
    pub(crate) fn partition(&mut self, incoming: Vec<u8>) -> (Vec<u8>, Vec<usize>) {
        let prefix = self.matched;
        let mut data = if prefix == 0 {
            incoming
        } else {
            let mut data = Vec::with_capacity(prefix + incoming.len());
            data.extend_from_slice(&self.anchor[..prefix]);
            data.extend_from_slice(&incoming);
            data
        };
        let mut boundaries = Vec::new();
        for (index, &byte) in data.iter().enumerate().skip(prefix) {
            if byte == self.anchor[self.matched] && (self.line_start || self.matched > 0) {
                self.matched += 1;
                if self.matched == self.anchor.len() {
                    boundaries.push(index + 1 - self.anchor.len());
                    self.matched = 0;
                }
            } else {
                self.matched = 0;
            }
            if matches!(byte, b'\n' | b'\r') {
                self.line_start = true;
            } else if byte.is_ascii_graphic() || !byte.is_ascii() {
                self.line_start = false;
            }
        }
        data.truncate(data.len() - self.matched);
        (data, boundaries)
    }

    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        self.anchor[..std::mem::take(&mut self.matched)].to_vec()
    }

    pub(crate) fn expect_new_line(&mut self) {
        self.line_start = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_prefix_survives_chunking_and_ignores_later_boot_stages() {
        let mut matcher = BootReference::from_capture(
            b"\r\nDDR Version 1.00\r\nU-Boot SPL build\r\nU-Boot 2025.01\nLinux version x\n",
        )
        .unwrap();
        assert_eq!(
            matcher.partition(b"previous\r\nDDR Ver".to_vec()),
            (b"previous\r\n".to_vec(), vec![])
        );
        assert_eq!(
            matcher.partition(b"sion 2.00\r\nU-Boot SPL\nDDR Version new\nD".to_vec()),
            (
                b"DDR Version 2.00\r\nU-Boot SPL\nDDR Version new\n".to_vec(),
                vec![0, 29]
            ),
        );
        assert_eq!(matcher.take_pending(), b"D");
    }

    #[test]
    fn learning_requires_every_pair_to_reach_ninety_five_percent() {
        let first =
            "DDR Version 1.01\nDDR training 0x011111aa\nU-Boot SPL\nBL31: entry\nStarting kernel";
        let second =
            "DDR Version 2.04\nDDR training 0x784343bb\nU-Boot SPL\nBL31: entry\nStarting kernel";
        let (reference, score) = evaluate(vec![first.into(), second.into()], 95).unwrap();
        assert_eq!(reference, second);
        assert_eq!(score, 1.0);
        let changed = vec![
            first.into(),
            second.into(),
            format!("{second}\nOther logged content"),
        ];
        assert!(evaluate(changed.clone(), 95).is_err());
        assert!(evaluate(changed, 50).is_ok());
    }

    #[test]
    fn diagnostic_messages_with_the_prefix_mid_line_do_not_rotate() {
        let mut matcher = BootReference::from_capture(b"DDR Version 1\nU-Boot SPL\n").unwrap();
        let (data, boundaries) = matcher.partition(b"echo DDR Version 1\nDDR Version 2\n".to_vec());
        assert_eq!(data, b"echo DDR Version 1\nDDR Version 2\n");
        assert_eq!(boundaries, vec![19]);
    }
}
