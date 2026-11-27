//! 控制租约不变量测试（docs/SECURITY.md；SPEC §6.1）：
//! INV-01 单租约 / INV-04 generation fencing / TTL 过期 / GUI 优先（Agent 排队被拒）
//! / INV-05 Protected / INV-06 跨 DUT 并行 / INV-07 重启作废。

use std::sync::Arc;
use std::time::Duration;

use duthub_core::audit::AuditSink;
use duthub_core::lease::{
    ControlLeaseManager, LeaseAuthority, LeaseError, LeaseHolder, LeasePhase, WriteIdentity,
};

fn manager() -> Arc<ControlLeaseManager> {
    // 审计内存环：测试后可检查事件流（不落盘）
    Arc::new(
        ControlLeaseManager::new(Some(AuditSink::memory()))
            .with_wait_poll(Duration::from_millis(10)),
    )
}

fn agent(name: &str) -> LeaseHolder {
    LeaseHolder::Agent {
        agent_name: name.into(),
        credential_id: format!("cred-{name}"),
    }
}

/// INV-01：并发 acquire 同一 DUT 只有 1 个成功；其余 Busy 且消息携带持有者。
#[test]
fn inv01_concurrent_acquire_single_winner() {
    let m = manager();
    let barrier = Arc::new(std::sync::Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|i| {
            let m = m.clone();
            let barrier = barrier.clone();
            let holder = if i == 0 {
                LeaseHolder::Gui
            } else {
                agent(&format!("bot-{i}"))
            };
            std::thread::spawn(move || {
                barrier.wait();
                m.acquire("dut-a", &holder, None)
            })
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let winners: Vec<_> = outcomes.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "INV-01：并发 acquire 必须恰好 1 个成功");
    // 5 个失败者全部 Busy，消息指明持有者
    for err in outcomes.into_iter().filter_map(std::result::Result::err) {
        match err {
            LeaseError::Busy { holder, .. } => assert_ne!(holder, ""),
            other => panic!("并发失败必须是 Busy，得到 {other:?}"),
        }
    }
    assert_eq!(m.status("dut-a").phase, LeasePhase::Held);
}

/// INV-01 另一侧：GUI 持有期间 Agent acquire 被拒（Busy + 持有者标签）。
#[test]
fn inv01_agent_acquire_rejected_while_gui_holds() {
    let m = manager();
    let g = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    assert_eq!(m.status("dut-a").holder.as_deref(), Some("GUI（人类在场）"));
    let err = m.acquire("dut-a", &agent("claude"), None).unwrap_err();
    match err {
        LeaseError::Busy { holder, .. } => {
            assert!(
                holder.contains("GUI"),
                "Busy 消息必须指明 GUI 持有：{holder}"
            );
        }
        other => panic!("期望 Busy，得到 {other:?}"),
    }
    // 释放后 Agent 才能取到
    m.release("dut-a", &g.lease_id).unwrap();
    m.acquire("dut-a", &agent("claude"), None).unwrap();
}

/// INV-04：release/re-acquire 后 generation 单调 +1，旧代数 admit 全拒。
#[test]
fn inv04_stale_generation_rejected() {
    let m = manager();
    let g1 = m.acquire("dut-a", &agent("claude"), None).unwrap();
    assert_eq!(g1.generation, 1);
    m.release("dut-a", &g1.lease_id).unwrap();
    let g2 = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    assert_eq!(g2.generation, 2, "每次 acquire generation+1");
    // 旧代数句柄写入 admit：租约已换人（GUI 在场）→ Busy 指向 GUI
    let stale_write = WriteIdentity::agent(Some(g1.lease_id.clone()), Some(g1.generation));
    assert!(matches!(
        m.admit_write("dut-a", &stale_write),
        Err(LeaseError::Busy { .. })
    ));
    // 当前句柄 + 旧代数 → Generation fencing
    let wrong_gen = WriteIdentity::agent(Some(g2.lease_id.clone()), Some(1));
    assert!(matches!(
        m.admit_write("dut-a", &wrong_gen),
        Err(LeaseError::Generation { current: 2 })
    ));
    // 正确 (lease_id, generation) 放行；renew 不变代数
    let ok = WriteIdentity::agent(Some(g2.lease_id.clone()), Some(2));
    assert!(m.admit_write("dut-a", &ok).is_ok());
    let renewed = m.renew("dut-a", &g2.lease_id).unwrap();
    assert_eq!(renewed.generation, 2, "renew 不得推进代数（SPEC §6.5）");
    // 强制接管：代数再 +1，旧持有者下一次写入被拒
    let g3 = m.force_takeover("dut-a").unwrap();
    assert_eq!(g3.generation, 3);
    assert!(matches!(
        m.admit_write("dut-a", &ok),
        Err(LeaseError::Busy { .. })
    ));
}

/// TTL 过期：到期后惰性回收 → Free；renew 报 Expired；Agent 重新可取。
#[test]
fn ttl_expiry_invalidates_and_frees() {
    let m = Arc::new(
        ControlLeaseManager::new(Some(AuditSink::memory()))
            .with_ttl(Duration::from_millis(80))
            .with_wait_poll(Duration::from_millis(10)),
    );
    let g = m.acquire("dut-a", &agent("claude"), None).unwrap();
    std::thread::sleep(Duration::from_millis(110));
    // renew 先于惰性回收触发 → 明确 Expired（区别 UnknownLease）
    assert_eq!(m.renew("dut-a", &g.lease_id), Err(LeaseError::Expired));
    assert_eq!(m.status("dut-a").phase, LeasePhase::Free);
    // GUI 持有期间 TTL 过期 → GUI 写入 admit 现场重新获取（新代数）
    let g2 = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    assert!(g2.generation > g.generation);
    std::thread::sleep(Duration::from_millis(110));
    // GUI 身份写入：admit 自动现场 acquire（人类在场）
    let ctx = m.admit_write("dut-a", &WriteIdentity::gui()).unwrap();
    let ctx = ctx.expect("GUI 写入必须携带新租约上下文");
    assert!(ctx.generation > g2.generation);
}

/// INV-05：Protected 期间 acquire/renew/release/写入/接管全拒；exit 走状态机。
#[test]
fn inv05_protected_blocks_everything_until_exit() {
    let m = manager();
    let g = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    m.enter_protected("dut-a", "烧录中").unwrap();
    assert!(matches!(
        m.acquire("dut-a", &agent("bot"), None),
        Err(LeaseError::Protected { .. })
    ));
    assert!(matches!(
        m.renew("dut-a", &g.lease_id),
        Err(LeaseError::Protected { .. })
    ));
    assert!(matches!(
        m.release("dut-a", &g.lease_id),
        Err(LeaseError::Protected { .. })
    ));
    assert!(matches!(
        m.force_takeover("dut-a"),
        Err(LeaseError::Protected { .. })
    ));
    for w in [
        WriteIdentity::gui(),
        WriteIdentity::system(),
        WriteIdentity::agent(None, None),
    ] {
        assert!(matches!(
            m.admit_write("dut-a", &w),
            Err(LeaseError::Protected { .. })
        ));
    }
    assert_eq!(
        m.status("dut-a").phase,
        LeasePhase::Protected,
        "status 必须显示保护态"
    );
    // exit(ok) → Free；随后 Agent 可取（代数 +1）
    m.exit_protected("dut-a", true).unwrap();
    assert_eq!(m.status("dut-a").phase, LeasePhase::Free);
    let g2 = m.acquire("dut-a", &agent("bot"), None).unwrap();
    assert!(g2.generation > g.generation);
    // exit(unknown) → NeedsInspection → 人工确认 → Free
    m.enter_protected("dut-a", "烧录中").unwrap();
    m.exit_protected("dut-a", false).unwrap();
    assert_eq!(m.status("dut-a").phase, LeasePhase::NeedsInspection);
    assert!(matches!(
        m.acquire("dut-a", &LeaseHolder::Gui, None),
        Err(LeaseError::NeedsInspection)
    ));
    m.resolve_inspection("dut-a", true);
    assert_eq!(m.status("dut-a").phase, LeasePhase::Free);
}

/// INV-06：跨 DUT 并行互不阻塞（互斥只作用于同 DUT）。
#[test]
fn inv06_parallel_duts_independent() {
    let m = manager();
    let ga = m.acquire("dut-a", &agent("bot-a"), None).unwrap();
    let gb = m.acquire("dut-b", &agent("bot-b"), None).unwrap();
    let gc = m.acquire("dut-c", &LeaseHolder::Gui, None).unwrap();
    assert_ne!(ga.lease_id, gb.lease_id);
    assert_ne!(gb.lease_id, gc.lease_id);
    // 三个 DUT 同时 Held
    for dut in ["dut-a", "dut-b", "dut-c"] {
        assert_eq!(m.status(dut).phase, LeasePhase::Held, "{dut}");
    }
    // 一个 DUT 的释放不影响其他
    m.release("dut-a", &ga.lease_id).unwrap();
    assert_eq!(m.status("dut-a").phase, LeasePhase::Free);
    assert_eq!(m.status("dut-b").phase, LeasePhase::Held);
}

/// INV-07：进程重启（新管理器）后旧租约全部作废。
#[test]
fn inv07_restart_invalidates_all_leases() {
    let m1 = manager();
    let g = m1.acquire("dut-a", &agent("claude"), None).unwrap();
    let m2 = manager();
    assert_eq!(
        m2.renew("dut-a", &g.lease_id),
        Err(LeaseError::UnknownLease),
        "重启后旧句柄必须 UnknownLease"
    );
    assert_eq!(
        m2.validate("dut-a", &g.lease_id, g.generation),
        Err(LeaseError::UnknownLease)
    );
    assert_eq!(m2.status("dut-a").phase, LeasePhase::Free);
    // 新管理器可立即授予
    m2.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
}

/// FIFO 等待队列：取消队首请求后，下一位获得租约。
#[test]
fn fifo_queue_orders_cancels_and_times_out() {
    let m = manager();
    let g1 = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    // 排队两个 Agent（a 先 b 后）
    let ha = {
        let m = m.clone();
        let holder = agent("bot-a");
        std::thread::spawn(move || m.acquire("dut-a", &holder, Some(Duration::from_millis(2000))))
    };
    std::thread::sleep(Duration::from_millis(50));
    let hb = {
        let m = m.clone();
        let holder = agent("bot-b");
        std::thread::spawn(move || m.acquire("dut-a", &holder, Some(Duration::from_millis(2000))))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(m.status("dut-a").queue_len, 2, "两个等待者已入队");
    // 取消 bot-a 的排队
    let req_a = m.pending_wait_requests("dut-a")[0];
    assert!(m.cancel_wait("dut-a", req_a));
    assert_eq!(m.status("dut-a").queue_len, 1);
    m.release("dut-a", &g1.lease_id).unwrap();
    // bot-a 已取消 → 队首是 bot-b
    let gb = hb.join().unwrap().unwrap();
    assert_eq!(gb.holder, "Agent bot-b");
    // bot-a 主动取消后应以 Busy 退出；等待超时由独立单测覆盖。
    assert!(matches!(ha.join().unwrap(), Err(LeaseError::Busy { .. })));
}

/// GUI 优先：GUI 身份写入在 Free 时自动获取租约（admit 现场 acquire）；
/// Agent 持有时 GUI 写入被拒（Busy 指向 Agent）。
#[test]
fn gui_priority_write_admit_both_directions() {
    let m = manager();
    // Free：GUI 写入自动获取
    let ctx = m
        .admit_write("dut-a", &WriteIdentity::gui())
        .unwrap()
        .expect("GUI admit 必须返回租约上下文");
    assert_eq!(m.status("dut-a").phase, LeasePhase::Held);
    // GUI 再次写入：同租约上下文（幂等）
    let ctx2 = m
        .admit_write("dut-a", &WriteIdentity::gui())
        .unwrap()
        .expect("GUI admit 幂等");
    assert_eq!(ctx, ctx2);
    // Agent 在控：GUI 写入被拒
    m.release("dut-a", &ctx.lease_id).unwrap();
    let ga = m.acquire("dut-a", &agent("bot"), None).unwrap();
    let err = m.admit_write("dut-a", &WriteIdentity::gui()).unwrap_err();
    match err {
        LeaseError::Busy { holder, .. } => assert!(holder.contains("bot"), "{holder}"),
        other => panic!("期望 Busy，得到 {other:?}"),
    }
    // System（心跳）：GUI 持有期放行；Agent 控制期静默被拒（方案 §5.3 admit 表）
    m.release("dut-a", &ga.lease_id).unwrap();
    let gg = m.acquire("dut-a", &LeaseHolder::Gui, None).unwrap();
    assert_eq!(
        m.admit_write("dut-a", &WriteIdentity::system()),
        Ok(None),
        "GUI 持有期 System 放行"
    );
    m.release("dut-a", &gg.lease_id).unwrap();
    let _ = m.acquire("dut-a", &agent("bot-2"), None).unwrap();
    assert!(
        matches!(
            m.admit_write("dut-a", &WriteIdentity::system()),
            Err(LeaseError::Busy { .. })
        ),
        "Agent 控制期心跳静默被拒"
    );
}

/// auth 开关联动：auth ON 时无租约 Agent 写拒（LeaseRequired）；GUI 不受影响。
#[test]
fn auth_switch_gates_leaseless_agent_writes() {
    let m = manager();
    // 默认 auth 关：无租约 Agent 写放行（偏差 7 过渡）
    assert_eq!(
        m.admit_write("dut-a", &WriteIdentity::agent(None, None)),
        Ok(None)
    );
    m.set_auth_enabled(true);
    assert!(matches!(
        m.admit_write("dut-a", &WriteIdentity::agent(None, None)),
        Err(LeaseError::LeaseRequired)
    ));
    // GUI 不受 auth 开关影响（本地人类在场）
    assert!(m.admit_write("dut-a", &WriteIdentity::gui()).is_ok());
    // System 心跳也不受影响（但受 Protected 影响，见 INV-05 用例）
    assert_eq!(m.admit_write("dut-a", &WriteIdentity::system()), Ok(None));
}
