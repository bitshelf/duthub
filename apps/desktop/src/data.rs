//! DUT/Host Data Status (Connection Management Page + Agent Target Selection Sharing).
//!
//! Data source: `.duthub/duts.json`. No pre-configured connections are available for the new
//! workspace.

use duthub_core::model::{AlertSettings, DutConfig, DutMediaConfig, FlashConfig, FlashToolPreset};
use duthub_core::{DutsFile, store};

pub fn flash_preset_labels(presets: &[FlashToolPreset]) -> Vec<(String, String)> {
    let mut options = Vec::with_capacity(presets.len());
    for preset in presets {
        let duplicate = presets
            .iter()
            .any(|other| other.id != preset.id && other.name == preset.name);
        let label = if duplicate {
            format!("{} · {}", preset.name, preset.id)
        } else {
            preset.name.clone()
        };
        options.push((label, preset.id.clone()));
    }
    options
}

pub fn flash_preset_index(flash: &FlashConfig, presets: &[FlashToolPreset]) -> Option<usize> {
    if let Some(id) = flash.preset_id.as_deref() {
        return presets.iter().position(|preset| preset.id == id);
    }
    if flash.executable.is_empty() {
        return None;
    }
    let mut matches = presets
        .iter()
        .enumerate()
        .filter(|(_, preset)| preset.executable == flash.executable);
    let (index, _) = matches.next()?;
    matches.next().is_none().then_some(index)
}

/// A missing selection keeps the existing snapshot. An empty ID explicitly clears it.
pub fn selected_flash_config(
    previous: Option<&FlashConfig>,
    selected: Option<&str>,
    presets: &[FlashToolPreset],
) -> FlashConfig {
    match selected {
        Some("") => FlashConfig::default(),
        Some(id) if previous.and_then(|flash| flash.preset_id.as_deref()) != Some(id) => {
            if let Some(preset) = presets.iter().find(|preset| preset.id == id) {
                return FlashConfig {
                    preset_id: Some(preset.id.clone()),
                    executable: preset.executable.clone(),
                    test_args: preset
                        .test_args
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect(),
                };
            }
            previous.cloned().unwrap_or_default()
        }
        _ => previous.cloned().unwrap_or_default(),
    }
}

#[cfg(test)]
const DEMO_DUTS_JSON: &str = include_str!("../../../docs/examples/duts.json");

pub fn remove_connection(file: &mut DutsFile, dut_id: &str) -> bool {
    let before = file.duts.len();
    file.duts.retain(|dut| dut.id != dut_id);
    file.alerts_by_dut.remove(dut_id);
    file.garbled_by_dut.remove(dut_id);
    file.duts.len() != before
}

pub fn remove_control_device(file: &mut DutsFile, id: &str) -> Result<bool, String> {
    use duthub_core::model::RelayConfig;

    if let Some(dut) = file
        .duts
        .iter()
        .find(|dut| matches!(&dut.relay, RelayConfig::UsbBoard { board_id, .. } if board_id == id))
    {
        return Err(crate::i18n::message(
            "controlDeviceInUse",
            &[("dut", &dut.name)],
        ));
    }
    let before = file.relay_boards.len();
    file.relay_boards.retain(|board| board.id != id);
    Ok(file.relay_boards.len() != before)
}

pub struct DutsState {
    pub file: DutsFile,
    pub selected: usize,
    pub new_connection: Option<NewConnection>,
    pub editing_new_connection: bool,
    /// The data has not been saved to the disk yet.
    pub dirty: bool,
    saved_media: std::collections::HashMap<String, DutMediaConfig>,
    saved_usb: std::collections::HashMap<String, duthub_core::model::AdbConfig>,
    saved_names: std::collections::HashMap<String, String>,
}

pub struct NewConnection {
    pub dut: DutConfig,
    pub alerts: Option<AlertSettings>,
    /// Per-DUT garbled-byte detection draft (`duts.json` `garbledByDut`).
    pub garbled: Option<duthub_core::model::GarbledDetection>,
    pub connect_after_save: bool,
}

