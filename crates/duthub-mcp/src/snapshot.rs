//! 快照状态源：独立 stdio 进程读取 desktop 落盘的 `mcp-state.json`（只读）。
//!
//! 写操作在快照模式下不可用（无法回调 desktop 进程）——返回只读限制说明。
//! 写操作请连接 desktop 的 Streamable HTTP 端点（RuntimeMcpState）。

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::state::{
    AdbDeviceDto, ApprovalErr, ApprovalIssueResult, DutInfo, DutSummary, LogLineDto, LogReadResult,
    McpState, ProcessResultDto, RelayBrief, SerialBindInfo,
};

/// mcp-state.json 结构（desktop 落盘）
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
    /// 最近日志（desktop 截取）
    pub lines: Vec<SnapshotLine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotLine {
    pub seq: u64,
    pub text: String,
}

pub struct SnapshotState {
    snapshot: StateSnapshot,
}

impl SnapshotState {
    /// 从 JSON 文件加载；文件缺失 → 空快照（dut.list 返回空列表）
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
                session_lines: Some(d.lines.last().map(|l| l.seq).unwrap_or(0)),
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
        // 快照模式只读：不发令牌（token 占位使返回类型成立）
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

/// 构造共享 Arc（bin 使用）
pub fn snapshot_state(path: &Path) -> Arc<dyn McpState> {
    Arc::new(SnapshotState::load(path))
}

/// 常驻 stdio 进程的动态快照：每次工具调用前重读 `mcp-state.json`。
///
/// MCP stdio 客户端会把子进程当常驻进程用（启动一次、整个会话复用）；
/// desktop 每 2s 落盘新快照（mcp_runtime），固定快照会让数据永久停留在
/// 进程启动时刻。每请求重读保证工具返回当前状态。
pub struct ReloadingSnapshotState {
    path: std::path::PathBuf,
}

impl ReloadingSnapshotState {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    fn fresh(&self) -> SnapshotState {
        SnapshotState::load(&self.path)
    }

    /// 快照路径（`<工作目录>/runtime/mcp-state.json`）反推工作目录；
    /// 路径无父级（异常构造）返回 None。
    fn work_dir(&self) -> Option<std::path::PathBuf> {
        self.path
            .parent()
            .and_then(|runtime| runtime.parent())
            .map(|dir| dir.to_path_buf())
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

/// 构造动态重读快照（stdio bin 使用：常驻进程数据不冻结）
pub fn reloading_snapshot_state(path: &Path) -> Arc<dyn McpState> {
    Arc::new(ReloadingSnapshotState::new(path.to_path_buf()))
}

/// 快照来源抽象（desktop 通过共享内存实现，避免子进程重复读盘时的时序问题）
pub trait StateSnapshotSource: Send + Sync + 'static {
    fn load(&self) -> StateSnapshot;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloading_state_skill_export_writes_to_snapshot_work_dir() {
        // 工作目录 = <tmp>/<unique>；快照在其 runtime/ 下（与 bin discover_data_dir 同构）
        let work = std::env::temp_dir().join(format!(
            "duthub-skill-snap-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let snapshot = work.join("runtime").join("mcp-state.json");
        std::fs::create_dir_all(snapshot.parent().unwrap()).expect("runtime 目录");
        let state = ReloadingSnapshotState::new(snapshot);

        let path = std::path::PathBuf::from(state.skill_export().expect("导出成功"));
        assert!(
            path.ends_with(
                std::path::Path::new("skill")
                    .join("duthub")
                    .join("SKILL.md")
            ),
            "路径应为 <work>/skill/duthub/SKILL.md: {}",
            path.display()
        );
        let raw = std::fs::read_to_string(&path).expect("SKILL.md 落盘");
        assert!(raw.contains("name: duthub"));

        // 无父级的异常路径 → 明确报错而非 panic
        let bad = ReloadingSnapshotState::new(std::path::PathBuf::from("mcp-state.json"));
        assert!(bad.skill_export().is_err());
        let _ = std::fs::remove_dir_all(&work);
    }
}
