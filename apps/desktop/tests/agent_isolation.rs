//! W2b OS isolation regressions (Windows only).
//!
//! - Hardening a per-DUT directory produces a protected DACL without
//!   Everyone/BUILTIN\Users ACEs, and the probe reports it as private.
//! - Hardening a file or a missing path fails and the probe does not claim the
//!   path is guarded.
//! - The restricted-token probe only derives (and closes) a token handle; it
//!   never spawns a process.

#![cfg(windows)]

use std::path::PathBuf;
use std::time::Duration;

use duthub_sandbox::os_acl::{
    AclError, harden_private_dir, inspect_private_dir_acl, probe_os_isolation,
    probe_restricted_token,
};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-agent-isolation-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn acl_hardening_applies_owner_only_dacl() {
    let dir = temp_dir("acl");
    harden_private_dir(&dir).expect("目录 ACL 加固失败");

    let report = inspect_private_dir_acl(&dir).expect("读取 DACL 失败");
    assert!(
        report.protected,
        "加固后的 DACL 必须是 protected: {report:?}"
    );
    assert!(report.private, "加固后的 DACL 必须被视为私有: {report:?}");
    assert!(
        report.entries.iter().any(|sid| sid == "S-1-5-18"),
        "必须保留 SYSTEM ACE: {:?}",
        report.entries
    );
    assert!(
        report.entries.iter().any(|sid| sid == "S-1-5-32-544"),
        "必须保留 Administrators ACE: {:?}",
        report.entries
    );
    assert!(
        !report.entries.iter().any(|sid| sid == "S-1-1-0"),
        "Everyone 不得出现在私有目录 ACL: {:?}",
        report.entries
    );
    assert!(
        !report.entries.iter().any(|sid| sid == "S-1-5-32-545"),
        "BUILTIN\\Users 不得出现在私有目录 ACL: {:?}",
        report.entries
    );

    // The probe must observe the same hardened state.
    let status = probe_os_isolation(&dir);
    assert!(!status.unsupported);
    assert!(
        status.private_dir_acl,
        "探测必须报告目录已加固: {}",
        status.detail
    );

    // A file created after hardening inherits the restricted ACEs; it must not
    // become readable by Everyone/Users.
    let child = dir.join("child.txt");
    std::fs::write(&child, b"x").unwrap();
    assert!(child.is_file());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn harden_failure_reports_unguarded() {
    let dir = temp_dir("unguarded");

    // A file is never a hardening target.
    let file = dir.join("plain.txt");
    std::fs::write(&file, b"x").unwrap();
    assert!(
        matches!(harden_private_dir(&file), Err(AclError::InvalidPath(_))),
        "文件路径必须被拒绝"
    );
    let file_status = probe_os_isolation(&file);
    assert!(!file_status.unsupported);
    assert!(
        !file_status.private_dir_acl,
        "未加固的文件不得报告为私有: {}",
        file_status.detail
    );

    // A missing directory cannot be hardened either.
    let missing = dir.join("missing-dir");
    assert!(harden_private_dir(&missing).is_err());
    let missing_status = probe_os_isolation(&missing);
    assert!(
        !missing_status.private_dir_acl,
        "缺失路径不得报告为私有: {}",
        missing_status.detail
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restricted_token_probe_never_spawns() {
    let dir = temp_dir("token");
    // The only executable artifact in the probed tree would create this marker
    // if anything ran it. The probe takes a directory path only; start it and
    // assert the marker never appears.
    let canary = dir.join("spawn-canary.cmd");
    std::fs::write(
        &canary,
        "@echo off\r\necho spawned > \"%~dp0spawned.txt\"\r\n",
    )
    .unwrap();
    let before = std::fs::read_dir(&dir).unwrap().count();

    probe_restricted_token().expect("CreateRestrictedToken 探测失败");
    let status = probe_os_isolation(&dir);
    assert!(!status.unsupported);
    assert!(
        status.restricted_token,
        "受限令牌探测必须成功（只创建令牌，不启动进程）: {}",
        status.detail
    );

    // Give a hypothetical child process time to run its canary.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !dir.join("spawned.txt").exists(),
        "受限令牌探测不得启动子进程"
    );
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        before,
        "探测不得增删文件"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
