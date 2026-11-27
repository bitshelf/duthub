//! Logical terminal key encoding and ANSI screen-text cleanup for MCP serial tools.
//!
//! Logical keys are validated as a complete sequence before any byte is written
//! to the serial port so an unknown key name never transmits partial control codes.

/// Maximum number of logical keys accepted in a single `serial_exec` call.
pub const MAX_KEYS_PER_CALL: usize = 64;

/// Encode a validated sequence of logical key names into raw terminal bytes.
///
/// Validation runs across the full slice before returning; any unsupported key
/// fails the whole batch with a descriptive error.
pub fn encode_logical_keys(keys: &[String]) -> Result<Vec<u8>, String> {
    if keys.len() > MAX_KEYS_PER_CALL {
        return Err(format!(
            "keys exceeds maximum of {MAX_KEYS_PER_CALL} entries per call"
        ));
    }
    let mut out = Vec::with_capacity(keys.len() * 3);
    for raw in keys {
        let normalized = raw.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err("logical key name cannot be empty".into());
        }
        match normalized.as_str() {
            "enter" | "return" | "cr" => out.push(b'\r'),
            "tab" => out.push(b'\t'),
            "space" => out.push(b' '),
            "esc" | "escape" => out.push(0x1b),
            "backspace" | "bs" => out.push(0x7f),
            "up" => out.extend_from_slice(b"\x1b[A"),
            "down" => out.extend_from_slice(b"\x1b[B"),
            "right" => out.extend_from_slice(b"\x1b[C"),
            "left" => out.extend_from_slice(b"\x1b[D"),
            "home" => out.extend_from_slice(b"\x1b[H"),
            "end" => out.extend_from_slice(b"\x1b[F"),
            other => {
                if let Some(ctrl_ch) = other.strip_prefix("ctrl+") {
                    let bytes = ctrl_ch.as_bytes();
                    if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
                        out.push((bytes[0] - b'a') + 1);
                        continue;
                    }
                }
                return Err(format!(
                    "unsupported logical key '{raw}'; allowed keys: enter, tab, space, esc, backspace, up, down, left, right, home, end, ctrl+a..ctrl+z"
                ));
            }
        }
    }
    Ok(out)
}

/// Strip ANSI escape sequences (colors, highlights, cursor motion, OSC titles)
/// while preserving raw chunk whitespace and newlines.
#[must_use]
pub fn strip_ansi_codes(input: &str) -> String {
    if !input.as_bytes().contains(&0x1b) {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut cleaned = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
                    // CSI sequence: ESC [ ... <final 0x40..=0x7E>
                    i += 1;
                    while i < bytes.len() {
                        let b = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&b) {
                            break;
                        }
                    }
                }
                b']' => {
                    // OSC sequence: ESC ] ... (BEL | ST)
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    // Two-byte escape sequence.
                    i += 1;
                }
            }
            continue;
        }
        // Preserve valid UTF-8 character starting at byte `i`.
        let rest = &input[i..];
        if let Some(ch) = rest.chars().next() {
            cleaned.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    cleaned
}

/// Strip ANSI escape sequences and collapse carriage-return progress overwrites.
///
/// Used as a clean fallback when a session has no live VT100 `SerialScreen`
/// snapshot (such as demo sessions or persisted snapshots) and when matching
/// wait patterns against colored serial streams.
#[must_use]
pub fn strip_ansi_escapes(input: &str) -> String {
    let cleaned = strip_ansi_codes(input);

    // Resolve in-line carriage return overwrites (e.g. progress bars) line by line.
    let mut lines: Vec<&str> = cleaned
        .split('\n')
        .map(|line| {
            let without_trailing_cr = line.strip_suffix('\r').unwrap_or(line);
            without_trailing_cr
                .rsplit('\r')
                .next()
                .unwrap_or(without_trailing_cr)
                .trim_end()
        })
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_all_supported_logical_keys() {
        let keys: Vec<String> = [
            "ctrl+c",
            "CTRL+D",
            "ctrl+z",
            "ctrl+l",
            "esc",
            "enter",
            "tab",
            "space",
            "backspace",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let encoded = encode_logical_keys(&keys).expect("all keys valid");
        let expected: Vec<u8> = [
            &[0x03u8][..],
            &[0x04],
            &[0x1a],
            &[0x0c],
            &[0x1b],
            b"\r",
            b"\t",
            b" ",
            &[0x7f],
            b"\x1b[A",
            b"\x1b[B",
            b"\x1b[D",
            b"\x1b[C",
            b"\x1b[H",
            b"\x1b[F",
        ]
        .concat();
        assert_eq!(encoded, expected);
    }

    #[test]
    fn rejects_invalid_key_before_emitting_any_bytes() {
        let keys = vec!["ctrl+c".into(), "alt+f4".into(), "enter".into()];
        let err = encode_logical_keys(&keys).expect_err("unknown key must fail entire batch");
        assert!(err.contains("alt+f4"));
    }

    #[test]
    fn strips_ansi_and_progress_overwrites() {
        let raw = "\x1b[32mBooting...\x1b[0m\r\n10%\r50%\r100% done   \r\n\n";
        assert_eq!(strip_ansi_escapes(raw), "Booting...\n100% done");
    }

    #[test]
    fn strip_ansi_codes_removes_highlights_and_preserves_chunk_spacing() {
        let colored = "\x1b[1;32mconsole:/\x1b[0m $ \x1b]0;title\x07\x1b[Kls\r\n";
        assert_eq!(strip_ansi_codes(colored), "console:/ $ ls\r\n");
        assert_eq!(strip_ansi_codes("plain text "), "plain text ");
    }
}
