//! 写入通道租约复核集成测试（方案 N6；docs/SECURITY.md）：
//! GUI 打开会话自动持有租约 / 关闭释放 / GUI 持有期间 Agent 写被拒（INV-01）/
//! System（心跳）放行 / 过期代数 admit 拒（INV-04 [D] 侧）/ 审计 write.deny /
//! 控制入口（ControlService）同规则复核。
//!
//! [E] actor 侧复核（demo 会话）见 `src/demo.rs::actor_recheck_drops_stale_generation_write`。

use std::sync::Arc;
use std::time::Duration;

use duthub_core::ApprovalManager;
use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_core::lease::{LeaseHolder, LeasePhase, WriteIdentity};
use duthub_core::model::{DutConfig, McpConfig, RelayChannels, RelayConfig, SerialConfig};
use duthub_device::control::{ControlAction, ControlService};
use duthub_device::{DeviceService, SessionKind};

fn service_with_lease(
    tag: &str,
) -> (
    DeviceService,
    Arc<duthub_core::lease::ControlLeaseManager>,
    Arc<AuditSink>,
) {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target/test-tmp/lease-admission")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sink = AuditSink::memory();
    let mgr = Arc::new(duthub_core::lease::ControlLeaseManager::new(Some(
        sink.clone(),
    )));
    let service = DeviceService::custom(256, dir, 0)
        .with_lease_authority(mgr.clone())
        .with_audit_sink(sink.clone());
    (service, mgr, sink)
}

fn agent(name: &str) -> LeaseHolder {
    LeaseHolder::Agent {
        agent_name: name.into(),
        credential_id: format!("cred-{name}"),
    }
}

/// GUI 交互路径：打开会话即自动持有租约，关闭释放；期间 Agent 写被拒、
/// System（心跳）放行（INV-01 双写互斥在写入通道的执行点）。
#[test]
fn gui_session_auto_holds_lease_and_blocks_agent_writes() {
    let (service, mgr, sink) = service_with_lease("auto-hold");
    service
        .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    // 打开即 GUI 持有
    let status = mgr.status("dut-a");
    assert_eq!(status.phase, LeasePhase::Held);
    assert_eq!(status.holder.as_deref(), Some("GUI（人类在场）"));
    // Agent 写（auth 关过渡无租约形态）→ GUI 优先必拒（INV-01）
    let err = service
        .write_line("dut-a", WriteIdentity::agent(None, None), "agent-cmd")
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("GUI"), "拒绝消息必须指明 GUI 在场：{msg}");
    // Agent 带（无效）租约句柄写 → 同样拒
    assert!(
        service
            .write_line(
                "dut-a",
                WriteIdentity::agent(Some("lease-nope".into()), Some(9)),
                "agent-cmd2"
            )
            .is_err()
    );
    // GUI 自己写放行（自动租约上下文）
    service
        .write_line("dut-a", WriteIdentity::gui(), "gui-cmd")
        .unwrap();
    // System（心跳）放行
    service
        .write_line("dut-a", WriteIdentity::system(), "uname -a")
        .unwrap();
    // GUI 持有期间 Agent acquire 被拒（Busy → GUI）
    assert!(matches!(
        mgr.acquire("dut-a", &agent("claude"), None),
        Err(duthub_core::lease::LeaseError::Busy { .. })
    ));
    // 拒绝落审计 write.deny
    assert!(sink.recent(200).iter().any(|e| e.action == "write.deny"
        && e.dut.as_deref() == Some("dut-a")
        && e.result == AuditOutcome::Denied));
    // 关闭会话 → 自动释放（人类离场）
    service.close_session("dut-a").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(mgr.status("dut-a").phase, LeasePhase::Free);
    // 释放后 Agent 立即能取到控制权
    mgr.acquire("dut-a", &agent("claude"), None).unwrap();
}

/// GUI 关闭会话释放后，Agent 取得租约期间 GUI 写被拒（互斥对称性）。
#[test]
fn agent_controlled_gui_write_denied_until_takeover() {
    let (service, mgr, _sink) = service_with_lease("agent-holds");
    // Agent 先取控制权（无 GUI 会话）
    let grant = mgr.acquire("dut-a", &agent("claude"), None).unwrap();
    // GUI 打开会话：仍成功（读不受限），但拿不到租约
    service
        .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(mgr.status("dut-a").holder.as_deref(), Some("Agent claude"));
    // GUI 写 → 拒（指向 Agent）
    let err = service
        .write_line("dut-a", WriteIdentity::gui(), "gui-cmd")
        .unwrap_err();
    assert!(err.to_string().contains("claude"), "{err}");
    // Agent 本人凭 (lease_id, generation) 写 → 放行
    service
        .write_line(
            "dut-a",
            WriteIdentity::agent(Some(grant.lease_id.clone()), Some(grant.generation)),
            "agent-cmd",
        )
        .unwrap();
    // GUI 强制接管（SPEC §6.7）→ 代数 +1 → Agent 旧代数写 admit 拒（INV-04 [D] 侧）
    let takeover = mgr.force_takeover("dut-a").unwrap();
    assert!(takeover.generation > grant.generation);
    let stale = service.write_line(
        "dut-a",
        WriteIdentity::agent(Some(grant.lease_id.clone()), Some(grant.generation)),
        "stale-agent-cmd",
    );
    assert!(stale.is_err(), "旧代数写入必须在入口被拒");
    // 接管后 GUI 写放行
    service
        .write_line("dut-a", WriteIdentity::gui(), "gui-cmd-2")
        .unwrap();
    service.close_session("dut-a").unwrap();
}

