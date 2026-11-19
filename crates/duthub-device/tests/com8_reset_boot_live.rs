//! COM8 + COM25 真机端到端（串口性能任务验收，2026-11-21 用户授权实动）：
//! 继电器通道 1（DUT 复位）脉冲 → RK3576 启动日志洪流 → 实时快照发布证据 →
//! 会话关闭排空 → 日志零丢失核对。
//!
//! 前置：
//! - COM8 = RK3576 控制台 @ 115200 8N1（已保存绑定，见 Desktop/test/.duthub/duts.json）；
//! - COM25 = CH340 继电器板，通道 1 接 DUT 复位（AGENTS.md 并行协作节已授权，绝不同时动两通道）；
//! - 两个口运行时不能被其他进程占用（GUI / 旧测试）。
//!
//! 运行：
//! `cargo test -p duthub-device --features real-hardware --test com8_reset_boot_live -- --ignored --nocapture`
//!
//! 锁住的不变量（见 AGENTS.md 坑 #46–#51 与回归测试加固报告）：
//! - real.rs 新事件循环（pending 队列 + TX 合并 + 快照节流）在真实串口上工作；
//! - 空闲首回显直接发布、洪流按 16ms tick 合并 → SerialScreen 事件成串到达（PERF-3 的实机面）；
//! - 日志写线程逐条 flush、会话关闭排空 → .bin 原始字节 ≥ 收到的文本量、.log 含内核启动行（LOG-1/LOG-6）；
//! - TX 写路径真机成功且无「写入失败」注记（IO-3 的实机面）。
#![cfg(feature = "real-hardware")]

use std::time::{Duration, Instant};

use duthub_control::Ch340RelayDriver;
use duthub_core::ipc::UiEvent;
use duthub_core::{ApprovalManager, ControlBackend};
use duthub_device::control::ControlService;
use duthub_device::{DeviceService, SessionKind};

const PORT: &str = "COM8";
const RELAY_PORT: &str = "COM25";
const BAUD: u32 = 115_200;
const DUT: &str = "rk3576";
/// 复位后等待内核启动行的窗口（RK3576 上电到 kernel 打印实测数秒，留足余量）。
const BOOT_WINDOW: Duration = Duration::from_secs(90);

