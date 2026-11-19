//! CH340 类 USB 继电器的 4 字节串口协议。
//!
//! 协议参考 `bitshelf/sermcp`（sermcp/dut-ctrl/src/ch340.rs）与社区通用实现：
//!
//! - 命令帧 4 字节：`[0xA0, channel, opcode, checksum]`，
//!   `checksum = (0xA0 + channel + opcode) & 0xFF`；
//! - 通道从 1 开始编号；
//! - opcode：`0x00` OFF、`0x01` ON、`0x05` STATUS；
//! - 多数板卡对 ON/OFF 原样回显同一帧（用于读回校验）；
//! - STATUS 应答帧 `[0xA0, channel, state, checksum]`（state 仅 0/1，
//!   状态字节落在 opcode 位，校验和覆盖 A0+channel+state）；
//! - 波特率固定 9600 8N1（板卡出厂即此，无配置项）。

/// 帧头。
pub const HEADER: u8 = 0xA0;
/// 断开（释放）。
pub const OP_OFF: u8 = 0x00;
/// 吸合（按下）。
pub const OP_ON: u8 = 0x01;
/// 查询通道状态。
pub const OP_STATUS: u8 = 0x05;
/// CH340 继电器板出厂波特率。
pub const DEFAULT_BAUD: u32 = 9600;
/// `open()` 通道枚举的探测上限（常见板卡 1-8 通道）。
pub const MAX_CHANNELS: u8 = 8;

/// 构造 4 字节命令帧。
pub fn frame(channel: u8, opcode: u8) -> [u8; 4] {
    [HEADER, channel, opcode, checksum(channel, opcode)]
}

/// 帧校验和：`(0xA0 + channel + opcode) & 0xFF`。
pub fn checksum(channel: u8, opcode: u8) -> u8 {
    HEADER.wrapping_add(channel).wrapping_add(opcode)
}

/// STATUS 应答帧的校验和（状态字节占 opcode 位）。
fn status_checksum(channel: u8, state: u8) -> u8 {
    checksum(channel, state)
}

/// 读回帧分类结果（参考 sermcp `EchoCheck`）。
#[derive(Debug, PartialEq, Eq)]
pub enum EchoCheck<'a> {
    /// 与期望通道/操作码完全一致且校验通过。
    Verified(&'a [u8]),
    /// 通道/操作码匹配但校验字节错误——内容不可信。
    ChecksumBad(&'a [u8]),
    /// 4 字节帧但通道/操作码不符（对端噪声或异协议）。
    Mismatch(&'a [u8]),
    /// 帧头后不足 4 字节——可能只是读分片，不算协议违规。
    Truncated(&'a [u8]),
    /// 缓冲中没有任何帧头（静默板卡）。
    Absent,
}

/// 在读回缓冲中扫描期望的回显帧（从每个 0xA0 位置起尝试）。
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

/// 在 STATUS 应答缓冲中扫描指定通道的状态帧。
///
/// 只接受「帧头 + 通道匹配 + 状态字节 0/1 + 校验通过」的帧；串口噪声里
/// 恰好形似的字节（如文本字节 0x50/0x72）不会误报为通道状态。
/// 返回 `None` 表示未找到有效应答（板卡不回答 STATUS 或数据损坏）。
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
        // ch1 ON：0xA0 01 01，校验 (0xA0+1+1)=0xA2
        assert_eq!(frame(1, OP_ON), [0xA0, 0x01, 0x01, 0xA2]);
        // ch1 OFF：校验 (0xA0+1+0)=0xA1
        assert_eq!(frame(1, OP_OFF), [0xA0, 0x01, 0x00, 0xA1]);
        // ch4 STATUS：校验 (0xA0+4+5)=0xA9
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
        // 板卡先回显查询帧（opcode=0x05），再回状态帧；混杂文本字节
        let mut buf = b"ser2net-ish text \x50\x72".to_vec();
        buf.extend_from_slice(&frame(2, OP_STATUS)); // 回显查询帧：opcode=5 > 0x01，必须被忽略
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
        assert_eq!(scan_status(&buf, 2), None, "文本字节不得误报为状态");
    }
}
