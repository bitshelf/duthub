//! Read-only discovery via the official `adb` CLI (`adb devices -l`).
//!
//! Contract (`docs/SECURITY.md` #6): the executable path comes from
//! `settings.tools.adbExecutable`; subsequent write operations require `state == device`.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use duthub_core::process::{ProcessRunner, RealProcessRunner, resolve_executable, run_binary};
use duthub_core::{AdbDevice, DeviceError, DeviceResult};

const DEVICES_TIMEOUT: Duration = Duration::from_secs(12);
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub enum TargetUnavailable {
    Missing,
    NotReady(String),
    Ambiguous,
}

/// Auto selection is constrained to one online, unreserved device. A manual binding
/// always refers to its exact serial, including when the attached hardware changes.
pub fn choose_target<'a>(
    devices: &'a [AdbDevice],
    serial: Option<&str>,
    reserved: &[String],
) -> Result<&'a str, TargetUnavailable> {
    if let Some(serial) = serial {
        let device = devices
            .iter()
            .find(|device| device.serial == serial)
            .ok_or(TargetUnavailable::Missing)?;
        return if device.state == "device" {
            Ok(&device.serial)
        } else {
            Err(TargetUnavailable::NotReady(device.state.clone()))
        };
    }
    let mut candidates = devices
        .iter()
        .filter(|device| !reserved.contains(&device.serial));
    let mut online = candidates.clone().filter(|device| device.state == "device");
    match (online.next(), online.next()) {
        (Some(device), None) => Ok(&device.serial),
        (Some(_), Some(_)) => Err(TargetUnavailable::Ambiguous),
        _ => match (candidates.next(), candidates.next()) {
            (Some(device), None) => Err(TargetUnavailable::NotReady(device.state.clone())),
            _ => Err(TargetUnavailable::Missing),
        },
    }
}

#[derive(Debug, Clone)]
pub struct AndroidPreview {
    executable: std::path::PathBuf,
    serial: String,
    display: Option<String>,
}

impl AndroidPreview {
    pub fn capture(&self) -> DeviceResult<Vec<u8>> {
        let mut args = vec![
            "-s".into(),
            self.serial.clone(),
            "exec-out".into(),
            "screencap".into(),
            "-p".into(),
        ];
        if let Some(display) = &self.display {
            args.extend(["-d".into(), display.clone()]);
        }
        let mut bytes = run_binary(&self.executable, &args, PREVIEW_TIMEOUT, 32 * 1024 * 1024)?;
        let offset = png_start(&bytes).ok_or(DeviceError::Unsupported)?;
        bytes.drain(..offset);
        Ok(bytes)
    }
}

fn png_start(bytes: &[u8]) -> Option<usize> {
    // Some Android builds emit a diagnostic before the PNG on stdout.
    bytes
        .get(..bytes.len().min(4096))?
        .windows(8)
        .position(|bytes| bytes == b"\x89PNG\r\n\x1a\n")
}

fn first_display(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let id = line
            .trim()
            .strip_prefix("Display ")?
            .split_whitespace()
            .next()?;
        id.parse::<u64>().ok().map(|_| id.to_owned())
    })
}

pub struct AdbClient {
    runner: Arc<dyn ProcessRunner>,
}

impl std::fmt::Debug for AdbClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdbClient").finish_non_exhaustive()
    }
}

impl AdbClient {
    pub fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self { runner }
    }

    /// Default real process runner.
    #[must_use]
    pub fn real() -> Self {
        Self::new(Arc::new(RealProcessRunner))
    }

    /// Discover Android capabilities independently from opening an ADB shell. Older
    /// adbd builds and Debian boards may provide shell without Android properties.
    #[must_use]
    pub fn android_preview(&self, adb_spec: &str, serial: &str) -> Option<AndroidPreview> {
        let executable = resolve_executable(adb_spec).ok()?;
        let sdk = self
            .runner
            .run(
                &executable,
                &[
                    "-s".into(),
                    serial.into(),
                    "shell".into(),
                    "getprop".into(),
                    "ro.build.version.sdk".into(),
                ],
                PREVIEW_TIMEOUT,
            )
            .ok()?;
        if sdk.exit_code != Some(0)
            || !sdk
                .stdout
                .lines()
                .any(|line| line.trim().parse::<u32>().is_ok_and(|sdk| sdk > 0))
        {
            return None;
        }
        let display = self
            .runner
            .run(
                &executable,
                &[
                    "-s".into(),
                    serial.into(),
                    "shell".into(),
                    "dumpsys".into(),
                    "SurfaceFlinger".into(),
                    "--display-id".into(),
                ],
                PREVIEW_TIMEOUT,
            )
            .ok()
            .filter(|output| output.exit_code == Some(0))
            .and_then(|output| first_display(&output.stdout));
        Some(AndroidPreview {
            executable,
            serial: serial.to_owned(),
            display,
        })
    }

    /// Run `adb devices -l` and parse the device list (read-only, no side effects).
    pub fn devices_l(&self, adb_spec: &str) -> DeviceResult<Vec<AdbDevice>> {
        let exe = resolve_executable(adb_spec)?;
        let result = self
            .runner
            .run(&exe, &["devices".into(), "-l".into()], DEVICES_TIMEOUT)?;
        if result.exit_code != Some(0) {
            let detail = excerpt(&result.stderr, &result.stdout);
            return Err(DeviceError::Invalid(format!(
                "adb 退出码 {:?}: {detail}",
                result.exit_code
            )));
        }
        Ok(parse_devices_l(&result.stdout))
    }
}

