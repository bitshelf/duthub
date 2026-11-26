//! Background disconnect monitor for bound HDMI / capture devices and USB debug
//! (ADB / HDC) targets.
//!
//! All blocking probes (`ffmpeg -list_devices`, `adb devices -l`, `hdc list targets`)
//! run on a dedicated background thread; the UI tick only syncs the watched target
//! list under a short lock and drains produced events into `AlertCenter` / toasts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use duthub_core::model::CaptureSettings;

use crate::media::Source;

/// Default poll cadence when at least one binding is watched.
pub const DEVICE_WATCH_INTERVAL_MS: u64 = 5_000;
/// Repeat reminder interval while a device remains disconnected (slightly above
/// `AlertCenter::DEDUP_WINDOW` so 50 ms tick jitter never drops a repeat alert).
pub const DEVICE_WATCH_REMIND_MS: u64 = 31_000;

/// One bound video capture device to watch for disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedMedia {
    pub dut_id: String,
    pub dut_name: String,
    pub source: Source,
    pub source_label: String,
    pub video: String,
}

/// One bound USB debug target (ADB or HDC) to watch for disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedUsb {
    pub dut_id: String,
    pub dut_name: String,
    /// `"adb"` or `"hdc"`.
    pub tool: String,
    pub serial: String,
}

/// Snapshot of one currently present video capture device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDeviceSnapshot {
    pub name: String,
    pub endpoint: Option<String>,
}

/// Disconnect / recovery events delivered to the UI tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceWatchEvent {
    MediaOffline {
        dut_id: String,
        dut_name: String,
        source_label: String,
        device_name: String,
    },
    MediaRecovered {
        dut_id: String,
        dut_name: String,
        source_label: String,
    },
    UsbOffline {
        dut_id: String,
        dut_name: String,
        tool: String,
        serial: String,
    },
    UsbRecovered {
        dut_id: String,
        dut_name: String,
        tool: String,
        serial: String,
    },
}

/// Read-only device presence probe (injectable for unit tests).
pub trait DeviceProbe: Send + Sync {
    fn probe_media(&self, settings: &CaptureSettings) -> Result<Vec<MediaDeviceSnapshot>, String>;
    /// Returns online device serials / connect keys for the given tool (`"adb"` or `"hdc"`).
    fn probe_usb(&self, tool: &str, executable: &str) -> Result<Vec<String>, String>;
}

/// Production probe using FFmpeg DirectShow/V4L2 and ADB/HDC CLIs.
pub struct RealDeviceProbe;

impl DeviceProbe for RealDeviceProbe {
    fn probe_media(&self, settings: &CaptureSettings) -> Result<Vec<MediaDeviceSnapshot>, String> {
        let raw = crate::media::list_devices(settings)?;
        if cfg!(windows) {
            Ok(crate::media_page::directshow_video_devices(&raw)
                .into_iter()
                .map(|device| MediaDeviceSnapshot {
                    name: device.name,
                    endpoint: device.endpoint,
                })
                .collect())
        } else {
            Ok(crate::media_page::detected_devices(&raw, false)
                .into_iter()
                .map(|name| MediaDeviceSnapshot {
                    name,
                    endpoint: None,
                })
                .collect())
        }
    }

    fn probe_usb(&self, tool: &str, executable: &str) -> Result<Vec<String>, String> {
        if tool.eq_ignore_ascii_case("hdc") {
            let targets = duthub_adb::HdcClient::real()
                .list_targets(executable)
                .map_err(|err| err.to_string())?;
            Ok(targets
                .into_iter()
                .filter(|target| !target.state.eq_ignore_ascii_case("offline"))
                .map(|target| target.key)
                .collect())
        } else {
            let targets = duthub_adb::AdbClient::real()
                .devices_l(executable)
                .map_err(|err| err.to_string())?;
            Ok(targets
                .into_iter()
                .filter(|target| !target.state.eq_ignore_ascii_case("offline"))
                .map(|target| target.serial)
                .collect())
        }
    }
}

#[derive(Debug, Clone, Default)]
struct TrackedState {
    reported_offline: bool,
    last_emit_ms: u64,
    /// Last resolved friendly display name for media endpoints.
    display_name: Option<String>,
}

#[derive(Debug, Default)]
struct WatchInner {
    media: Vec<WatchedMedia>,
    usb: Vec<WatchedUsb>,
    capture: CaptureSettings,
    adb_exe: String,
    hdc_exe: String,
    revision: u64,
    next_due_ms: u64,
    media_state: BTreeMap<(String, Source), TrackedState>,
    usb_state: BTreeMap<String, TrackedState>,
    events: Vec<DeviceWatchEvent>,
}

/// Disconnect monitor for bound HDMI / capture devices and USB debug targets.
pub struct DeviceWatchService {
    probe: Arc<dyn DeviceProbe>,
    interval_ms: u64,
    remind_interval_ms: u64,
    inner: Mutex<WatchInner>,
}

