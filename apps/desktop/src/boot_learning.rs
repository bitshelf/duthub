use std::io::{self, Write};
use std::path::{Path, PathBuf};

use duthub_core::model::DutConfig;

const REFERENCE_DIR: &str = "references";
const REFERENCE_LOG: &str = "reference-boot.log";

pub(crate) fn boot_reference_path(root: &Path, dut: &DutConfig) -> Option<PathBuf> {
    if let Some(path) = dut.serial.reference_log.as_deref() {
        return (!path.trim().is_empty()).then(|| root.join(path.trim()));
    }
    let name = duthub_core::dut_artifacts::name_component(&dut.name).ok()?;
    let path = root.join(name).join(REFERENCE_DIR).join(REFERENCE_LOG);
    path.is_file().then_some(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_lookup_uses_the_dut_configuration_and_its_learned_file() {
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
        assert_eq!(boot_reference_path(&root, &dut), Some(learned));
        dut.serial.reference_log = Some("manual.log".to_owned());
        assert_eq!(
            boot_reference_path(&root, &dut),
            Some(root.join("manual.log"))
        );
        dut.serial.reference_log = Some(String::new());
        assert_eq!(boot_reference_path(&root, &dut), None);

        std::fs::remove_dir_all(root).unwrap();
    }
}
