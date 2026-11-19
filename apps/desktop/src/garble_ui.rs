//! Pure helpers for the serial garbled-byte signal (baud-rate mismatch).
//!
//! The device layer already decides the [`GarbleLevel`] and publishes it in
//! every screen snapshot; this module only decides *when* the desktop raises a
//! pop-up and how the control-byte share is formatted. Keeping both pure makes
//! the "alert once per entry" rule and the integer percent formatting
//! unit-testable without a GPUI context.

use duthub_core::ipc::GarbleLevel;

/// Whether a snapshot transition must raise a pop-up: only the step into
/// `Garbled`. Clear → watch is informational (the tab dot only), and staying
/// garbled must not re-alert every frame (the `AlertCenter` 30s dedup is a
/// second, independent guard for reconnects).
pub fn alert_on_transition(previous: GarbleLevel, next: GarbleLevel) -> bool {
    next == GarbleLevel::Garbled && previous != GarbleLevel::Garbled
}

/// Format permille as a one-decimal percentage without floats
/// (`605` → `"60.5"`). The device layer reports integers because
/// `ScreenSnapshot` derives `Eq`, so the formatting must stay integral too.
pub fn control_percent_text(permille: u16) -> String {
    format!("{}.{}", permille / 10, permille % 10)
}

#[cfg(test)]
mod tests {
    use super::{alert_on_transition, control_percent_text};
    use duthub_core::ipc::GarbleLevel;

    #[test]
    fn alert_fires_only_on_entry_to_garbled() {
        assert!(alert_on_transition(
            GarbleLevel::Clean,
            GarbleLevel::Garbled
        ));
        assert!(alert_on_transition(
            GarbleLevel::Watch,
            GarbleLevel::Garbled
        ));
        assert!(!alert_on_transition(
            GarbleLevel::Garbled,
            GarbleLevel::Garbled
        ));
        assert!(!alert_on_transition(
            GarbleLevel::Garbled,
            GarbleLevel::Watch
        ));
        assert!(!alert_on_transition(GarbleLevel::Clean, GarbleLevel::Watch));
        assert!(!alert_on_transition(GarbleLevel::Watch, GarbleLevel::Clean));
    }

    #[test]
    fn percent_text_uses_one_decimal_without_floats() {
        assert_eq!(control_percent_text(0), "0.0");
        assert_eq!(control_percent_text(7), "0.7");
        assert_eq!(control_percent_text(30), "3.0");
        assert_eq!(control_percent_text(605), "60.5");
        assert_eq!(control_percent_text(1000), "100.0");
        // Above 100% is possible: one stray control byte per ASCII byte still
        // counts raw bytes, and the window may hold any mix.
        assert_eq!(control_percent_text(1234), "123.4");
    }
}