impl DeviceWatchService {
    pub fn new(probe: Arc<dyn DeviceProbe>) -> Self {
        Self {
            probe,
            interval_ms: DEVICE_WATCH_INTERVAL_MS,
            remind_interval_ms: DEVICE_WATCH_REMIND_MS,
            inner: Mutex::new(WatchInner::default()),
        }
    }

    /// Update the watched targets and tool paths (cheap and idempotent).
    pub fn sync_targets(
        &self,
        media: Vec<WatchedMedia>,
        usb: Vec<WatchedUsb>,
        capture: CaptureSettings,
        adb_exe: String,
        hdc_exe: String,
    ) {
        let mut inner = self.lock();
        if inner.media == media
            && inner.usb == usb
            && inner.capture == capture
            && inner.adb_exe == adb_exe
            && inner.hdc_exe == hdc_exe
        {
            return;
        }
        let media_keys: BTreeSet<(String, Source)> = media
            .iter()
            .map(|item| (item.dut_id.clone(), item.source))
            .collect();
        inner.media_state.retain(|key, _| media_keys.contains(key));
        let usb_keys: BTreeSet<String> = usb.iter().map(|item| item.dut_id.clone()).collect();
        inner.usb_state.retain(|key, _| usb_keys.contains(key));
        inner.media = media;
        inner.usb = usb;
        inner.capture = capture;
        inner.adb_exe = adb_exe;
        inner.hdc_exe = hdc_exe;
        inner.revision = inner.revision.wrapping_add(1);
        inner.next_due_ms = 0;
    }

    /// Run one probe pass if due (blocks on I/O — call from background thread only).
    pub fn tick_at(&self, now_ms: u64) -> bool {
        let (revision, media, usb, capture, adb_exe, hdc_exe) = {
            let inner = self.lock();
            if (inner.media.is_empty() && inner.usb.is_empty()) || now_ms < inner.next_due_ms {
                return false;
            }
            (
                inner.revision,
                inner.media.clone(),
                inner.usb.clone(),
                inner.capture.clone(),
                inner.adb_exe.clone(),
                inner.hdc_exe.clone(),
            )
        };

        let media_sample = if media.is_empty() {
            None
        } else {
            self.probe.probe_media(&capture).ok()
        };
        let needs_adb = usb
            .iter()
            .any(|item| !item.tool.eq_ignore_ascii_case("hdc"));
        let needs_hdc = usb.iter().any(|item| item.tool.eq_ignore_ascii_case("hdc"));
        let adb_sample = if needs_adb {
            self.probe.probe_usb("adb", &adb_exe).ok()
        } else {
            None
        };
        let hdc_sample = if needs_hdc {
            self.probe.probe_usb("hdc", &hdc_exe).ok()
        } else {
            None
        };

        let mut inner = self.lock();
        if inner.revision != revision {
            return true;
        }
        inner.next_due_ms = now_ms.saturating_add(self.interval_ms);

        let mut new_events = Vec::new();
        if let Some(devices) = media_sample.as_deref() {
            for item in &media {
                let key = (item.dut_id.clone(), item.source);
                let state = inner.media_state.entry(key).or_default();
                let matched = devices.iter().find(|device| {
                    device.name.eq_ignore_ascii_case(item.video.trim())
                        || device
                            .endpoint
                            .as_deref()
                            .is_some_and(|id| id.eq_ignore_ascii_case(item.video.trim()))
                });
                if let Some(device) = matched {
                    state.display_name = Some(device.name.clone());
                    if std::mem::replace(&mut state.reported_offline, false) {
                        new_events.push(DeviceWatchEvent::MediaRecovered {
                            dut_id: item.dut_id.clone(),
                            dut_name: item.dut_name.clone(),
                            source_label: item.source_label.clone(),
                        });
                    }
                } else {
                    let due = !state.reported_offline
                        || now_ms >= state.last_emit_ms.saturating_add(self.remind_interval_ms);
                    if due {
                        state.reported_offline = true;
                        state.last_emit_ms = now_ms;
                        let device_name = state
                            .display_name
                            .clone()
                            .unwrap_or_else(|| item.video.trim().to_owned());
                        new_events.push(DeviceWatchEvent::MediaOffline {
                            dut_id: item.dut_id.clone(),
                            dut_name: item.dut_name.clone(),
                            source_label: item.source_label.clone(),
                            device_name,
                        });
                    }
                }
            }
        }

        for item in &usb {
            let sample = if item.tool.eq_ignore_ascii_case("hdc") {
                hdc_sample.as_deref()
            } else {
                adb_sample.as_deref()
            };
            let Some(online_serials) = sample else {
                continue;
            };
            let state = inner.usb_state.entry(item.dut_id.clone()).or_default();
            let present = online_serials
                .iter()
                .any(|serial| serial.eq_ignore_ascii_case(item.serial.trim()));
            if present {
                if std::mem::replace(&mut state.reported_offline, false) {
                    new_events.push(DeviceWatchEvent::UsbRecovered {
                        dut_id: item.dut_id.clone(),
                        dut_name: item.dut_name.clone(),
                        tool: item.tool.to_ascii_uppercase(),
                        serial: item.serial.clone(),
                    });
                }
            } else {
                let due = !state.reported_offline
                    || now_ms >= state.last_emit_ms.saturating_add(self.remind_interval_ms);
                if due {
                    state.reported_offline = true;
                    state.last_emit_ms = now_ms;
                    new_events.push(DeviceWatchEvent::UsbOffline {
                        dut_id: item.dut_id.clone(),
                        dut_name: item.dut_name.clone(),
                        tool: item.tool.to_ascii_uppercase(),
                        serial: item.serial.clone(),
                    });
                }
            }
        }

        inner.events.extend(new_events);
        true
    }

