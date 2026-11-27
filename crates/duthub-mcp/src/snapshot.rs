//! Read-only snapshot state for an MCP stdio subprocess.
//!
//! The snapshot cannot dispatch writes because it has no callback to the live desktop.
//! Clients that need device writes connect to the desktop's Streamable HTTP listener.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, DutInfo, DutSummary, LogLineDto, LogReadResult,
    McpState, ProcessResultDto, RelayBrief, SerialBindInfo,
};

/// Snapshot file written by the desktop application.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub version: u32,
    pub ts_unix_ms: u64,
    pub duts: Vec<SnapshotDut>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_bindings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDut {
    pub dut_id: String,
    pub name: String,
    pub source: String,
    pub serial: SerialBindInfo,
    pub adb_serial: Option<String>,
    pub relay: Option<RelayBrief>,
    pub mcp_label: Option<String>,
    /// Recent log entries retained by the desktop.
    pub lines: Vec<SnapshotLine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotLine {
    pub seq: u64,
    pub text: String,
}

#[derive(Debug)]
pub struct SnapshotState {
    snapshot: StateSnapshot,
}

impl SnapshotState {
    /// Load the snapshot from JSON. A missing file yields an empty DUT list.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let snapshot = match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                eprintln!(
                    "[duthub-mcp] invalid snapshot {} at {}:{}",
                    path.display(),
                    error.line(),
                    error.column()
                );
                StateSnapshot::default()
            }),
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("[duthub-mcp] reading snapshot {}: {error}", path.display());
                }
                StateSnapshot::default()
            }
        };
        Self { snapshot }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self {
            snapshot: StateSnapshot::default(),
        }
    }
}

impl McpState for SnapshotState {
    fn list_duts(&self) -> Vec<DutSummary> {
        self.snapshot
            .duts
            .iter()
            .map(|d| DutSummary {
                dut_id: d.dut_id.clone(),
                name: d.name.clone(),
                source: d.source.clone(),
                port: d.serial.port.clone(),
                baud: d.serial.baud,
            })
            .collect()
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        self.snapshot
            .duts
            .iter()
            .find(|d| d.dut_id == dut_id)
            .map(|d| DutInfo {
                dut_id: d.dut_id.clone(),
                name: d.name.clone(),
                source: d.source.clone(),
                serial: d.serial.clone(),
                adb_serial: d.adb_serial.clone(),
                relay: d.relay.clone(),
                mcp_label: d.mcp_label.clone(),
                session_lines: Some(d.lines.last().map_or(0, |l| l.seq)),
            })
    }

    fn serial_read(&self, dut_id: &str, since_seq: u64, limit: usize) -> Option<LogReadResult> {
        let dut = self.snapshot.duts.iter().find(|d| d.dut_id == dut_id)?;
        if dut.source == "idle" {
            return None;
        }
        let tail = dut.lines.last().map_or(0, |line| line.seq);
        let since = if since_seq > tail { 0 } else { since_seq };
        let lines: Vec<LogLineDto> = dut
            .lines
            .iter()
            .filter(|l| l.seq > since)
            .take(limit)
            .map(|l| LogLineDto {
                seq: l.seq,
                text: l.text.clone(),
            })
            .collect();
        Some(LogReadResult {
            dut_id: dut.dut_id.clone(),
            session_id: Some(format!("sess-{}", dut.dut_id)),
            last_seq: lines.last().map_or(since, |line| line.seq),
            lines,
        })
    }

    fn serial_write(
        &self,
        _dut_id: &str,
        _text: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err(
            "stdio 快照模式为只读；写操作请连接 desktop 的 Streamable HTTP 端点（回环或已开启的局域网地址）"
                .into(),
        )
    }

    fn adb_list(&self) -> Vec<AdbDeviceDto> {
        Vec::new()
    }

    fn adb_shell(
        &self,
        _dut_id: &str,
        _argv: &[String],
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        Err("stdio 快照模式为只读；adb.shell 请连接 desktop 的 Streamable HTTP 端点".into())
    }

    fn control_power(
        &self,
        _dut_id: &str,
        _on: bool,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("stdio 快照模式为只读；控制操作请连接 desktop 的 Streamable HTTP 端点".into())
    }

    fn control_reset(
        &self,
        _dut_id: &str,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("stdio 快照模式为只读；控制操作请连接 desktop 的 Streamable HTTP 端点".into())
    }

    fn control_power_key(
        &self,
        _dut_id: &str,
        _long_press: bool,
        _approval_token: &str,
        _writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        Err("stdio 快照模式为只读；控制操作请连接 desktop 的 Streamable HTTP 端点".into())
    }

    fn flash_test(&self, _preset_id: &str) -> Result<ProcessResultDto, String> {
        Err("stdio 快照模式为只读；flash.test 请连接 desktop 的 Streamable HTTP 端点".into())
    }

    fn approval_issue(
        &self,
        _dut_id: &str,
        _action: &str,
        _args_json: &str,
    ) -> ApprovalIssueResult {
        // Snapshot mode cannot issue usable approval tokens.
        ApprovalIssueResult {
            token: String::new(),
            dut_id: String::new(),
            action: String::new(),
            expires_in_secs: 0,
        }
    }

