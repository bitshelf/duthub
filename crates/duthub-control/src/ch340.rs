//! The 4-byte serial port protocol of the CH340-class USB relay.
//!
//! For details on the protocol implementation, refer to `bitshelf/sermcp`
//! (sermcp/dut-ctrl/src/ch340.rs). This is a commonly used implementation across various
//! communities.
//!
//! - Command frame: 4 bytes: `[0xA0, channel, opcode, checksum]`
//!   where `checksum = (0xA0 + channel + opcode) & 0xFF`;
//! - Channel numbering starts at 1.
//! - Opcodes: `0x00` OFF, `0x01` ON, `0x05` STATUS;
//! - Most cards echo the same frame for ON/OFF status (used for read-back verification);
//! - STATUS response frame `[0xA0, channel, state, checksum]` (state is only 0/1; the state byte
//!   is located at the opcode byte, and the checksum covers `0xA0 + channel + state`);
//! - Fixed baud rate of 9600, 8N1 (factory default on the relay board; not configurable).

/// Frame header.
pub const HEADER: u8 = 0xA0;
/// Disconnect (release).
pub const OP_OFF: u8 = 0x00;
/// Energize (pressing down).
pub const OP_ON: u8 = 0x01;
/// querying the status of the channel.
pub const OP_STATUS: u8 = 0x05;
/// The baud rate for the CH340 relay board upon manufacture.
pub const DEFAULT_BAUD: u32 = 9600;
/// The upper limit for channel enumeration using `open()` method (common for boards with 1-8
/// channels).
pub const MAX_CHANNELS: u8 = 8;

/// Construct a 4-byte command frame.
#[must_use]
pub fn frame(channel: u8, opcode: u8) -> [u8; 4] {
    [HEADER, channel, opcode, checksum(channel, opcode)]
}

/// Frame check sum: `(0xA0 + channel + opcode) & 0xFF`.
#[must_use]
pub fn checksum(channel: u8, opcode: u8) -> u8 {
    HEADER.wrapping_add(channel).wrapping_add(opcode)
}

/// STATUS is the checksum of the response frame (the status byte occupies the opcode bits).
fn status_checksum(channel: u8, state: u8) -> u8 {
    checksum(channel, state)
}