    pub fn drain_events(&self) -> Vec<DeviceWatchEvent> {
        std::mem::take(&mut self.lock().events)
    }

    fn lock(&self) -> MutexGuard<'_, WatchInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProbe {
        media: Mutex<Result<Vec<MediaDeviceSnapshot>, String>>,
        usb: Mutex<Result<Vec<String>, String>>,
    }

    impl DeviceProbe for FakeProbe {
        fn probe_media(
            &self,
            _settings: &CaptureSettings,
        ) -> Result<Vec<MediaDeviceSnapshot>, String> {
            self.media.lock().unwrap().clone()
        }

        fn probe_usb(&self, _tool: &str, _executable: &str) -> Result<Vec<String>, String> {
            self.usb.lock().unwrap().clone()
        }
    }

    #[test]
    fn media_and_usb_emit_offline_remind_and_recovery_edges() {
        let probe = Arc::new(FakeProbe {
            media: Mutex::new(Ok(vec![MediaDeviceSnapshot {
                name: "UGREEN 35866".into(),
                endpoint: Some("@device_pnp_ugreen".into()),
            }])),
            usb: Mutex::new(Ok(vec!["SER123".into()])),
        });
        let watch = DeviceWatchService::new(probe.clone());
        watch.sync_targets(
            vec![WatchedMedia {
                dut_id: "dut-1".into(),
                dut_name: "DUT-1".into(),
                source: Source::Hdmi,
                source_label: "HDMI".into(),
                video: "@device_pnp_ugreen".into(),
            }],
            vec![WatchedUsb {
                dut_id: "dut-1".into(),
                dut_name: "DUT-1".into(),
                tool: "adb".into(),
                serial: "SER123".into(),
            }],
            CaptureSettings::default(),
            "adb".into(),
            "hdc".into(),
        );

        // Both online -> no events, and display name is learned.
        assert!(watch.tick_at(1_000));
        assert!(watch.drain_events().is_empty());

        // Both unplug -> Offline events with resolved friendly device name.
        *probe.media.lock().unwrap() = Ok(Vec::new());
        *probe.usb.lock().unwrap() = Ok(Vec::new());
        assert!(watch.tick_at(6_000));
        let events = watch.drain_events();
        assert_eq!(
            events,
            vec![
                DeviceWatchEvent::MediaOffline {
                    dut_id: "dut-1".into(),
                    dut_name: "DUT-1".into(),
                    source_label: "HDMI".into(),
                    device_name: "UGREEN 35866".into(),
                },
                DeviceWatchEvent::UsbOffline {
                    dut_id: "dut-1".into(),
                    dut_name: "DUT-1".into(),
                    tool: "ADB".into(),
                    serial: "SER123".into(),
                },
            ]
        );

        // Inside remind window -> silent.
        assert!(watch.tick_at(15_000));
        assert!(watch.drain_events().is_empty());

        // Probe errors do not trigger false alerts or drop offline state.
        *probe.media.lock().unwrap() = Err("ffmpeg missing".into());
        *probe.usb.lock().unwrap() = Err("adb error".into());
        assert!(watch.tick_at(80_000));
        assert!(watch.drain_events().is_empty());

        // Both reconnect -> Recovered events.
        *probe.media.lock().unwrap() = Ok(vec![MediaDeviceSnapshot {
            name: "UGREEN 35866".into(),
            endpoint: Some("@device_pnp_ugreen".into()),
        }]);
        *probe.usb.lock().unwrap() = Ok(vec!["SER123".into()]);
        assert!(watch.tick_at(86_000));
        assert_eq!(
            watch.drain_events(),
            vec![
                DeviceWatchEvent::MediaRecovered {
                    dut_id: "dut-1".into(),
                    dut_name: "DUT-1".into(),
                    source_label: "HDMI".into(),
                },
                DeviceWatchEvent::UsbRecovered {
                    dut_id: "dut-1".into(),
                    dut_name: "DUT-1".into(),
                    tool: "ADB".into(),
                    serial: "SER123".into(),
                },
            ]
        );
    }
}