    fn approval_validate(
        &self,
        _token: &str,
        _dut_id: &str,
        _action: &str,
        _args_json: &str,
    ) -> Result<(), ApprovalErr> {
        Err(ApprovalErr {
            kind: duthub_core::ApprovalError::Unknown,
        })
    }
}

/// Create a shared snapshot state for the stdio executable.
#[must_use]
pub fn snapshot_state(path: &Path) -> Arc<dyn McpState> {
    Arc::new(SnapshotState::load(path))
}

/// State source that reloads the snapshot for each request.
///
/// A client can keep the same stdio subprocess alive for an entire session.
/// The desktop updates the persisted snapshot periodically.
/// Reloading avoids freezing tool results at subprocess startup.
#[derive(Debug)]
pub struct ReloadingSnapshotState {
    path: std::path::PathBuf,
}

impl ReloadingSnapshotState {
    #[must_use]
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    fn fresh(&self) -> SnapshotState {
        SnapshotState::load(&self.path)
    }

    /// Derive the work directory from the snapshot file under its runtime directory.
    /// Return no work directory if the path has no parent.
    fn work_dir(&self) -> Option<std::path::PathBuf> {
        self.path
            .parent()
            .and_then(|runtime| runtime.parent())
            .map(std::path::Path::to_path_buf)
    }
}

impl McpState for ReloadingSnapshotState {
    fn log_archive_dir(&self) -> Option<std::path::PathBuf> {
        self.work_dir()
    }
    fn list_duts(&self) -> Vec<DutSummary> {
        self.fresh().list_duts()
    }

    fn dut_info(&self, dut_id: &str) -> Option<DutInfo> {
        self.fresh().dut_info(dut_id)
    }

    fn serial_read(&self, dut_id: &str, since_seq: u64, limit: usize) -> Option<LogReadResult> {
        self.fresh().serial_read(dut_id, since_seq, limit)
    }

    fn serial_write(
        &self,
        dut_id: &str,
        text: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        self.fresh().serial_write(dut_id, text, writer)
    }

    fn adb_list(&self) -> Vec<AdbDeviceDto> {
        self.fresh().adb_list()
    }

    fn adb_shell(
        &self,
        dut_id: &str,
        argv: &[String],
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<ProcessResultDto, String> {
        self.fresh().adb_shell(dut_id, argv, writer)
    }

    fn control_power(
        &self,
        dut_id: &str,
        on: bool,
        approval_token: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        self.fresh()
            .control_power(dut_id, on, approval_token, writer)
    }

    fn control_reset(
        &self,
        dut_id: &str,
        approval_token: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        self.fresh().control_reset(dut_id, approval_token, writer)
    }

    fn control_power_key(
        &self,
        dut_id: &str,
        long_press: bool,
        approval_token: &str,
        writer: duthub_core::lease::WriteIdentity,
    ) -> Result<(), String> {
        self.fresh()
            .control_power_key(dut_id, long_press, approval_token, writer)
    }

    fn flash_test(&self, preset_id: &str) -> Result<ProcessResultDto, String> {
        self.fresh().flash_test(preset_id)
    }

    fn skill_export(&self) -> Result<String, String> {
        let dir = self
            .work_dir()
            .ok_or_else(|| "快照路径缺少父目录，无法定位工作目录".to_string())?;
        crate::skill::export_skill_for_listener(&dir, None)
            .map(|path| path.display().to_string())
            .map_err(|err| err.to_string())
    }

    fn approval_issue(&self, dut_id: &str, action: &str, args_json: &str) -> ApprovalIssueResult {
        self.fresh().approval_issue(dut_id, action, args_json)
    }

    fn approval_validate(
        &self,
        token: &str,
        dut_id: &str,
        action: &str,
        args_json: &str,
    ) -> Result<(), ApprovalErr> {
        self.fresh()
            .approval_validate(token, dut_id, action, args_json)
    }
}

/// Create a state source for a long-running stdio subprocess.
#[must_use]
pub fn reloading_snapshot_state(path: &Path) -> Arc<dyn McpState> {
    Arc::new(ReloadingSnapshotState::new(path.to_path_buf()))
}

/// State capture interface also implemented by the desktop's shared memory state.
pub trait StateSnapshotSource: Send + Sync + 'static {
    fn load(&self) -> StateSnapshot;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloading_state_skill_export_writes_to_snapshot_work_dir() {
        // Use a temporary work directory containing the same runtime layout as the desktop.
        let work = std::env::temp_dir().join(format!(
            "duthub-skill-snap-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or_default(|d| d.as_nanos())
        ));
        let snapshot = work.join("runtime").join("mcp-state.json");
        std::fs::create_dir_all(snapshot.parent().unwrap()).expect("create runtime directory");
        let state = ReloadingSnapshotState::new(snapshot);

        let path = std::path::PathBuf::from(state.skill_export().expect("skill export succeeded"));
        assert!(
            path.ends_with(
                std::path::Path::new("skill")
                    .join("duthub")
                    .join("SKILL.md")
            ),
            "expected <work>/skill/duthub/SKILL.md, got: {}",
            path.display()
        );
        let raw = std::fs::read_to_string(&path).expect("SKILL.md written to disk");
        assert!(raw.contains("name: duthub"));

        // An invalid snapshot path must return an error.
        let bad = ReloadingSnapshotState::new(std::path::PathBuf::from("mcp-state.json"));
        assert!(bad.skill_export().is_err());
        let _ = std::fs::remove_dir_all(&work);
    }
}
