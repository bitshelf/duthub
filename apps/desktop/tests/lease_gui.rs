//! W3 验收（F10）：桌面租约链必须启用 GUI 会话生命周期。
//!
//! 两条断言：
//! 1. `src/main.rs` 的共享 `ControlLeaseManager` 与 `DeviceService` 同口径调用
//!    `.with_gui_session_lifetime()` —— 漏掉时 GUI 自动租约会被 TTL 过期，
//!    「GUI 在场 → 拒 Agent」的防线在 TTL 之后失效（`--open-port` 直连实例）。
//! 2. 启用该模式后，GUI 租约不随 TTL 过期、剩余时间恒为 `None`、Agent 接管保持
//!    Busy、GUI 写入复核放行；释放后恢复 Free。

use std::path::PathBuf;
use std::time::Duration;

use duthub_core::{
    ControlLeaseManager, LeaseAuthority, LeaseError, LeaseHolder, LeasePhase, WriteIdentity,
};

fn manifest_dir() -> PathBuf {
    // 坑 #4：运行期读取，不能 env!（编译期烘焙绝对路径，目录改名即失效）。
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
}

fn agent_holder() -> LeaseHolder {
    LeaseHolder::Agent {
        agent_name: "test-agent".into(),
        credential_id: "cred-test".into(),
    }
}

#[test]
fn main_wires_gui_session_lifetime_onto_the_shared_lease_manager() {
    let main = std::fs::read_to_string(manifest_dir().join("src").join("main.rs"))
        .expect("read apps/desktop/src/main.rs");
    let construction = main
        .find("ControlLeaseManager::new")
        .expect("main.rs 必须构造 ControlLeaseManager");
    let gui_lifetime = main
        .find(".with_gui_session_lifetime()")
        .expect("F10：main.rs 的 ControlLeaseManager 链缺少 .with_gui_session_lifetime()");
    assert!(
        gui_lifetime > construction,
        "with_gui_session_lifetime 必须接在控制租约管理器构造链上"
    );
    // main.rs injects the shared ControlLeaseManager (with GUI session lifetime)
    // directly into DeviceService via `with_lease_authority`, keeping one unified
    // lease authority across desktop and device layers.
    assert!(
        main.contains(".with_lease_authority(lease.clone())"),
        "main.rs must wire the shared ControlLeaseManager into DeviceService"
    );
}

#[test]
fn gui_session_lease_survives_ttl_and_keeps_blocking_agent_takeover() {
    let manager = ControlLeaseManager::new(None)
        .with_ttl(Duration::from_millis(40))
        .with_gui_session_lifetime();
    let grant = manager
        .acquire("dut-w3", &LeaseHolder::Gui, None)
        .expect("GUI acquire");
    // 双保险之一：GUI 授予的到期时刻为 u64::MAX。
    assert_eq!(grant.expires_at_unix_ms, u64::MAX);

    std::thread::sleep(Duration::from_millis(80));

    // status 的惰性清理不得清掉 GUI 租约；剩余时间不暴露 TTL。
    let status = manager.status("dut-w3");
    assert_eq!(status.phase, LeasePhase::Held);
    assert_eq!(status.remaining_secs, None);

    // Agent 接管保持 Busy（防线在 TTL 之外仍生效）。
    let error = manager
        .acquire("dut-w3", &agent_holder(), None)
        .expect_err("GUI 在场时 Agent 必须 Busy");
    assert!(matches!(error, LeaseError::Busy { .. }), "{error:?}");

    // GUI 写入复核仍放行（admit 路径不受 TTL 影响）。
    assert!(
        manager.admit_write("dut-w3", &WriteIdentity::gui()).is_ok(),
        "GUI 写入必须放行"
    );

    // 释放路径不变：GUI 释放后恢复 Free。
    manager.release_for_holder("dut-w3", &LeaseHolder::Gui);
    assert_eq!(manager.status("dut-w3").phase, LeasePhase::Free);
}
