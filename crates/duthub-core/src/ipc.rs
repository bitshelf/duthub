//! Typed UI event contract (`docs/API_CONTRACT.md`).
//!
//! Event envelopes carry a monotonic sequence number and optional DUT/session
//! ownership. The serialized JSON shape is a stable contract: new fields must
//! provide defaults and tolerate unknown keys.

use serde::{Deserialize, Serialize};

use crate::backend::DutId;

/// Semantic style annotation for terminal text; the UI maps this to theme colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStyle {
    Normal,
    Dim,
    Good,
    Prompt,
    /// Kernel error line (`panic`/`oops`/`failed`), rendered with error accent.
    Error,
    /// Segment marker line (`[mark] ...`) emitted by log segmentation.
    Mark,
}

/// Terminal grid row snapshot: text plus optional RGB foreground/background colors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenRow {
    pub text: String,
    pub fg: Option<u32>,
    pub bg: Option<u32>,
}

/// Garbled-byte detection level (baud-rate mismatch heuristic over the raw
/// serial byte stream). The serialized spelling is a stable contract:
/// `"clean"` / `"watch"` / `"garbled"` (pinned by a unit test below).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum GarbleLevel {
    #[default]
    Clean,
    Watch,
    Garbled,
}

/// Garbled-byte statistics carried by every screen snapshot. All integers:
/// `ScreenSnapshot` derives `Eq`, so no floats are allowed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GarbledStats {
    pub level: GarbleLevel,
    /// Unexpected control bytes per mille of the sliding window.
    pub control_permille: u16,
    /// Number of raw bytes currently in the window.
    pub window_bytes: u32,
}

/// Terminal screen snapshot pushed from the terminal emulator to the UI.
/// Cursor coordinates are row index and character offset (`None` = hidden).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSnapshot {
    pub rows: Vec<ScreenRow>,
    pub cursor_row: Option<u32>,
    pub cursor_char: Option<u32>,
    #[serde(default)]
    pub application_cursor: bool,
    #[serde(default)]
    pub bracketed_paste: bool,
    /// Garbled-byte statistics from the raw RX byte layer (default = Clean).
    /// Only serial sessions fill this; USB Shell snapshots keep the default
    /// and the UI must not read them as a serial baud-rate signal.
    #[serde(default)]
    pub garbled: GarbledStats,
}

/// Event payload streamed to the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UiEvent {
    SerialChunk {
        text: String,
        style: ChunkStyle,
    },
    /// Terminal emulator grid snapshot (UI rendering only; not written to `.log`/`serial_read`).
    SerialScreen {
        screen: std::sync::Arc<ScreenSnapshot>,
    },
    SerialDisconnected {
        reason: String,
    },
    DutUpdated {
        dut: DutId,
    },
    VersionInfo {
        version: String,
        schema: u32,
    },
    Pong {
        seq_seen: u64,
    },
}

/// Envelope wrapping a [`UiEvent`] with monotonic sequence, timestamp, and ownership.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub seq: u64,
    pub ts_unix_ms: u64,
    #[serde(default)]
    pub dut_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub event: UiEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip_and_unknown_field_tolerance() {
        let env = EventEnvelope {
            seq: 1,
            ts_unix_ms: 1_700_000_000_000,
            dut_id: Some("rk3576-a".into()),
            session_id: Some("sess-1".into()),
            event: UiEvent::VersionInfo {
                version: env!("CARGO_PKG_VERSION").into(),
                schema: 1,
            },
        };
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);

        let patched = format!(r#"{{"futureField":true,{}"#, &json[1..]);
        let tolerant: EventEnvelope = serde_json::from_str(&patched).unwrap();
        assert_eq!(env, tolerant);
    }

    #[test]
    fn screen_snapshot_garbled_contract() {
        assert_eq!(
            serde_json::to_value(GarbleLevel::Clean).unwrap(),
            serde_json::json!("clean")
        );
        assert_eq!(
            serde_json::to_value(GarbleLevel::Watch).unwrap(),
            serde_json::json!("watch")
        );
        assert_eq!(
            serde_json::to_value(GarbleLevel::Garbled).unwrap(),
            serde_json::json!("garbled")
        );

        let screen = ScreenSnapshot {
            rows: vec![ScreenRow {
                text: "abc".into(),
                fg: None,
                bg: Some(0x00_ff_00),
            }],
            cursor_row: Some(0),
            cursor_char: Some(3),
            application_cursor: false,
            bracketed_paste: true,
            garbled: GarbledStats {
                level: GarbleLevel::Watch,
                control_permille: 42,
                window_bytes: 4096,
            },
        };
        let json = serde_json::to_string(&screen).unwrap();
        let back: ScreenSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(screen, back);
        assert_eq!(
            serde_json::to_value(&screen).unwrap()["garbled"]["controlPermille"],
            serde_json::json!(42)
        );

        let legacy = serde_json::json!({
            "rows": [],
            "cursorRow": null,
            "cursorChar": null
        });
        let parsed: ScreenSnapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.garbled, GarbledStats::default());
        assert_eq!(parsed.garbled.level, GarbleLevel::Clean);
    }

    #[test]
    fn shared_screen_keeps_the_serial_screen_json_shape() {
        let screen = ScreenSnapshot::default();
        let value = serde_json::json!({ "event": "serial_screen", "screen": screen });
        let event: UiEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), value);
    }
}
