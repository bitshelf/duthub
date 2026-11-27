//! goose licence/release facts (compatibility surface).
//!
//! goose used to ship as an embedded asset (ADR-3). Since 2026-11-24 it is
//! installed from the "tool downloads" page through `duthub-tooldown`, the same
//! way adb and ffmpeg are: no goose bytes are compiled into duthub any more, so
//! there is nothing here to hash, install or unpack.
//!
//! Resolution lives in [`super::registry`] (configured external program >
//! downloaded install > PATH). This module only answers the questions the UI
//! still asks (licence text, notice, distribution record, Authenticode fact) and
//! states them honestly: goose is not bundled, its licence and release notes
//! come with the upstream package, and duthub never claims a signature status
//! for a program it did not verify.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// Recommended/minimum goose version.
///
/// This is the version gate applied to every downloaded or externally
/// configured goose (see [`super::registry::MIN_GOOSE_VERSION`], which reuses
/// this literal). It is no longer "the version of the embedded build": there is
/// no embedded build.
pub const GOOSE_VERSION: &str = "1.60.2";

/// The distribution record: how goose reaches this machine now.
pub const MANIFEST_JSON: &str = r#"{
  "id": "goose",
  "distribution": "downloaded",
  "embedded": false,
  "source": "https://github.com/aaif-goose/goose/releases/latest/download/goose-x86_64-pc-windows-msvc.zip",
  "installRoot": "<config dir>/tools/goose/",
  "license": "Apache-2.0",
  "note": "goose is installed from the tool-downloads page; no goose bytes ship with duthub."
}"#;

const NOT_EMBEDDED_STATUS: &str = "未内置（随上游发行包）";

const LICENSE_TEXT: &str = "\
goose 不再随 duthub 分发。duthub 在「工具下载」页从上游发行包下载安装 goose，\
运行的是用户机器上的那份程序，duthub 不复制、不修改它。\n\n\
goose 以 Apache License 2.0 授权，版权归 AAIF/goose 项目贡献者所有。\
完整许可文本随上游发行包提供（发行包内的 LICENSE 文件），也可以从上游仓库获取；\
duthub 不再内嵌该文本，因此这里只说明事实与来源。";

const NOTICE_TEXT: &str = "\
goose NOTICE：goose 由 AAIF/goose 项目维护，归属声明以上游发行包自带的 NOTICE 为准。\n\n\
duthub 不内嵌也不修改 goose 二进制：安装来自上游 release（「工具下载」页），\
版本门槛见 GOOSE_VERSION（推荐/最低版本），运行时按「已下载安装 > PATH」解析。";

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const VERSION_PROBE_LIMIT: usize = 4096;

#[derive(Debug)]
pub enum BundleError {
    Io(String),
    Manifest(String),
    Hash {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    Version(String),
    /// No build-time goose exists any more: the caller must resolve the
    /// downloaded install or PATH instead (see
    /// [`super::registry::goose_install_state`]).
    NotInstalled(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleError::Io(error) => write!(formatter, "goose 资产 I/O 失败: {error}"),
            BundleError::Manifest(error) => write!(formatter, "goose manifest 无效: {error}"),
            BundleError::Hash {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "goose 资产哈希不匹配（{}）：期望 {expected}，实际 {actual}",
                path.display()
            ),
            BundleError::Version(error) => write!(formatter, "goose 版本探测失败: {error}"),
            BundleError::NotInstalled(reason) => write!(formatter, "goose 未安装: {reason}"),
        }
    }
}

impl std::error::Error for BundleError {}

/// Parse [`MANIFEST_JSON`]: the distribution record shown to operators.
///
/// There is no build-time binary to cross-check, so this only rejects a
/// malformed record (`manifest_json_is_a_parseable_distribution_record` pins the
/// constant itself).
pub fn manifest() -> Result<Value, BundleError> {
    serde_json::from_str(MANIFEST_JSON).map_err(|error| BundleError::Manifest(error.to_string()))
}

/// goose licence statement: not bundled, licence ships with the upstream
/// package (Apache-2.0). Replaces the former embedded `LICENSE` full text.
#[must_use]
pub fn license_text() -> &'static str {
    LICENSE_TEXT
}

/// goose attribution statement: not bundled, NOTICE ships with the upstream
/// package. Replaces the former embedded `NOTICE` file.
#[must_use]
pub fn notice_text() -> &'static str {
    NOTICE_TEXT
}

/// The distribution record as raw JSON (`embedded: false`, upstream source).
#[must_use]
pub fn manifest_json() -> &'static str {
    MANIFEST_JSON
}

/// Authenticode fact for a goose binary duthub did not verify.
///
/// Nothing is embedded any more, so this cannot report a manifest-recorded
/// signature status; it must not claim "`NotSigned`" either, which would assert
/// the absence of a signature that nobody checked.
#[must_use]
pub fn authenticode_status() -> &'static str {
    NOT_EMBEDDED_STATUS
}

/// Resolve the program to execute.
///
/// - `over = Some((path, sha256))`: a user-configured external program; the full
///   SHA-256 is recomputed at launch and a mismatch refuses to run.
/// - `over = None`: there is no embedded copy any more. This returns
///   [`BundleError::NotInstalled`] on purpose, so callers route to the
///   downloadable install ([`super::registry::goose_install_state`]) instead of
///   unwrapping a path that does not exist.
///
/// `work_dir` is kept for source compatibility with the former embedded install
/// (which unpacked into `<work_dir>/agent/bin/goose/`); nothing is written here
/// now. Installed tools live under `<config dir>/tools/goose/` and are owned by
/// the `duthub-tooldown` install kernel.
pub fn resolve(work_dir: &Path, over: Option<(&Path, &str)>) -> Result<PathBuf, BundleError> {
    match over {
        Some((path, expected)) => {
            verify_override(path, expected)?;
            Ok(path.to_path_buf())
        }
        None => Err(BundleError::NotInstalled(format!(
            "内嵌 goose 已移除（2026-11-24）：{} 不再安装构建期资产；\
             内置 goose 由 registry 按「已下载安装 > PATH」解析，请先完成工具下载",
            work_dir.display()
        ))),
    }
}

