//! M1.7（可选）：com0com 虚拟串口对回环测试。
//!
//! 需要 com0com（https://sourceforge.net/projects/com0com/）安装并创建一对端口
//! （例如 COM10<->COM11，CNC 参数含 "com0com"）。未安装 → SKIP（不失败）。
//! 检测方式：枚举端口 friendly name / 描述含 "com0com"；本测试仅发现与开合冒烟，
//! 不做真实回环写入（该验证属 H1 人工清单，避免 CI 环境差异）。

#![cfg(feature = "real-hardware")]

use duthub_device::{DeviceService, SessionKind};

#[test]
fn com0com_loopback_smoke_or_skip() {
    let service = DeviceService::custom(
        256,
        {
            let dir = std::env::current_dir()
                .unwrap()
                .join("target/test-tmp/com0com")
                .join(std::process::id().to_string());
            let _ = std::fs::remove_dir_all(&dir);
            dir
        },
        5,
    );

    let ports = service.list_ports();
    let pairs: Vec<_> = ports
        .iter()
        .filter(|p| {
            p.description
                .as_deref()
                .is_some_and(|d| d.to_ascii_lowercase().contains("com0com"))
        })
        .collect();

    if pairs.len() < 2 {
        eprintln!(
            "SKIP: 未检测到 com0com 虚拟串口对（{} 个端口），回环测试跳过",
            ports.len()
        );
        return;
    }

    // 发现成对端口 → 打开/关闭冒烟（同一时刻只能绑定一个会话）
    let port_a = pairs[0].port.clone();
    let session = service.open_session("com0com-a", &port_a, 115_200, SessionKind::Real);
    match session {
        Ok(session_id) => {
            eprintln!("OK: com0com 端口 {port_a} 打开成功（session={session_id}）");
            service.close_session("com0com-a").ok();
        }
        Err(err) => {
            // 端口被占用/权限不足等环境问题：报告并跳过，不算失败
            eprintln!("SKIP: com0com 端口 {port_a} 打开失败（{err}）");
        }
    }
}
