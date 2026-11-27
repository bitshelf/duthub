use std::io::{self, Write};
use std::path::{Path, PathBuf};

use duthub_core::model::DutConfig;

const REFERENCE_DIR: &str = "references";
const REFERENCE_LOG: &str = "reference-boot.log";

pub(crate) fn boot_reference_path(root: &Path, dut: &DutConfig) -> Option<PathBuf> {
    let configured = dut.serial.reference_log.as_deref()?.trim();
    if configured.is_empty() {
        return None;
    }
    let path = root.join(configured);
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
        .then_some(path)
}

pub(crate) fn reference_path_text(root: &Path, path: &Path) -> String {
    let root = duthub_core::store::plain_path(root);
    let path = duthub_core::store::plain_path(path);
    path.strip_prefix(root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned()
}

pub fn save(root: &Path, dut_id: &str, name: &str, text: &str) -> io::Result<PathBuf> {
    let (name, owner) = duthub_core::dut_artifacts::open_dut_for_write(root, dut_id, name)?;
    let directory = duthub_core::dut_artifacts::open_subdir_for_write(&owner, REFERENCE_DIR)?;
    let staged = format!(".reference-{}.tmp", uuid::Uuid::new_v4());
    let result = (|| {
        let mut file = directory.open_with(
            &staged,
            duthub_core::dut_artifacts::OpenOptions::new()
                .write(true)
                .create_new(true),
        )?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        directory.rename(&staged, &directory, REFERENCE_LOG)
    })();
    if result.is_err() {
        let _ = directory.remove_file(&staged);
    }
    result.map(|()| root.join(name).join(REFERENCE_DIR).join(REFERENCE_LOG))
}

pub(crate) fn record_learned_reference(
    file: &mut duthub_core::DutsFile,
    dut_id: &str,
    path: &str,
) -> bool {
    let Some(dut) = file.duts.iter_mut().find(|dut| dut.id == dut_id) else {
        return false;
    };
    dut.serial.reference_log = Some(path.to_owned());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_lookup_requires_a_configured_nonempty_file() {
        let mut dut: DutConfig = serde_json::from_value(serde_json::json!({
            "id": "dut-test",
            "name": "DUT Test",
            "serial": {"host": "local", "port": "COM3"}
        }))
        .unwrap();
        let root = std::env::temp_dir().join(format!("duthub-boot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();

        assert_eq!(boot_reference_path(&root, &dut), None);
        let learned = save(&root, &dut.id, &dut.name, "DDR Version 1").unwrap();
        assert_eq!(boot_reference_path(&root, &dut), None);
        dut.serial.reference_log = Some(reference_path_text(&root, &learned));
        assert_eq!(boot_reference_path(&root, &dut), Some(learned.clone()));
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(learned)
            .unwrap();
        assert_eq!(boot_reference_path(&root, &dut), None);
        dut.serial.reference_log = Some("manual.log".to_owned());
        assert_eq!(boot_reference_path(&root, &dut), None);
        dut.serial.reference_log = Some(String::new());
        assert_eq!(boot_reference_path(&root, &dut), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn record_learned_reference_updates_only_the_target_dut() {
        let mut file: duthub_core::DutsFile = serde_json::from_value(serde_json::json!({
            "version": 1,
            "duts": [
                {
                    "id": "dut-a",
                    "name": "DUT A",
                    "serial": {"host": "local", "port": "COM3", "referenceLog": ""}
                },
                {
                    "id": "dut-b",
                    "name": "DUT B",
                    "serial": {"host": "local", "port": "COM4", "referenceLog": "keep.log"}
                }
            ]
        }))
        .unwrap();

        let learned = "DUT A/references/reference-boot.log";
        assert!(record_learned_reference(&mut file, "dut-a", learned));
        assert!(!record_learned_reference(&mut file, "dut-missing", learned));
        assert_eq!(file.duts[0].serial.reference_log.as_deref(), Some(learned));
        assert_eq!(
            file.duts[1].serial.reference_log.as_deref(),
            Some("keep.log")
        );
    }
}
