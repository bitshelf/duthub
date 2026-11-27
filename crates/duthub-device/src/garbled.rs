//! Garbled-byte detection for serial sessions (baud-rate mismatch heuristic).
//!
//! The judge runs on the raw RX byte stream, before UTF-8 decoding and before
//! the terminal emulator: `vte` swallows most C0/C1 control bytes, so screen
//! text under-reports garbling. Only unexpected C0 control bytes (plus DEL)
//! count; illegal UTF-8 and encoding heuristics are deliberately out of scope.
//!
//! Window: 4 KiB of recent bytes (recorded per RX block, evicted block by
//! block). Levels: `>= 30‰` unexpected control bytes enters `Watch`, `>= 60‰`
//! enters `Garbled`; recovery uses lower thresholds (`Garbled -> Watch` below
//! 30‰, `Watch -> Clean` below 15‰). Windows below 512 bytes are never judged.

use std::collections::VecDeque;

use duthub_core::ipc::{GarbleLevel, GarbledStats};

/// Sliding window size in bytes.
pub const WINDOW_BYTES: usize = 4096;
/// Enter `Watch` at this share of unexpected control bytes (per mille).
pub const WATCH_ENTER_PERMILLE: u16 = 30;
/// Enter `Garbled` at this share of unexpected control bytes (per mille).
pub const GARBLED_ENTER_PERMILLE: u16 = 60;
/// Drop from `Watch` back to `Clean` below this share (per mille).
pub const WATCH_EXIT_PERMILLE: u16 = 15;
/// Drop from `Garbled` back to `Watch` below this share (per mille).
pub const GARBLED_EXIT_PERMILLE: u16 = 30;
/// Windows shorter than this many bytes are not judged (counts still reported).
pub const MIN_WINDOW_BYTES: u32 = 512;

/// Whether a byte counts as an "unexpected control byte": C0 controls except
/// the text/format controls a healthy terminal stream legitimately carries,
/// plus DEL (0x7F).
#[must_use]
pub fn is_control_byte(b: u8) -> bool {
    match b {
        // BEL, BS, HT, LF, CR and ESC are normal in text/terminal output.
        0x07 | 0x08 | 0x09 | 0x0a | 0x0d | 0x1b => false,
        0x00..=0x1f | 0x7f => true,
        _ => false,
    }
}

/// Sliding-window meter over raw RX bytes. Disabled by default: `observe` is a
/// no-op and `stats` reports `Clean`.
#[derive(Debug, Clone, Default)]
pub struct GarbleMeter {
    enabled: bool,
    /// `(bytes, control)` per observed block, oldest first.
    chunks: VecDeque<(u32, u32)>,
    bytes: u32,
    control: u32,
    level: GarbleLevel,
}

impl GarbleMeter {
    /// Turns detection on or off. Any change resets the window and the level.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.reset();
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Samples one RX block. No-op while disabled.
    pub fn observe(&mut self, bytes: &[u8]) {
        if !self.enabled || bytes.is_empty() {
            return;
        }
        // A single block larger than the window only contributes its tail;
        // everything older is already outside the window.
        let sample = if bytes.len() > WINDOW_BYTES {
            self.reset();
            &bytes[bytes.len() - WINDOW_BYTES..]
        } else {
            bytes
        };
        let sample_len = u32::try_from(sample.len()).unwrap_or(u32::MAX);
        let control =
            u32::try_from(sample.iter().filter(|b| is_control_byte(**b)).count()).unwrap_or(u32::MAX);
        self.chunks.push_back((sample_len, control));
        self.bytes += sample_len;
        self.control += control;
        let max_window = u32::try_from(WINDOW_BYTES).unwrap_or(u32::MAX);
        while self.bytes > max_window {
            let Some((len, ctrl)) = self.chunks.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(len);
            self.control = self.control.saturating_sub(ctrl);
        }
        self.update_level();
    }

    /// Current statistics. A disabled meter reports the default (`Clean`).
    #[must_use]
    pub fn stats(&self) -> GarbledStats {
        if !self.enabled {
            return GarbledStats::default();
        }
        GarbledStats {
            level: self.level,
            control_permille: self.permille(),
            window_bytes: self.bytes,
        }
    }