/// 控制入口（ControlService）同规则复核：GUI 会话在场时 Agent 控制动作被拒，
/// GUI 路径放行；admit 失败不消费审批令牌（M10）。
#[test]
fn control_entry_lease_admission_gates_agent_actions() {
    // 记录型假后端：验证驱动只在 admit 放行后被调用
    #[derive(Default)]
    struct MockBackend {
        power_calls: std::sync::Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl duthub_core::ControlBackend for MockBackend {
        async fn power(
            &self,
            _target: &duthub_core::ControlTarget,
            _on: bool,
        ) -> duthub_core::DeviceResult<()> {
            *self.power_calls.lock().unwrap() += 1;
            Ok(())
        }
        async fn reset(
            &self,
            _target: &duthub_core::ControlTarget,
        ) -> duthub_core::DeviceResult<()> {
            Err(duthub_core::DeviceError::Unsupported)
        }
        async fn power_key(
            &self,
            _target: &duthub_core::ControlTarget,
            _long_press: bool,
        ) -> duthub_core::DeviceResult<()> {
            Err(duthub_core::DeviceError::Unsupported)
        }
    }
    let (service, mgr, _sink) = service_with_lease("control-entry");
    service
        .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    let backend = Arc::new(MockBackend::default());
    let approval = Arc::new(ApprovalManager::new());
    let control = ControlService::new(Some(backend.clone()), approval.clone())
        .with_lease_authority(mgr.clone());
    control.register_dut(DutConfig {
        id: "dut-a".into(),
        name: "样机".into(),
        serial: SerialConfig {
            host: "local".into(),
            port: "COM3".into(),
            baud: 115_200,
            format: "8N1".into(),
            reference_log: None,
            reference_similarity: None,
            split_logs: false,
        },
        adb: duthub_core::model::AdbConfig::default(),
        relay: RelayConfig::UsbSerial {
            host: String::new(),
            node: "COM25".into(),
            driver: String::new(),
            channels: RelayChannels {
                power: Some(1),
                reset: None,
                recovery: None,
                maskrom: None,
                power_key: None,
            },
        },
        mcp: McpConfig::StreamableHttp {
            url: "http://127.0.0.1:8797/mcp".into(),
        },
        a2a: duthub_core::model::DutA2aConfig::default(),
        flash: duthub_core::model::FlashConfig::default(),
        media: duthub_core::model::DutMediaConfig::default(),
        heartbeat: duthub_core::model::HeartbeatConfig::default(),
    });
    // GUI 会话在场 → Agent 控制动作被租约拒（INV-01 扩展到控制通道）
    let token = approval.issue("dut-a", "control.power", r#"{"on":true}"#);
    let err = control
        .execute_by_id_as(
            "dut-a",
            ControlAction::Power { on: true },
            &token.token,
            &WriteIdentity::agent(None, None),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("GUI"),
        "控制拒绝必须指明 GUI 在场：{err}"
    );
    assert_eq!(*backend.power_calls.lock().unwrap(), 0, "驱动不得被调用");
    // 同一枚令牌仍可用（admit 失败不消费，M10）
    // GUI 路径（人类在场，按钮即授权）→ 放行并驱动被调
    control
        .execute_by_id("dut-a", ControlAction::Power { on: true }, &token.token)
        .unwrap();
    assert_eq!(*backend.power_calls.lock().unwrap(), 1);
    service.close_session("dut-a").unwrap();
}

/// 未接线租约的 DeviceService（既有路径）：写行为零改动（回归护栏）。
#[test]
fn legacy_service_without_lease_behaves_unchanged() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target/test-tmp/lease-admission")
        .join(format!("legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let service = DeviceService::custom(64, dir, 0);
    service
        .open_session("dut-a", "COM3", 115_200, SessionKind::Demo)
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    // 无租约复核：Agent 无租约形态也直接放行（既有测试等价路径）
    service
        .write_line("dut-a", WriteIdentity::agent(None, None), "uname -a")
        .unwrap();
    service
        .write_bytes("dut-a", WriteIdentity::gui(), b"key")
        .unwrap();
    service.close_session("dut-a").unwrap();
}
