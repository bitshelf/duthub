//! serde contract for `duts.json` `garbledByDut` (per-DUT garbled-byte
//! detection switches).
//!
//! The existing `roundtrip.rs` / example files stay untouched; this file pins
//! the camelCase key, its omission rules, and the save→load path next to
//! `alertsByDut`.

use duthub_core::model::{AlertSettings, DutsFile, GarbledDetection};

fn empty_duts() -> DutsFile {
    DutsFile {
        version: 1,
        hosts: Vec::new(),
        relay_boards: Vec::new(),
        alerts_by_dut: std::collections::BTreeMap::new(),
        garbled_by_dut: std::collections::BTreeMap::new(),
        duts: Vec::new(),
    }
}

#[test]
fn empty_garbled_map_is_not_serialized() {
    // Legacy duts.json files have neither map; an empty section must not
    // appear on disk (docs/examples/duts.json stays byte-for-byte valid).
    let raw = serde_json::to_value(empty_duts()).unwrap();
    assert!(raw.get("garbledByDut").is_none(), "{raw}");
    assert!(raw.get("alertsByDut").is_none(), "{raw}");
}

#[test]
fn garbled_map_roundtrips_camel_case() {
    let mut file = empty_duts();
    file.garbled_by_dut
        .insert("dut-1".into(), GarbledDetection { enabled: true });

    let raw = serde_json::to_value(&file).unwrap();
    assert_eq!(
        raw["garbledByDut"]["dut-1"],
        serde_json::json!({"enabled": true})
    );
    // snake_case keys are silently ignored by serde (pitfall #15), so the
    // camelCase shape above is the only contract.
    let back: DutsFile = serde_json::from_value(raw).unwrap();
    assert_eq!(back.garbled_by_dut, file.garbled_by_dut);
}

#[test]
fn false_entry_serializes_as_empty_object() {
    // Fact pin: `enabled: false` is skipped by `is_false`. The product write
    // path never stores such an entry (it removes the key instead), which keeps
    // the empty-map omission invariant reachable.
    let raw = serde_json::to_value(GarbledDetection { enabled: false }).unwrap();
    assert_eq!(raw, serde_json::json!({}));
}

#[test]
fn save_load_keeps_per_dut_switch() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = empty_duts();
    file.garbled_by_dut
        .insert("dut-1".into(), GarbledDetection { enabled: true });
    duthub_core::store::save_duts_atomic(dir.path(), &file).unwrap();

    let loaded = duthub_core::store::load_duts(dir.path())
        .unwrap()
        .expect("duts.json should exist");
    assert_eq!(loaded, file);
    assert!(loaded.garbled_by_dut["dut-1"].enabled);
    assert!(!loaded.garbled_by_dut.contains_key("dut-2"));
}

#[test]
fn alerts_and_garbled_maps_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = empty_duts();
    file.alerts_by_dut.insert(
        "dut-1".into(),
        AlertSettings {
            enabled: false,
            suppressed_sources: vec!["serial".into()],
        },
    );
    file.garbled_by_dut
        .insert("dut-2".into(), GarbledDetection { enabled: true });
    duthub_core::store::save_duts_atomic(dir.path(), &file).unwrap();

    let loaded = duthub_core::store::load_duts(dir.path()).unwrap().unwrap();
    assert!(!loaded.alerts_by_dut["dut-1"].enabled);
    assert!(!loaded.garbled_by_dut.contains_key("dut-1"));
    assert!(!loaded.alerts_by_dut.contains_key("dut-2"));
    assert!(loaded.garbled_by_dut["dut-2"].enabled);
}