fn verify_override(path: &Path, expected: &str) -> Result<(), BundleError> {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BundleError::Manifest(
            "外部 goose 路径的 SHA-256 必须是 64 位十六进制".into(),
        ));
    }
    let metadata = std::fs::metadata(path).map_err(|error| {
        BundleError::Io(format!(
            "外部 goose 路径不可用（{}）: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(BundleError::Io(format!(
            "外部 goose 路径不是文件: {}",
            path.display()
        )));
    }
    let actual = duthub_core::store::sha256_file(path)
        .map_err(|error| BundleError::Io(format!("读取外部 goose 哈希失败: {error}")))?;
    if actual != expected {
        return Err(BundleError::Hash {
            path: path.to_path_buf(),
            expected,
            actual,
        });
    }
    Ok(())
}

/// Run `<goose> --version` (5s timeout, no console window), return the first
/// non-empty line.
///
/// The general `goose.exe` writes a CLI log on every invocation, including
/// `--version`. That probe is duthub's own trusted call (outside the agent
/// sandbox), so it must not leak into the user profile: `GOOSE_PATH_ROOT` points
/// at a scratch directory next to the binary (temp dir as fallback) and keyring
/// access stays off.
pub fn probe_version(path: &Path) -> Result<String, BundleError> {
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
        ("GOOSE_DISABLE_KEYRING".into(), "1".into()),
        ("GOOSE_TELEMETRY_OPTOUT".into(), "1".into()),
    ];
    if let Some(root) = probe_root(path) {
        env.push(("GOOSE_PATH_ROOT".into(), root.into_os_string()));
    }
    let output = duthub_core::process::run_binary_with_env(
        path,
        &["--version".to_owned()],
        &env,
        VERSION_PROBE_TIMEOUT,
        VERSION_PROBE_LIMIT,
    )
    .map_err(|error| BundleError::Version(error.to_string()))?;
    let text = String::from_utf8_lossy(&output);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| BundleError::Version("--version 没有输出".into()))
}

/// Scratch `GOOSE_PATH_ROOT` for the version probe: a sibling of the binary so
/// probe logs stay next to the installed tool, with the OS temp dir as the
/// fallback when that location is not writable (an external goose under
/// `Program Files`, for example).
fn probe_root(path: &Path) -> Option<PathBuf> {
    let sibling = path.parent()?.join(".goose-probe");
    if std::fs::create_dir_all(&sibling).is_ok() {
        return Some(sibling);
    }
    let fallback = std::env::temp_dir().join("duthub-goose-probe");
    std::fs::create_dir_all(&fallback).ok()?;
    Some(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_json_is_a_parseable_distribution_record() {
        let record = manifest().expect("manifest JSON 必须可解析");
        assert_eq!(record["id"], "goose");
        assert_eq!(record["distribution"], "downloaded");
        assert_eq!(record["embedded"], false);
        assert_eq!(record["license"], "Apache-2.0");
        assert!(
            record["source"]
                .as_str()
                .is_some_and(|url| url.contains("aaif-goose/goose")),
            "来源必须是上游 release：{record}"
        );
    }

    #[test]
    fn compat_accessors_report_not_embedded() {
        let status = authenticode_status();
        assert!(status.contains("未内置"), "{status}");
        assert!(
            !status.eq_ignore_ascii_case("NotSigned") && !status.eq_ignore_ascii_case("Valid"),
            "未内置时不得宣称任何签名结论: {status}"
        );

        let license = license_text();
        assert!(license.contains("Apache License 2.0"), "{license}");
        assert!(license.contains("上游发行包"), "{license}");
        assert!(
            !license.contains("TERMS AND CONDITIONS"),
            "不得再内嵌 Apache 全文"
        );

        let notice = notice_text();
        assert!(notice.contains("AAIF/goose"), "{notice}");
        assert!(notice.contains("上游发行包"), "{notice}");
    }

    #[test]
    fn resolve_without_an_override_reports_not_installed() {
        let work = std::env::temp_dir().join(format!(
            "duthub-bundle-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&work).unwrap();
        let error = resolve(&work, None).expect_err("没有内嵌资产可解析");
        assert!(matches!(error, BundleError::NotInstalled(_)), "{error}");
        assert!(error.to_string().contains("内嵌 goose 已移除"), "{error}");
        assert!(
            !work.join("agent").exists(),
            "resolve 不得再往工作目录安装任何资产"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn resolve_verifies_an_explicit_override() {
        let work = std::env::temp_dir().join(format!(
            "duthub-bundle-override-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&work).unwrap();
        let exe = work.join("goose-candidate.exe");
        std::fs::write(&exe, b"candidate-bytes").unwrap();
        let sha = duthub_core::store::sha256_hex(b"candidate-bytes");
        assert_eq!(resolve(&work, Some((&exe, &sha))).unwrap(), exe);
        let wrong = "0".repeat(64);
        let error = resolve(&work, Some((&exe, &wrong))).expect_err("哈希不匹配必须拒绝");
        assert!(matches!(error, BundleError::Hash { .. }), "{error}");
        let _ = std::fs::remove_dir_all(&work);
    }
}
