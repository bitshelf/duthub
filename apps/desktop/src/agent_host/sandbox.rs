//! 每 DUT Agent 的目录布局（W2）。
//!
//! `<work_dir>/agent/dut/<hex(dut_id)>/`
//! - `goose/`      → GOOSE_PATH_ROOT（config/、data/sessions/）
//! - `sandbox/`    → ACP `session/new.cwd`（会话工作目录）
//! - `sandbox/tmp/` → 子进程 TEMP/TMP（不继承用户 TEMP）
//! - `transcript.jsonl`
//! - `session-id` (legacy resume marker)
//! - `resume.json` (`agentId` + `programSha256` + `sessionId`)
//!
//! 目录逐级经 `duthub_core::store::ensure_agent_dir`（cap-std；junction/symlink
//! 与非法路径段一律拒绝）。
//!
//! **范围声明**：这里做的是**路径收敛**（把 goose 的配置、会话与临时文件压进
//! 每 DUT 目录），不是 OS 级强制边界——子进程仍可能通过绝对路径访问用户目录。
//! 受限令牌（只挡写）、专用账户 + NTFS ACL、AppContainer 仍 deferred；
//! 见 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md` §5.1。

use std::path::{Path, PathBuf};

use duthub_core::store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    pub dut_dir: PathBuf,
    pub sandbox: PathBuf,
    /// 子进程 TEMP/TMP：`<sandbox>/tmp`（见 [`temp_dir`]）。
    pub temp: PathBuf,
    pub goose_root: PathBuf,
    pub transcript: PathBuf,
    pub session_id: PathBuf,
    /// Structured resume marker (`agentId` + `programSha256` + `sessionId`).
    pub resume: PathBuf,
}

#[derive(Debug)]
pub enum SandboxError {
    InvalidDutId,
    Io(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::InvalidDutId => write!(formatter, "DUT id 为空，无法建立 Agent 沙箱"),
            SandboxError::Io(error) => write!(formatter, "Agent 沙箱目录不可用: {error}"),
        }
    }
}

impl std::error::Error for SandboxError {}

pub fn prepare(work_dir: &Path, dut_id: &str) -> Result<AgentPaths, SandboxError> {
    if dut_id.trim().is_empty() {
        return Err(SandboxError::InvalidDutId);
    }
    let dut_dir = store::agent_dut_dir(work_dir, dut_id)
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let sandbox = store::ensure_agent_dir(&dut_dir, &["sandbox"])
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    let temp = temp_dir(&sandbox)?;
    let goose_root = store::ensure_agent_dir(&dut_dir, &["goose"])
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    Ok(AgentPaths {
        transcript: dut_dir.join("transcript.jsonl"),
        session_id: dut_dir.join("session-id"),
        resume: dut_dir.join("resume.json"),
        dut_dir,
        sandbox,
        temp,
        goose_root,
    })
}

/// 子进程 TEMP/TMP 目录：`<sandbox>/tmp`。
///
/// 只做**路径收敛**，不是 OS 强制边界（受限令牌 / AppContainer 仍 deferred）：
/// 子进程拿到的 TEMP/TMP 指向该 DUT 的 sandbox，不再继承用户 `%TEMP%`。
/// 目录先创建并通过与 [`prepare`] 相同的 cap-std 校验（junction/symlink 与
/// 非目录一律拒绝），再返回普通路径。
pub fn temp_dir(sandbox: &Path) -> Result<PathBuf, SandboxError> {
    let dir = store::ensure_agent_dir(sandbox, &["tmp"])
        .map_err(|error| SandboxError::Io(error.to_string()))?;
    // ensure_agent_dir 已拒绝 junction/symlink；这里对最终路径再做一次显式
    // 普通目录校验，避免调用方单独使用本函数时把非目录/重解析点当 TEMP。
    let metadata =
        std::fs::symlink_metadata(&dir).map_err(|error| SandboxError::Io(error.to_string()))?;
    if !metadata.is_dir() || is_redirect(&metadata) {
        return Err(SandboxError::Io(format!(
            "TEMP 路径不是普通目录: {}",
            dir.display()
        )));
    }
    Ok(dir)
}

#[cfg(windows)]
fn is_redirect(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_redirect(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_work_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("duthub-sandbox-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prepares_versioned_per_dut_layout() {
        let work = temp_work_dir("layout");
        let paths = prepare(&work, "dut-A").unwrap();
        assert!(paths.sandbox.is_dir());
        assert!(paths.goose_root.is_dir());
        assert!(paths.temp.is_dir(), "TEMP/TMP 目录必须预先创建");
        assert_eq!(paths.temp, paths.sandbox.join("tmp"));
        let key = store::agent_dut_key("dut-A").unwrap();
        assert!(paths.dut_dir.ends_with(format!("agent/dut/{key}")));
        assert!(paths.transcript.ends_with("transcript.jsonl"));
        assert!(paths.session_id.ends_with("session-id"));
        assert!(paths.resume.ends_with("resume.json"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn temp_dir_rejects_non_directory() {
        let work = temp_work_dir("temp-file");
        let paths = prepare(&work, "dut-temp").unwrap();
        // 模拟攻击面：把 tmp 换成普通文件后，temp_dir 必须拒绝而不是静默使用。
        std::fs::remove_dir(&paths.temp).unwrap();
        std::fs::write(&paths.temp, b"not a directory").unwrap();
        assert!(matches!(temp_dir(&paths.sandbox), Err(SandboxError::Io(_))));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn rejects_empty_dut_id() {
        let work = temp_work_dir("empty");
        assert!(matches!(
            prepare(&work, "   "),
            Err(SandboxError::InvalidDutId)
        ));
        let _ = std::fs::remove_dir_all(&work);
    }
}