impl NewConnection {
    pub fn config_to_save(&self) -> Result<DutConfig, &'static str> {
        let mut dut = self.dut.clone();
        dut.name = dut.name.trim().to_owned();
        dut.serial.port = dut.serial.port.trim().to_owned();
        if dut.name.is_empty() {
            return Err("enterDutName");
        }
        if dut.serial.port.is_empty() {
            return Err("selectPortFirst");
        }
        Ok(dut)
    }
}

impl DutsState {
    /// Loading (starting from an empty list in case of missing or damaged files); returning status
    /// and optional error messages.
    pub fn load(dir: &std::path::Path) -> (Self, Option<String>) {
        match store::load_duts(dir) {
            Ok(Some(file)) => (Self::from_file(file), None),
            Ok(None) => (Self::empty(), None),
            Err(err) => (Self::empty(), Some(err.to_string())),
        }
    }

    pub fn empty() -> Self {
        Self::from_file(store::default_duts())
    }

    #[cfg(test)]
    pub fn demo() -> Self {
        let file: DutsFile =
            serde_json::from_str(DEMO_DUTS_JSON).expect("docs/examples/duts.json is a valid file.");
        Self::from_file(file)
    }

    pub fn from_file(mut file: DutsFile) -> Self {
        file.migrate_usb_relays();
        let mut state = Self {
            file,
            selected: 0,
            new_connection: None,
            editing_new_connection: false,
            dirty: false,
            saved_media: std::collections::HashMap::default(),
            saved_usb: std::collections::HashMap::default(),
            saved_names: std::collections::HashMap::default(),
        };
        state.sync_saved_media();
        state
    }

    pub fn sync_saved_media(&mut self) {
        self.saved_media.clear();
        let mut ids = std::collections::HashSet::new();
        for dut in &self.file.duts {
            if ids.insert(&dut.id) {
                self.saved_media.insert(dut.id.clone(), dut.media.clone());
            } else {
                self.saved_media.remove(&dut.id);
            }
        }
        self.saved_usb = self
            .file
            .duts
            .iter()
            .map(|dut| (dut.id.clone(), dut.adb.clone()))
            .collect();
        self.saved_names = self
            .file
            .duts
            .iter()
            .map(|dut| (dut.id.clone(), dut.name.clone()))
            .collect();
    }

    pub fn saved_usb(&self, dut_id: &str) -> Option<&duthub_core::model::AdbConfig> {
        self.saved_usb.get(dut_id)
    }

    pub fn saved_name(&self, dut_id: &str) -> Option<&str> {
        self.saved_names.get(dut_id).map(String::as_str)
    }