fn excerpt(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    text.lines()
        .last()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// Read the existing loopback ADB server without resolving or launching an executable.
pub fn list_local_server_devices() -> DeviceResult<Vec<AdbDevice>> {
    let host = mozdevice::Host {
        host: Some("127.0.0.1".into()),
        port: Some(5037),
        ..Default::default()
    };
    let output = host
        .execute_host_command("devices-l", true, true)
        .map_err(|_| DeviceError::Invalid("本机 ADB 服务不可用".into()))?;
    Ok(parse_devices_l(&output))
}

/// Parse `adb devices -l` output: skip header, blank, and daemon lines; extract serial, state, and model.
#[must_use]
pub fn parse_devices_l(output: &str) -> Vec<AdbDevice> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches(['\r']);
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("List of devices")
                || trimmed.starts_with("* daemon")
                || trimmed.starts_with("adb server")
            {
                return None;
            }
            let mut tokens = trimmed.split_whitespace();
            let serial = tokens.next()?;
            let state = tokens.next()?;
            if state.contains(':') {
                // Not a bare state token, so this is not a device row.
                return None;
            }
            let model = tokens
                .find_map(|t| t.strip_prefix("model:"))
                .map(|m| m.trim_end_matches(',').to_string());
            Some(AdbDevice {
                serial: serial.to_string(),
                state: state.to_string(),
                model,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use duthub_core::ProcessResult;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct MockRunner {
        stdout: String,
        exit_code: i32,
        calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
    }

    impl ProcessRunner for MockRunner {
        fn run(
            &self,
            exe: &Path,
            args: &[String],
            _timeout: Duration,
        ) -> DeviceResult<ProcessResult> {
            self.calls
                .lock()
                .unwrap()
                .push((exe.to_path_buf(), args.to_vec()));
            Ok(ProcessResult {
                exit_code: Some(self.exit_code),
                stdout: self.stdout.clone(),
                stderr: String::new(),
                elapsed_ms: 12,
            })
        }
    }

    const FIXTURE: &str = "List of devices attached\r\n* daemon not running; starting now at tcp:5037\r\n* daemon started successfully\r\nRK3576USB002           device usb:1-2 product:rk3576 model:RK3576_Board device:rk3576 transport_id:2\r\nA12BC34D5678           offline\r\nEMULATOR123           unauthorized\r\n\r\n";

    #[test]
    fn parses_fixture_states_and_model() {
        let devices = parse_devices_l(FIXTURE);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].serial, "RK3576USB002");
        assert_eq!(devices[0].state, "device");
        assert_eq!(devices[0].model.as_deref(), Some("RK3576_Board"));
        assert_eq!(devices[1].state, "offline");
        assert_eq!(devices[1].model, None);
        assert_eq!(devices[2].state, "unauthorized");
    }

    #[test]
    fn auto_target_requires_an_unassigned_online_device_and_manual_does_not_fall_back() {
        let mut devices = parse_devices_l(FIXTURE);
        assert_eq!(choose_target(&devices, None, &[]), Ok("RK3576USB002"));
        assert_eq!(
            choose_target(&devices, Some("missing"), &[]),
            Err(TargetUnavailable::Missing)
        );
        assert!(matches!(
            choose_target(&devices, Some("A12BC34D5678"), &[]),
            Err(TargetUnavailable::NotReady(_))
        ));
        devices.push(AdbDevice {
            serial: "second".into(),
            state: "device".into(),
            model: None,
        });
        assert_eq!(
            choose_target(&devices, None, &[]),
            Err(TargetUnavailable::Ambiguous)
        );
        assert_eq!(
            choose_target(&devices, None, &["second".into()]),
            Ok("RK3576USB002")
        );
    }

    #[test]
    fn discovers_display_and_identifies_png_after_device_diagnostics() {
        assert_eq!(
            first_display("Display 0 (HWC display 0): no display identification data\nDisplay 1")
                .as_deref(),
            Some("0")
        );
        assert_eq!(first_display("Display not-an-id"), None);
        assert_eq!(png_start(b"[Warning]\n\x89PNG\r\n\x1a\n"), Some(10));
        assert_eq!(png_start(b"screencap: not found"), None);
    }

    #[test]
    fn shell_only_device_does_not_enable_android_preview() {
        let linux = Arc::new(MockRunner {
            stdout: "sh: getprop: not found\n".into(),
            exit_code: 127,
            calls: Mutex::new(Vec::new()),
        });
        let client = AdbClient::new(linux.clone());
        let executable = std::env::current_exe().unwrap();
        assert!(
            client
                .android_preview(executable.to_str().unwrap(), "board")
                .is_none()
        );
        let calls = linux.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1,
            ["-s", "board", "shell", "getprop", "ro.build.version.sdk"]
        );
    }

    #[test]
    fn empty_and_header_only() {
        assert_eq!(parse_devices_l(""), [] as [duthub_core::AdbDevice; 0]);
        assert_eq!(
            parse_devices_l("List of devices attached\n\n"),
            [] as [duthub_core::AdbDevice; 0]
        );
    }

    #[test]
    fn client_uses_devices_dash_l_args() {
        let mock = Arc::new(MockRunner {
            stdout: FIXTURE.into(),
            exit_code: 0,
            calls: Mutex::new(Vec::new()),
        });
        let client = AdbClient::new(mock.clone());
        let executable = std::env::current_exe().unwrap();
        let devices = client.devices_l(executable.to_str().unwrap()).unwrap();
        assert_eq!(devices.len(), 3);
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls[0].1, vec!["devices".to_string(), "-l".to_string()]);
    }

    #[test]
    fn nonzero_exit_is_error() {
        let mock = Arc::new(MockRunner {
            stdout: String::new(),
            exit_code: 1,
            calls: Mutex::new(Vec::new()),
        });
        let client = AdbClient::new(mock);
        let executable = std::env::current_exe().unwrap();
        let err = client.devices_l(executable.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, DeviceError::Invalid(_)));
    }
}