/// Read the frame classification results again (refer to sermcp `EchoCheck`).
#[derive(Debug, PartialEq, Eq)]
pub enum EchoCheck<'a> {
    /// The result is consistent with the expected channel/operation code, and the verification
    /// process was successful.
    Verified(&'a [u8]),
    /// Channel/operation code match, but there are errors in the verification bytes. The content
    /// cannot be trusted.
    ChecksumBad(&'a [u8]),
    /// A 4-byte frame, but the channel/operation code does not match (noise from the other end or
    /// incompatible protocols).
    Mismatch(&'a [u8]),
    /// Less than 4 bytes after the frame header — it may just be a read of a fragment, and does not
    /// count as a protocol violation.
    Truncated(&'a [u8]),
    /// There are no frame headers in the buffer (silent board).
    Absent,
}

/// Scanning for the desired echo frame in the read-back buffer (attempting from each 0xA0
/// position).
#[must_use]
pub fn classify_echo(buf: &[u8], channel: u8, opcode: u8) -> EchoCheck<'_> {
    let mut fallback = EchoCheck::Absent;
    for start in 0..buf.len() {
        if buf[start] != HEADER {
            continue;
        }
        let tail = &buf[start..];
        if tail.len() < 4 {
            if matches!(fallback, EchoCheck::Absent) {
                fallback = EchoCheck::Truncated(tail);
            }
            continue;
        }
        let candidate = &buf[start..start + 4];
        if candidate[1] == channel && candidate[2] == opcode {
            if candidate[3] == checksum(channel, opcode) {
                return EchoCheck::Verified(candidate);
            }
            if matches!(fallback, EchoCheck::Absent | EchoCheck::Truncated(_)) {
                fallback = EchoCheck::ChecksumBad(candidate);
            }
        } else if matches!(fallback, EchoCheck::Absent | EchoCheck::Truncated(_)) {
            fallback = EchoCheck::Mismatch(candidate);
        }
    }
    fallback
}

/// Scan the status frames of the specified channel in the STATUS response buffer.
///
/// It only accepts frames with a "frame header + channel match + status byte 0/1 + verification
/// passed". In serial port noise, bytes that resemble the expected ones (such as text bytes
/// 0x50/0x72) will not be misreported as channel status. Returning `None` indicates that no valid
/// response was found (the board does not respond to STATUS requests, or the data is corrupted).
#[must_use]
pub fn scan_status(buf: &[u8], channel: u8) -> Option<u8> {
    if buf.len() < 4 {
        return None;
    }
    for pos in 0..=buf.len() - 4 {
        let f = &buf[pos..pos + 4];
        if f[0] == HEADER
            && f[1] == channel
            && f[2] <= 0x01
            && f[3] == status_checksum(channel, f[2])
        {
            return Some(f[2]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout_matches_protocol() {
        // ch1 ON: 0xA0 01 01. Verification: (0xA0 + 1 + 1) = 0xA2
        assert_eq!(frame(1, OP_ON), [0xA0, 0x01, 0x01, 0xA2]);
        // ch1 OFF: Verification (0xA0+1+0) = 0xA1
        assert_eq!(frame(1, OP_OFF), [0xA0, 0x01, 0x00, 0xA1]);
        // CH4 STATUS: Verification (0xA0 + 4 + 5) = 0xA9
        assert_eq!(frame(4, OP_STATUS), [0xA0, 0x04, 0x05, 0xA9]);
    }

    #[test]
    fn checksum_wraps_at_255() {
        // 0xA0 + 0x40 + 0x40 = 0x120 → 0x20
        assert_eq!(checksum(0x40, 0x40), 0x20);
    }

    #[test]
    fn classify_echo_verifies_exact_frame() {
        let buf = frame(2, OP_ON);
        assert_eq!(classify_echo(&buf, 2, OP_ON), EchoCheck::Verified(&buf[..]));
    }

    #[test]
    fn classify_echo_finds_frame_after_noise() {
        let mut buf = b"noise\r\n".to_vec();
        buf.extend_from_slice(&frame(3, OP_OFF));
        assert!(matches!(
            classify_echo(&buf, 3, OP_OFF),
            EchoCheck::Verified(_)
        ));
    }

    #[test]
    fn classify_echo_flags_bad_checksum() {
        let buf = [HEADER, 1, OP_ON, 0xFF];
        assert!(matches!(
            classify_echo(&buf, 1, OP_ON),
            EchoCheck::ChecksumBad(_)
        ));
    }

    #[test]
    fn classify_echo_reports_channel_mismatch() {
        let buf = frame(3, OP_ON);
        assert!(matches!(
            classify_echo(&buf, 1, OP_ON),
            EchoCheck::Mismatch(_)
        ));
    }

    #[test]
    fn classify_echo_labels_partial_frame_as_truncated() {
        let buf = [0x10, HEADER, 1, OP_ON];
        assert!(matches!(
            classify_echo(&buf, 1, OP_ON),
            EchoCheck::Truncated(_)
        ));
    }

    #[test]
    fn classify_echo_absent_without_header() {
        assert_eq!(classify_echo(b"banner\r\n", 1, OP_ON), EchoCheck::Absent);
    }

    #[test]
    fn scan_status_reads_state_byte() {
        let buf = [HEADER, 2, 0x01, status_checksum(2, 0x01)];
        assert_eq!(scan_status(&buf, 2), Some(0x01));
    }

    #[test]
    fn scan_status_ignores_query_echo_and_text_bytes() {
        // The board card first echoes the query frame (opcode = 0x05), and then sends a status
        // frame; mixed text bytes
        let mut buf = b"ser2net-ish text \x50\x72".to_vec();
        buf.extend_from_slice(&frame(2, OP_STATUS)); // Echo query frame: opcode=5 > 0x01, must be ignored
        buf.extend_from_slice(&[HEADER, 2, 0x00, status_checksum(2, 0x00)]);
        assert_eq!(scan_status(&buf, 2), Some(0x00));
    }

    #[test]
    fn scan_status_rejects_bad_checksum_and_other_channel() {
        let buf = [HEADER, 2, 0x01, 0xFF];
        assert_eq!(scan_status(&buf, 2), None);
        let buf = [HEADER, 3, 0x01, status_checksum(3, 0x01)];
        assert_eq!(scan_status(&buf, 2), None);
        let buf = [HEADER, 2, 0x72, status_checksum(2, 0x72)];
        assert_eq!(
            scan_status(&buf, 2),
            None,
            "The text bytes should not be misreported as states."
        );
    }
}