    pub fn other_usb_bindings<'a>(
        &'a self,
        dut_id: &'a str,
    ) -> impl Iterator<Item = &'a duthub_core::model::AdbConfig> {
        self.saved_usb
            .iter()
            .filter(move |(id, _)| id.as_str() != dut_id)
            .map(|(_, config)| config)
    }

    pub fn saved_media(&self, dut_id: &str) -> Option<&DutMediaConfig> {
        self.saved_media.get(dut_id)
    }

    pub fn saved_media_bindings(&self) -> impl Iterator<Item = (&str, &DutMediaConfig)> {
        self.saved_media
            .iter()
            .map(|(id, media)| (id.as_str(), media))
    }

    pub fn selected(&self) -> Option<&DutConfig> {
        if self.editing_new_connection {
            self.new_connection.as_ref().map(|draft| &draft.dut)
        } else {
            self.file.duts.get(self.selected)
        }
    }

    pub fn selected_mut(&mut self) -> Option<&mut DutConfig> {
        if self.editing_new_connection {
            self.new_connection.as_mut().map(|draft| &mut draft.dut)
        } else {
            self.file.duts.get_mut(self.selected)
        }
    }

    pub fn connection_mut(&mut self, id: &str) -> Option<&mut DutConfig> {
        if let Some(draft) = self.new_connection.as_mut()
            && draft.dut.id == id
        {
            return Some(&mut draft.dut);
        }
        self.file.duts.iter_mut().find(|dut| dut.id == id)
    }

    pub fn mark_connection_dirty(&mut self) {
        self.dirty |= !self.editing_new_connection;
    }

    pub fn add_saved_connection(&mut self, dut: DutConfig) {
        self.saved_media.insert(dut.id.clone(), dut.media.clone());
        self.saved_usb.insert(dut.id.clone(), dut.adb.clone());
        self.saved_names.insert(dut.id.clone(), dut.name.clone());
        self.file.duts.push(dut);
    }

    pub fn forget_saved_bindings(&mut self, dut_id: &str) {
        self.saved_media.remove(dut_id);
        self.saved_usb.remove(dut_id);
        self.saved_names.remove(dut_id);
    }

    pub fn save(&mut self, dir: &std::path::Path) -> Result<(), String> {
        store::save_duts_atomic(dir, &self.file).map_err(|e| e.to_string())?;
        self.sync_saved_media();
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_flash_bindings_display_a_unique_configured_preset() {
        let flash = FlashConfig {
            executable: "upgrade_tool".into(),
            ..FlashConfig::default()
        };
        let preset = FlashToolPreset {
            id: "rockchip".into(),
            name: "Rockchip".into(),
            executable: flash.executable.clone(),
            test_args: String::new(),
        };
        assert_eq!(
            flash_preset_index(&flash, std::slice::from_ref(&preset)),
            Some(0)
        );
        assert_eq!(flash_preset_index(&flash, &[]), None);
        let mut other = preset.clone();
        other.id = "other".into();
        assert_eq!(flash_preset_index(&flash, &[preset, other]), None);
        assert_eq!(flash.preset_id, None);
    }

    #[test]
    fn new_connection_stays_out_of_saved_bindings_until_committed() {
        let mut state = DutsState::demo();
        let mut dut = state.file.duts[0].clone();
        dut.id = "new-dut".into();
        dut.name.clear();
        dut.serial.port.clear();
        state.new_connection = Some(NewConnection {
            dut,
            alerts: None,
            garbled: None,
            connect_after_save: true,
        });
        state.editing_new_connection = true;
        assert_eq!(
            state.new_connection.as_ref().unwrap().config_to_save(),
            Err("enterDutName")
        );
        state.selected_mut().unwrap().name = " board ".into();
        assert_eq!(
            state.new_connection.as_ref().unwrap().config_to_save(),
            Err("selectPortFirst")
        );
        let dut = state.selected_mut().unwrap();
        dut.serial.port = " COM30 ".into();
        dut.adb.serial = "test-device".into();
        dut.media.hdmi.video = "test-capture".into();
        state.mark_connection_dirty();
        assert!(!state.dirty);
        assert_eq!(state.selected().unwrap().id, "new-dut");
        assert!(state.saved_media("new-dut").is_none());
        assert!(state.saved_usb("new-dut").is_none());
        assert!(state.file.duts.iter().all(|dut| dut.id != "new-dut"));

        let draft = state.new_connection.take().unwrap();
        let saved = draft.config_to_save().unwrap();
        assert_eq!(saved.name, "board");
        assert_eq!(saved.serial.port, "COM30");
        state.add_saved_connection(saved);
        assert_eq!(state.saved_usb("new-dut").unwrap().serial, "test-device");
        assert_eq!(
            state.saved_media("new-dut").unwrap().hdmi.video,
            "test-capture"
        );
    }

    #[test]
    fn flash_selection_uses_configured_preset_ids_and_preserves_legacy_bindings() {
        let saved = FlashConfig {
            preset_id: Some("removed".into()),
            executable: "old.exe".into(),
            test_args: vec!["--help".into()],
        };
        let tool = FlashToolPreset {
            id: "phoenix".into(),
            name: "PhoenixSuit".into(),
            executable: "C:/Burn Tools/PhoenixSuit.exe".into(),
            test_args: String::new(),
        };
        let catalog = [tool];
        assert_eq!(
            selected_flash_config(None, None, &catalog),
            FlashConfig::default()
        );
        assert_eq!(selected_flash_config(Some(&saved), None, &catalog), saved);
        assert_eq!(
            selected_flash_config(Some(&saved), Some("missing"), &catalog),
            saved
        );
        assert_eq!(
            selected_flash_config(Some(&saved), Some(""), &catalog),
            FlashConfig::default()
        );
        let selected = selected_flash_config(Some(&saved), Some("phoenix"), &catalog);
        assert_eq!(selected.preset_id.as_deref(), Some("phoenix"));
        assert_eq!(selected.executable, catalog[0].executable);
        assert_eq!(selected.test_args, Vec::<String>::new());
    }

    #[test]
    fn removing_control_devices_preserves_dut_bindings() {
        let mut file = DutsState::demo().file;
        let bound = file.relay_boards[0].clone();
        let mut spare = bound.clone();
        spare.id = "spare".to_owned();
        file.relay_boards.push(spare);
        assert!(remove_control_device(&mut file, &bound.id).is_err());
        assert_eq!(file.relay_boards.len(), 2);
        assert!(remove_control_device(&mut file, "spare").unwrap());
        assert_eq!(file.relay_boards[0].id, bound.id);
        assert_eq!(file.duts.len(), 1);
    }

    #[test]
    fn missing_connection_file_does_not_create_demo_duts() {
        let path = std::env::temp_dir().join(format!("duthub-fresh-{}", uuid::Uuid::new_v4()));
        let (state, error) = DutsState::load(&path);
        assert!(error.is_none());
        assert_eq!(state.file.duts, Vec::<DutConfig>::new());
        assert_eq!(
            state.file.relay_boards,
            Vec::<duthub_core::model::RelayBoard>::new()
        );
    }

    #[test]
    fn removing_a_connection_leaves_relay_drafts_and_other_duts_intact() {
        let mut data = DutsState::demo().file;
        let original = data.duts[0].clone();
        let mut second = original.clone();
        second.id = "removable".into();
        data.duts.push(second);
        data.alerts_by_dut
            .insert("removable".into(), AlertSettings::default());
        data.relay_boards[0].node.clear();
        assert!(remove_connection(&mut data, "removable"));
        assert!(!remove_connection(&mut data, "removable"));
        assert_eq!(data.duts, [original]);
        assert_eq!(data.relay_boards[0].node, "");
        assert!(!data.alerts_by_dut.contains_key("removable"));
    }

    #[test]
    fn legacy_relay_bindings_share_a_board_without_changing_channels() {
        let original: DutsFile = serde_json::from_str(DEMO_DUTS_JSON).unwrap();
        let mut file = original.clone();
        let mut second = file.duts[0].clone();
        second.id = "another-dut".into();
        file.duts.push(second);
        file.migrate_usb_relays();
        assert_eq!(file.relay_boards.len(), 1);
        assert_eq!(file.duts[0].relay, file.duts[1].relay);
        assert_eq!(
            file.resolve_relay(&file.duts[0].relay),
            original.duts[0].relay
        );
    }

    #[test]
    fn saved_empty_connection_list_survives_reload() {
        let dir = std::env::temp_dir().join(format!("duthub-empty-{}", uuid::Uuid::new_v4()));
        let mut state = DutsState::demo();
        state.file.duts.clear();
        state.save(&dir).unwrap();
        let (reloaded, error) = DutsState::load(&dir);
        assert!(error.is_none());
        assert_eq!(reloaded.file.duts, Vec::<DutConfig>::new());
        std::fs::remove_file(dir.join("duts.json")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