    /// Clears the window and returns to `Clean` (keeps the enabled flag).
    pub fn reset(&mut self) {
        self.chunks.clear();
        self.bytes = 0;
        self.control = 0;
        self.level = GarbleLevel::Clean;
    }

    fn permille(&self) -> u16 {
        if self.bytes == 0 {
            return 0;
        }
        // u64 intermediates keep the arithmetic overflow-free even if the
        // window size grows later.
        u16::try_from((u64::from(self.control) * 1000) / u64::from(self.bytes)).unwrap_or(1000)
    }

    fn update_level(&mut self) {
        if self.bytes < MIN_WINDOW_BYTES {
            self.level = GarbleLevel::Clean;
            return;
        }
        let p = self.permille();
        self.level = match (self.level, p) {
            (_, p) if p >= GARBLED_ENTER_PERMILLE => GarbleLevel::Garbled,
            // Hysteresis: a Garbled stream only leaves the level once the
            // share drops below `GARBLED_EXIT_PERMILLE` (30‰), so a share
            // hovering in the 30–60‰ band does not flap back to Watch (and
            // re-trigger the entry alert) on every observation.
            (GarbleLevel::Garbled, p) if p >= GARBLED_EXIT_PERMILLE => GarbleLevel::Garbled,
            // p < GARBLED_EXIT_PERMILLE: one step down to Watch first;
            // `Watch -> Clean` needs another observation below
            // `WATCH_EXIT_PERMILLE` (hysteresis around 30‰/15‰).
            (GarbleLevel::Garbled, _) => GarbleLevel::Watch,
            (_, p) if p >= WATCH_ENTER_PERMILLE => GarbleLevel::Watch,
            (GarbleLevel::Watch, p) if p < WATCH_EXIT_PERMILLE => GarbleLevel::Clean,
            (current, _) => current,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_meter() -> GarbleMeter {
        let mut meter = GarbleMeter::default();
        meter.set_enabled(true);
        meter
    }

    /// `total` bytes: `control` counted NULs followed by `A` filler.
    fn chunk(total: usize, control: usize) -> Vec<u8> {
        let mut out = vec![0u8; control];
        out.resize(total, b'A');
        out
    }

    #[test]
    fn control_byte_classifier_excludes_text_controls() {
        for b in [0x07u8, 0x08, 0x09, 0x0a, 0x0d, 0x1b] {
            assert!(!is_control_byte(b), "0x{b:02x} 是正常的文本/终端控制符");
        }
        for b in [0x00u8, 0x01, 0x06, 0x0b, 0x0c, 0x0e, 0x1f, 0x7f] {
            assert!(is_control_byte(b), "0x{b:02x} 必须计入乱码");
        }
        for b in [b' ', b'0', b'A', b'~', 0x80, 0xff] {
            assert!(!is_control_byte(b), "0x{b:02x} 不是控制字节");
        }
    }

    #[test]
    fn ascii_text_stays_clean() {
        let mut meter = enabled_meter();
        let line = b"Linux version 6.6.0 (builder@lab) #1 SMP PREEMPT\r\n";
        for _ in 0..40 {
            meter.observe(line);
        }
        let stats = meter.stats();
        assert_eq!(stats.level, GarbleLevel::Clean);
        assert_eq!(stats.control_permille, 0);
        assert!(stats.window_bytes >= MIN_WINDOW_BYTES);
    }

    #[test]
    fn excluded_control_bytes_stay_clean() {
        let mut meter = enabled_meter();
        // BEL, BS, HT, CR, LF and an ANSI escape are legitimate.
        let text = b"\x07\x08\t\r\n\x1b[0m";
        for _ in 0..100 {
            meter.observe(text);
        }
        let stats = meter.stats();
        assert_eq!(stats.control_permille, 0);
        assert_eq!(stats.level, GarbleLevel::Clean);
    }

    #[test]
    fn del_counts_as_control() {
        let mut meter = enabled_meter();
        meter.observe(&[0x7f; MIN_WINDOW_BYTES as usize]);
        let stats = meter.stats();
        assert_eq!(stats.window_bytes, MIN_WINDOW_BYTES);
        assert_eq!(stats.control_permille, 1000);
        assert_eq!(stats.level, GarbleLevel::Garbled);
    }

    #[test]
    fn dense_control_bytes_reach_garbled() {
        let mut meter = enabled_meter();
        let pattern: Vec<u8> = (0x00u8..=0x1f)
            .filter(|b| is_control_byte(*b))
            .chain(std::iter::once(0x7f))
            .collect();
        let noisy: Vec<u8> = pattern.iter().copied().cycle().take(600).collect();
        meter.observe(&noisy);
        let stats = meter.stats();
        assert_eq!(stats.control_permille, 1000);
        assert_eq!(stats.level, GarbleLevel::Garbled);
    }

    #[test]
    fn thresholds_enter_watch_and_garbled_at_30_and_60() {
        let mut watch = enabled_meter();
        watch.observe(&chunk(1000, 30));
        assert_eq!(watch.stats().control_permille, 30);
        assert_eq!(watch.stats().level, GarbleLevel::Watch);

        let mut below = enabled_meter();
        below.observe(&chunk(1000, 29));
        assert_eq!(below.stats().control_permille, 29);
        assert_eq!(below.stats().level, GarbleLevel::Clean);

        let mut garbled = enabled_meter();
        garbled.observe(&chunk(1000, 60));
        assert_eq!(garbled.stats().control_permille, 60);
        assert_eq!(garbled.stats().level, GarbleLevel::Garbled);
    }

    #[test]
    fn hysteresis_exits_to_watch_then_clean() {
        let mut meter = enabled_meter();
        meter.observe(&chunk(600, 600));
        assert_eq!(meter.stats().level, GarbleLevel::Garbled);

        // Window turnover to a clean 4 KiB block: one step down to Watch even
        // though the share is already 0‰.
        meter.observe(&chunk(WINDOW_BYTES, 0));
        assert_eq!(meter.stats().window_bytes, u32::try_from(WINDOW_BYTES).unwrap());
        assert_eq!(meter.stats().control_permille, 0);
        assert_eq!(meter.stats().level, GarbleLevel::Watch);

        // A second clean round is needed for Watch -> Clean.
        meter.observe(&chunk(WINDOW_BYTES, 0));
        assert_eq!(meter.stats().level, GarbleLevel::Clean);
    }

    /// Regression (review): the Garbled exit edge is `< 30‰`, not `< 60‰`.
    /// A share inside the 30–60‰ band must keep the level at Garbled instead of
    /// flapping back to Watch (which would re-trigger the entry alert).
    #[test]
    fn garbled_stays_until_the_share_drops_below_30_permille() {
        let mut meter = enabled_meter();
        meter.observe(&chunk(1000, 60));
        assert_eq!(meter.stats().level, GarbleLevel::Garbled);

        // 118/2000 = 59‰: inside (30, 60), must stay Garbled.
        meter.observe(&chunk(1000, 58));
        assert_eq!(meter.stats().control_permille, 59);
        assert_eq!(meter.stats().level, GarbleLevel::Garbled);

        // 120/3000 = 40‰: still above the 30‰ exit edge.
        meter.observe(&chunk(1000, 2));
        assert_eq!(meter.stats().control_permille, 40);
        assert_eq!(meter.stats().level, GarbleLevel::Garbled);

        // 120/4000 = 30‰: the exit edge itself keeps Garbled.
        meter.observe(&chunk(1000, 0));
        assert_eq!(meter.stats().control_permille, 30);
        assert_eq!(meter.stats().level, GarbleLevel::Garbled);

        // Window turnover evicts the oldest block: 60/4000 = 15‰ < 30‰ -> Watch.
        meter.observe(&chunk(1000, 0));
        assert_eq!(meter.stats().control_permille, 15);
        assert_eq!(meter.stats().level, GarbleLevel::Watch);

        // 2/4000 = 0‰ < 15‰ -> Clean (second step of the hysteresis).
        meter.observe(&chunk(1000, 0));
        assert_eq!(meter.stats().control_permille, 0);
        assert_eq!(meter.stats().level, GarbleLevel::Clean);
    }

    #[test]
    fn watch_band_between_15_and_30_permille_does_not_oscillate() {
        let mut meter = enabled_meter();
        meter.observe(&chunk(1000, 30));
        assert_eq!(meter.stats().level, GarbleLevel::Watch);

        meter.observe(&chunk(1000, 20));
        assert_eq!(meter.stats().control_permille, 25);
        assert_eq!(meter.stats().level, GarbleLevel::Watch);

        // Evict the first block (30/1000) -> 20/4000 = 5‰, below the exit band.
        meter.observe(&chunk(3000, 0));
        assert_eq!(meter.stats().window_bytes, 4000);
        assert_eq!(meter.stats().control_permille, 5);
        assert_eq!(meter.stats().level, GarbleLevel::Clean);
    }

    #[test]
    fn window_evicts_old_chunks() {
        let mut meter = enabled_meter();
        meter.observe(&chunk(2048, 2048));
        assert_eq!(meter.stats().control_permille, 1000);
        assert_eq!(meter.stats().level, GarbleLevel::Garbled);

        // Two more blocks push the total past 4 KiB: the whole old
        // all-control block is evicted (eviction happens block by block).
        meter.observe(&chunk(2048, 0));
        meter.observe(&chunk(2048, 0));
        let stats = meter.stats();
        assert_eq!(stats.window_bytes, u32::try_from(WINDOW_BYTES).unwrap());
        assert_eq!(stats.control_permille, 0, "旧的高占比块必须被淘汰");
        assert_eq!(stats.level, GarbleLevel::Watch);
    }

    #[test]
    fn window_smaller_than_512_is_not_judged() {
        let mut meter = enabled_meter();
        meter.observe(&[0x00; MIN_WINDOW_BYTES as usize - 1]);
        let stats = meter.stats();
        assert_eq!(stats.window_bytes, 511);
        assert_eq!(stats.control_permille, 1000);
        assert_eq!(stats.level, GarbleLevel::Clean, "min window 前不判定");

        meter.observe(&[0x00; 1]);
        assert_eq!(meter.stats().window_bytes, MIN_WINDOW_BYTES);
        assert_eq!(meter.stats().level, GarbleLevel::Garbled);
    }

    #[test]
    fn oversized_chunk_keeps_only_the_tail() {
        let mut meter = enabled_meter();
        let mut payload = vec![0x00u8; WINDOW_BYTES];
        payload.extend_from_slice(&vec![b'A'; WINDOW_BYTES]);
        meter.observe(&payload);
        let stats = meter.stats();
        assert_eq!(stats.window_bytes, u32::try_from(WINDOW_BYTES).unwrap());
        assert_eq!(stats.control_permille, 0);
        assert_eq!(stats.level, GarbleLevel::Clean);
    }

    #[test]
    fn empty_blocks_are_ignored() {
        let mut meter = enabled_meter();
        meter.observe(b"");
        assert_eq!(meter.stats(), GarbledStats::default());
    }

    #[test]
    fn disabled_meter_returns_clean_after_reset() {
        let mut meter = enabled_meter();
        meter.observe(&[0x00; 600]);
        assert_eq!(meter.stats().level, GarbleLevel::Garbled);

        meter.set_enabled(false);
        assert!(!meter.enabled());
        assert_eq!(meter.stats(), GarbledStats::default());
        // No sampling while disabled.
        meter.observe(&[0x00; 600]);
        assert_eq!(meter.stats(), GarbledStats::default());

        // Re-enabling starts from a clean window (no stale residue).
        meter.set_enabled(true);
        let stats = meter.stats();
        assert_eq!(stats.level, GarbleLevel::Clean);
        assert_eq!(stats.window_bytes, 0);
    }

    #[test]
    fn reset_clears_counts() {
        let mut meter = enabled_meter();
        meter.observe(&[0x00; 600]);
        assert!(meter.stats().window_bytes > 0);

        meter.reset();
        let stats = meter.stats();
        assert_eq!(stats.level, GarbleLevel::Clean);
        assert_eq!(stats.control_permille, 0);
        assert_eq!(stats.window_bytes, 0);
        assert!(meter.enabled(), "reset 不得改变开关状态");
    }
}