/// `OpenHarmony` HDC (`HarmonyOS` Device Connector) target device.
#[derive(Debug, Clone, PartialEq)]
pub struct HdcTarget {
    /// Connection key (for example `COM5` or `127.0.0.1:5555`).
    pub key: String,
    /// Device state (for example `USB`, `CONN`, or `Update`).
    pub state: String,
}

/// `OpenHarmony` `hdc` CLI client (read-only discovery via `hdc list targets`).
pub struct HdcClient {
    runner: Arc<dyn ProcessRunner>,
}

impl std::fmt::Debug for HdcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HdcClient").finish_non_exhaustive()
    }
}

impl HdcClient {
    pub fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self { runner }
    }

    #[must_use]
    pub fn real() -> Self {
        Self::new(Arc::new(RealProcessRunner))
    }

    /// Run `hdc list targets` and parse the target list (read-only, no side effects).
    pub fn list_targets(&self, hdc_spec: &str) -> DeviceResult<Vec<HdcTarget>> {
        let exe = resolve_executable(hdc_spec)?;
        let result = self
            .runner
            .run(&exe, &["list".into(), "targets".into()], DEVICES_TIMEOUT)?;
        if result.exit_code != Some(0) {
            let detail = excerpt(&result.stderr, &result.stdout);
            return Err(DeviceError::Invalid(format!(
                "hdc 退出码 {:?}: {detail}",
                result.exit_code
            )));
        }
        Ok(parse_targets(&result.stdout))
    }
}

/// Parse `hdc list targets` output (`<connectKey> <state>`), ignoring `[Empty]` and blank lines.
#[must_use]
pub fn parse_targets(output: &str) -> Vec<HdcTarget> {
    let mut targets = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let mut parts = line.split_whitespace();
        if let Some(key) = parts.next() {
            let state = parts.next().unwrap_or("unknown").to_string();
            targets.push(HdcTarget {
                key: key.to_string(),
                state,
            });
        }
    }
    targets
}

#[cfg(test)]
mod hdc_tests {
    use super::*;

    #[test]
    fn parse_targets_skips_empty_and_bracket() {
        let out = "COM5\tUSB\n127.0.0.1:5555  CONN\n[Empty]\n\n";
        let parsed = parse_targets(out);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].key, "COM5");
        assert_eq!(parsed[0].state, "USB");
        assert_eq!(parsed[1].key, "127.0.0.1:5555");
        assert_eq!(parsed[1].state, "CONN");
    }

    #[test]
    fn parse_targets_empty_output() {
        assert_eq!(parse_targets(""), [] as [HdcTarget; 0]);
        assert_eq!(parse_targets("[Empty]"), [] as [HdcTarget; 0]);
    }
}
