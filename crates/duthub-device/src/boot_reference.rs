use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use duthub_core::ipc::{ChunkStyle, EventEnvelope, UiEvent};
use regex::Regex;
use tokio::sync::broadcast;

const MAX_REFERENCE_BYTES: u64 = 256 * 1024;
const MAX_ANCHOR_BYTES: usize = 256;
const LEARN_CYCLES: usize = 3;
// Compare the early firmware prefix to avoid noise from concurrent kernel init.
const COMPARE_LINES: usize = 40;
const COMPARE_BYTES: usize = 8 * 1024;
const REFERENCE_LINES: usize = 100;
const MIN_LEARNED_LINES: usize = 5;
const POWER_OFF_DELAY: Duration = Duration::from_secs(3);
const QUIET_WINDOW: Duration = Duration::from_millis(1_200);
const QUIET_WAIT_CAP: Duration = Duration::from_secs(5);
const CAPTURE_WINDOW: Duration = Duration::from_secs(45);

static ROCKCHIP_DDR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(DDR [[:xdigit:]]{8,})\b[^\r\n]*\bfwver:")
        .expect("static Rockchip DDR banner regex is valid")
});

/// Learn from GUI-confirmed reboot cycles (Power Supply preferred, or Reset).
/// All actions still go through the normal relay service so device ownership,
/// approval and audit rules apply.
pub fn learn(
    device: &crate::DeviceService,
    control: &crate::control::ControlService,
    dut_id: &str,
    minimum_percent: u8,
) -> Result<(String, f64), String> {
    if !(1..=100).contains(&minimum_percent) {
        return Err("Minimum similarity must be an integer between 1 and 100".into());
    }
    let use_power = control.power_configured(dut_id);
    let use_reset = control.reset_configured(dut_id);
    if !use_power && !use_reset {
        return Err("Power or reset control is required for boot learning".into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|error| error.to_string())?;
    let mut samples = Vec::with_capacity(LEARN_CYCLES);
    for cycle in 1..=LEARN_CYCLES {
        if !device.real_session_opened(dut_id) {
            return Err("The serial connection is closed".into());
        }
        let mut events = if use_power {
            let mut off_events = device.subscribe();
            control
                .execute_confirmed_power(dut_id, false)
                .map_err(|error| error.to_string())?;
            // Wait for power-off drain + serial silence before opening the new log file.
            std::thread::sleep(POWER_OFF_DELAY);
            runtime.block_on(wait_serial_quiet(
                &mut off_events,
                dut_id,
                QUIET_WINDOW,
                QUIET_WAIT_CAP,
            ));
            let _ = device.log_control_boundary(
                dut_id,
                &format!("Boot learning {cycle}/{LEARN_CYCLES}"),
                true,
            );
            let events = device.subscribe();
            if let Err(error) = control.execute_confirmed_power(dut_id, true) {
                return match control.execute_confirmed_power(dut_id, true) {
                    Ok(()) => Err(format!(
                        "Cycle {cycle}: power-on failed, but power was restored: {error}"
                    )),
                    Err(recovery) => Err(format!(
                        "Cycle {cycle}: power-on failed ({error}), and restoring power failed: {recovery}"
                    )),
                };
            }
            events
        } else {
            let _ = device.log_control_boundary(
                dut_id,
                &format!("Boot learning {cycle}/{LEARN_CYCLES}"),
                true,
            );
            let events = device.subscribe();
            control
                .execute_confirmed_reset(dut_id)
                .map_err(|error| error.to_string())?;
            events
        };
        let sample = runtime.block_on(capture_boot(&mut events, dut_id, CAPTURE_WINDOW))?;
        if sample
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            < MIN_LEARNED_LINES
        {
            return Err(format!(
                "Cycle {cycle}: not enough serial boot text to calculate similarity"
            ));
        }
        samples.push(sample);
    }
    evaluate(samples, minimum_percent)
}

async fn capture_boot(
    events: &mut broadcast::Receiver<EventEnvelope>,
    dut_id: &str,
    window: Duration,
) -> Result<String, String> {
    let mut raw = String::new();
    let max_bytes = usize::try_from(MAX_REFERENCE_BYTES).unwrap_or(usize::MAX);
    let mut scanned_bytes = 0usize;
    let mut capture_complete = false;
    let receive = async {
        // Keep the DUT running for the whole window, even if the reference is already full.
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(_)) if capture_complete => continue,
                Err(error) => return Err(error.to_string()),
            };
            if event.dut_id.as_deref() != Some(dut_id) {
                continue;
            }
            match event.event {
                UiEvent::SerialChunk {
                    text,
                    style: ChunkStyle::Normal,
                } if !capture_complete => {
                    let end = text.floor_char_boundary(text.len().min(max_bytes - raw.len()));
                    raw.push_str(&text[..end]);
                    let mut reached_kernel = false;
                    if let Some(last_nl) = raw[scanned_bytes..].rfind('\n') {
                        let complete_end = scanned_bytes + last_nl + 1;
                        reached_kernel = raw[scanned_bytes..complete_end]
                            .lines()
                            .any(is_kernel_boundary);
                        scanned_bytes = complete_end;
                    }
                    capture_complete = end < text.len() || raw.len() == max_bytes || reached_kernel;
                }
                UiEvent::SerialDisconnected { reason } => return Err::<(), _>(reason),
                _ => {}
            }
        }
    };
    if let Ok(result) = tokio::time::timeout(window, receive).await {
        result?;
    }
    let complete = raw.rsplit_once('\n').map_or("", |(lines, _)| lines);
    Ok(reference_window(complete))
}