/// 轮询等待会话日志（总线缓冲）出现 needle。
fn wait_for(service: &DeviceService, needle: &str, rounds: u32) -> bool {
    for _ in 0..rounds {
        if service
            .read_log(&format!("sess-{DUT}"), 0, usize::MAX)
            .iter()
            .any(|e| matches!(&e.event, UiEvent::SerialChunk { text, .. } if text.contains(needle)))
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
#[ignore = "真机端到端：显式运行（COM8 控制台 + COM25 通道 1 复位脉冲，已获用户授权）"]
fn com8_reset_boot_live() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target/test-tmp/device/com8-reset-live")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    let service = DeviceService::custom(1024, dir.clone(), 0);
    // 订阅必须先于开会话：打开注记与洪流都要经总线收集
    let mut rx = service.subscribe();

    // 1) 打开 COM8 真实会话（新事件循环：pending 队列 + 写线程 + 快照节流）
    service
        .open_session(DUT, PORT, BAUD, SessionKind::Real)
        .expect("COM8 打开失败（口被占用或不存在）");
    assert!(
        wait_for(&service, "@ 115200 已打开", 50),
        "5s 内未见到 COM8 打开注记"
    );
    println!("COM8 会话已打开 @ {BAUD}");

    // 2) TX 探针：写入成功 + 无「写入失败」注记（真机 write_all 成功的直接证据）。
    //    控制台是否回显取决于设备当前状态（可能在 shell / login / 静默），不做回显硬断言。
    service
        .write_line(
            DUT,
            duthub_core::lease::WriteIdentity::gui(),
            "echo duthub-com8-live",
        )
        .expect("TX 入队失败");
    std::thread::sleep(Duration::from_millis(1500));
    let write_failed = service
        .read_log(&format!("sess-{DUT}"), 0, usize::MAX)
        .iter()
        .any(
            |e| matches!(&e.event, UiEvent::SerialChunk { text, .. } if text.contains("写入失败")),
        );
    assert!(!write_failed, "TX 写入出现失败注记（真机 write_all 失败）");
    println!("TX 探针已下发且无失败注记");

    // 3) COM25 通道 1 复位脉冲（已获授权）：STATUS 枚举 → approval 签发 →
    //    channel_test(1) = ON→STATUS 吸合→OFF→STATUS 释放，报告逐项断言。
    let approval = std::sync::Arc::new(ApprovalManager::new());
    let backend: std::sync::Arc<dyn ControlBackend> = std::sync::Arc::new(Ch340RelayDriver::new());
    let control = ControlService::new(Some(backend), approval.clone());
    control.register_dut(duthub_core::model::DutConfig {
        id: "dut-live-com25".into(),
        name: "COM25 继电器板".into(),
        serial: duthub_core::model::SerialConfig {
            host: "local".into(),
            port: RELAY_PORT.into(),
            baud: 115_200,
            format: "8N1".into(),
            reference_log: None,
            reference_similarity: None,
            split_logs: false,
        },
        adb: Default::default(),
        relay: duthub_core::model::RelayConfig::UsbSerial {
            host: "local".into(),
            node: RELAY_PORT.into(),
            driver: "CH340".into(),
            channels: duthub_core::model::RelayChannels {
                power: None,
                reset: Some(1),
                recovery: None,
                maskrom: None,
                power_key: None,
            },
        },
        mcp: duthub_core::model::McpConfig::StreamableHttp {
            url: "http://127.0.0.1:8797/mcp".into(),
        },
        a2a: Default::default(),
        flash: Default::default(),
        media: Default::default(),
        heartbeat: Default::default(),
    });
    let channels = control
        .channel_status("dut-live-com25")
        .expect("COM25 STATUS 枚举失败");
    assert!(
        channels.iter().any(|&(c, _)| c == 1),
        "通道 1 未应答 STATUS（接线/驱动不符）：{channels:?}"
    );
    let token = approval.issue("dut-live-com25", "control.test", r#"{"channel":1}"#);
    let report = control
        .channel_test("dut-live-com25", 1, &token.token)
        .expect("通道 1 复位脉冲执行失败");
    println!("通道 1 往返报告: {report:?}");
    assert!(report.on_ok, "ON 后 STATUS 未确认吸合");
    assert!(report.off_ok, "OFF 后 STATUS 未确认释放");
    assert_eq!(report.after, Some(false), "脉冲后通道 1 必须恢复 OFF");
    println!("复位脉冲完成，等待 RK3576 启动日志洪流……");

    // 4) 启动洪流：复位后控制台重新打印 DDR/U-Boot/内核日志。收集总线文本与
    //    SerialScreen 计数——快照按 16ms tick 合并发布在洪流下必然成串到达。
    let deadline = Instant::now() + BOOT_WINDOW;
    let mut boot_text = String::new();
    let mut screen_events = 0usize;
    while Instant::now() < deadline {
        while let Ok(env) = rx.try_recv() {
            match &env.event {
                UiEvent::SerialChunk { text, .. } => boot_text.push_str(text),
                UiEvent::SerialScreen { .. } => screen_events += 1,
                _ => {}
            }
        }
        if boot_text.contains("Linux version") && screen_events >= 5 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        boot_text.contains("Linux version"),
        "{BOOT_WINDOW:?} 内未在 COM8 见到内核启动行（复位后控制台无输出？）：{} 字符",
        boot_text.chars().count()
    );
    assert!(
        screen_events >= 5,
        "洪流下 SerialScreen 快照仅 {screen_events} 帧——实时发布机制未按帧工作"
    );
    println!(
        "洪流到达：文本 {} 字符，快照 {screen_events} 帧（实时发布实机证据）",
        boot_text.chars().count()
    );

    // 5) 零丢失收尾：关闭会话 → SessionCtx Drop 排空写线程（condvar 3s 兜底）→
    //    等文件尺寸稳定后核对：.bin 原始字节 ≥ 收到的文本字符数（UTF-8 每字符
    //    ≥1 字节，多字节只会更大），.log 文本流含内核启动行。任何丢失都会让
    //    文件侧小于总线侧累计。
    service.close_session(DUT).expect("关闭会话失败");
    let bin_path = dir.join(format!("sess-{DUT}.bin"));
    let log_path = dir.join(format!("sess-{DUT}.log"));
    let mut last_size = 0u64;
    let stable_since = Instant::now();
    loop {
        let size = std::fs::metadata(&bin_path).map(|m| m.len()).unwrap_or(0);
        if size == last_size && stable_since.elapsed() > Duration::from_millis(800) {
            break;
        }
        if size != last_size {
            last_size = size;
        }
        assert!(
            stable_since.elapsed() < Duration::from_secs(8),
            "关闭 8s 后 .bin 仍在增长或缺失（写线程未排空？）：{size} 字节"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let bin = std::fs::read(&bin_path).expect(".bin 不存在");
    let log = std::fs::read_to_string(&log_path).expect(".log 不存在");
    println!(
        "落盘核对：.bin {} 字节 / 收到文本 {} 字符",
        bin.len(),
        boot_text.chars().count()
    );
    assert!(
        bin.len() >= boot_text.chars().count(),
        ".bin（{} 字节）小于收到的文本量（{} 字符）——日志丢失",
        bin.len(),
        boot_text.chars().count()
    );
    assert!(
        log.contains("Linux version"),
        ".log 文本流缺少内核启动行（utf8-lossy 文本丢失）"
    );
    println!("零丢失核对通过：复位 → 洪流 → 快照 → 排空 → 落盘全链路完整");
}