async fn wait_serial_quiet(
    events: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
    dut_id: &str,
    quiet_window: Duration,
    max_wait: Duration,
) {
    let deadline = tokio::time::Instant::now() + max_wait;
    let mut quiet_deadline = tokio::time::Instant::now() + quiet_window;
    loop {
        let now = tokio::time::Instant::now();
        let target = deadline.min(quiet_deadline);
        let step = target.saturating_duration_since(now);
        if step.is_zero() {
            return;
        }
        match tokio::time::timeout(step, events.recv()).await {
            Ok(Ok(event)) => {
                if event.dut_id.as_deref() == Some(dut_id)
                    && matches!(
                        event.event,
                        UiEvent::SerialChunk {
                            style: ChunkStyle::Normal,
                            ..
                        }
                    )
                {
                    quiet_deadline = tokio::time::Instant::now() + quiet_window;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                quiet_deadline = tokio::time::Instant::now() + quiet_window;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => return,
        }
    }
}

fn is_kernel_boundary(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    line.contains("starting kernel") || line.contains("start kernel")
}

fn reference_window(text: &str) -> String {
    // A reset can leave bytes from the preceding boot in front of the new banner.
    let mut remaining = text;
    let mut boot = text;
    for line in text.split_inclusive('\n') {
        if let Some(start) = early_boot_anchor(line).and_then(|anchor| line.find(anchor)) {
            boot = &remaining[start..];
            break;
        }
        remaining = &remaining[line.len()..];
    }
    let mut lines = Vec::new();
    for line in boot.trim().lines().take(REFERENCE_LINES) {
        lines.push(line);
        if is_kernel_boundary(line) {
            break;
        }
    }
    lines.join("\n")
}

fn early_boot_anchor(line: &str) -> Option<&str> {
    const EARLY_BOOT: [&str; 7] = [
        "BootROM",
        "Boot ROM",
        "DDR Version",
        "DDR Init",
        "U-Boot TPL",
        "U-Boot SPL",
        "SPL board init",
    ];
    let trimmed = line
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch.is_control() || ch == '\u{feff}');
    EARLY_BOOT
        .into_iter()
        .find(|prefix| trimmed.starts_with(prefix))
        .or_else(|| {
            ROCKCHIP_DDR
                .captures(line)
                .and_then(|captures| captures.get(1))
                .map(|anchor| anchor.as_str())
        })
}

fn evaluate(samples: Vec<String>, minimum_percent: u8) -> Result<(String, f64), String> {
    if samples.len() < 2 {
        return Err("At least two valid boot samples are required".into());
    }
    let normalized: Vec<_> = samples.iter().map(|sample| normalize(sample)).collect();
    if normalized.iter().any(String::is_empty) {
        return Err("At least one boot sample is empty".into());
    }
    let mut weakest = 1.0_f64;
    for (index, sample) in normalized.iter().enumerate() {
        for other in &normalized[index + 1..] {
            weakest = weakest.min(similarity(sample, other));
        }
    }
    if weakest < f64::from(minimum_percent) / 100.0 {
        return Err(format!(
            "Lowest similarity {:.2}% is below the required {minimum_percent}%",
            weakest * 100.0
        ));
    }
    Ok((
        samples.into_iter().last().expect("validated sample count"),
        weakest,
    ))
}

fn normalize(sample: &str) -> String {
    static ANSI: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("static ANSI regex literal is valid")
    });
    static VOLATILE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\[\s*\d+\.\d+\](?:\[\s*T\d+\])?|\b0x[0-9a-f]+\b|\b[0-9a-f]{8,}\b|\b\d+(?:\.\d+)?\b")
            .expect("static volatile-token regex literal is valid")
    });
    static DDR_TRAINING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(DQ roc:|DQ rds:|DQS roc:)[\s,\x08]*(?:[pnhl][ \t\r\n]*\d+[\s,\x08]*)+")
            .expect("static DDR training regex is valid")
    });
    let prefix = &sample[..sample.floor_char_boundary(sample.len().min(COMPARE_BYTES))];
    let prefix = ANSI.replace_all(prefix, "").replace('\0', "");
    let prefix = if prefix
        .lines()
        .next()
        .is_some_and(|line| ROCKCHIP_DDR.is_match(line))
    {
        // The training measurements and their line wraps can vary on every boot.
        DDR_TRAINING.replace_all(&prefix, "${1} #\n")
    } else {
        std::borrow::Cow::Borrowed(prefix.as_str())
    };
    let prefix = prefix
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(COMPARE_LINES)
        .collect::<Vec<_>>()
        .join("\n");
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
        #[expect(
            clippy::cast_precision_loss,
            reason = "trigram counts over bounded 8 KiB prefix fit in f64 mantissa"
        )]
        let ratio = left_grams.intersection(&right_grams).count() as f64 / union as f64;
        ratio
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
            if let Some(prefix) = early_boot_anchor(line) {
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

    #[tokio::test]
    async fn learning_keeps_collecting_after_the_first_forty_lines() {
        let bus = crate::bus::EventBus::new(16);
        let mut events = bus.subscribe();
        let banner = "DDR 2f85f4b2d4 cym 25/02/05-15:01.44,fwver: v1.09";
        let prefix = format!("{banner}\r\n{}", "DDR training\r\n".repeat(45));
        bus.publish(
            "session",
            "dut",
            crate::bus::chunk(prefix, ChunkStyle::Normal),
        );

        let capture = capture_boot(&mut events, "dut", Duration::from_millis(100));
        tokio::pin!(capture);
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut capture)
                .await
                .is_err()
        );
        bus.publish(
            "session",
            "other",
            crate::bus::chunk("Other DUT\n", ChunkStyle::Normal),
        );
        bus.publish(
            "session",
            "dut",
            crate::bus::chunk("U-Boot\r\nStarting ker", ChunkStyle::Normal),
        );
        bus.publish(
            "session",
            "dut",
            crate::bus::chunk("nel ...\r\nLinux init\r\n", ChunkStyle::Normal),
        );

        let sample = capture.await.unwrap();
        assert_eq!(sample.lines().count(), 48);
        assert!(sample.ends_with("U-Boot\nStarting kernel ..."));
    }

    #[tokio::test]
    async fn capture_boot_ignores_broadcast_lag_after_kernel_boundary() {
        let bus = crate::bus::EventBus::new(4);
        let mut events = bus.subscribe();
        bus.publish(
            "session",
            "dut",
            crate::bus::chunk(
                "DDR Version 1.0\r\nU-Boot SPL\r\nStarting kernel ...\r\n",
                ChunkStyle::Normal,
            ),
        );

        let capture = capture_boot(&mut events, "dut", Duration::from_millis(80));
        tokio::pin!(capture);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut capture)
                .await
                .is_err()
        );
        // Flood past the ring capacity while the capture window finishes.
        for index in 0..32 {
            bus.publish(
                "session",
                "dut",
                crate::bus::chunk(
                    format!("[ {index}.000000] kernel noise\r\n"),
                    ChunkStyle::Normal,
                ),
            );
        }

        let sample = capture.await.unwrap();
        assert!(sample.starts_with("DDR Version 1.0"));
        assert!(sample.ends_with("Starting kernel ..."));
    }

    #[test]
    fn learning_aligns_rockchip_samples_after_a_previous_boot_tail() {
        let banner = "DDR 2f85f4b2d4 cym 25/02/05-15:01.44,fwver: v1.09";
        let firmware = format!("{banner}\nIn\nDRAM init\nU-Boot\nStarting kernel ...\n");
        let first = reference_window(&firmware);
        let second = reference_window(&format!(
            "SPL: A/B-slot: _a\r\nChecking image ... OK\u{fffd}{firmware}"
        ));
        assert_eq!(first, second);
        let (reference, score) = evaluate(vec![first, second], 95).unwrap();
        assert_eq!(score, 1.0);
        let mut matcher = BootReference::from_capture(reference.as_bytes()).unwrap();
        let (_, boundaries) = matcher.partition(format!("prior output\n{firmware}").into_bytes());
        assert_eq!(boundaries, vec!["prior output\n".len()]);
    }

    #[test]
    fn reference_learned_at_ninety_percent_finds_each_boot_boundary() {
        let first = "DDR Version 1.01\nDDR training 0x011111aa\nU-Boot SPL\nBL31: entry\nBoot mode: normal\nStarting kernel ...\n";
        let second = first.replace("normal", "manual");
        let (reference, score) = evaluate(vec![first.into(), second.clone()], 90).unwrap();
        assert!((0.90..0.95).contains(&score), "Similarity was {score}");

        let mut matcher = BootReference::from_capture(reference.as_bytes()).unwrap();
        let raw = format!("{first}{second}").into_bytes();
        let (partitioned, boundaries) = matcher.partition(raw.clone());
        assert_eq!(boundaries, vec![0, first.len()]);
        assert_eq!(partitioned, raw);
    }

    #[test]
    fn learning_ignores_rockchip_training_values_and_retains_boot_diagnostics() {
        let banner = "DDR 2f85f4b2d4 cym 25/02/05-15:01.44,fwver: v1.09";
        let first = format!(
            "{banner}\nDQ roc:\np0 n7, p2 n0,\nDQ rds:h2 l0\x08, l0\nDQS roc: p1, n0\nDRAM diagnostic"
        );
        let second = format!(
            "{banner}\nDQ roc:\np1 n\n2, p0 n3,\nDQ rds:l0 h1 l0\nDQS roc: p3, n0\nDRAM diagnostic"
        );
        assert!(normalize(&first).ends_with("dram diagnostic"));
        let (reference, score) = evaluate(vec![first, second.clone()], 95).unwrap();
        assert_eq!(reference, second);
        assert_eq!(score, 1.0);
    }

    #[test]
    fn diagnostic_messages_with_the_prefix_mid_line_do_not_rotate() {
        let mut matcher = BootReference::from_capture(b"DDR Version 1\nU-Boot SPL\n").unwrap();
        let (data, boundaries) = matcher.partition(b"echo DDR Version 1\nDDR Version 2\n".to_vec());
        assert_eq!(data, b"echo DDR Version 1\nDDR Version 2\n");
        assert_eq!(boundaries, vec![19]);
    }
}
