//! COM8 键入路径实机延迟与正确性（串口性能任务验收，用户授权实动）：
//! 逐键走 WriteBytes（键盘路径，无行尾追加、无本地回显）→ 测 TX→回显 与
//! TX→快照两段延迟；空格/退格正确性从回显流与快照网格双向核对；末尾连发
//! 8 个退格锁 TX 合并的排空表现。
//!
//! 前置：COM8 = RK3576 控制台 @ 115200 8N1，对端 tty 处于会回显的状态
//!（shell 提示符 / login 提示符均可）；GUI 实例必须先释放 COM8。
//! 若对端正在刷启动日志，先等静默 + 提示符再测（启动期 tty 不回显键入）。
//!
//! 运行：
//! `cargo test -p duthub-device --features real-hardware --test com8_io_latency_live -- --ignored --nocapture`
//! Set `DUTHUB_SERIAL_TEST_PORT` to exercise a different shell console at 115200 8N1.
#![cfg(feature = "real-hardware")]

use std::time::{Duration, Instant};

use duthub_core::ipc::UiEvent;
use duthub_device::{DeviceService, SessionKind};

const PORT: &str = "COM8";
const BAUD: u32 = 115_200;
const DUT: &str = "rk3576";

/// 一次延迟测量的结果（微秒）。
struct Latency {
    echo_us: u128,
    snapshot_us: Option<u128>,
}

/// 键入事件收集器：回显文本 + 快照光标行。
#[derive(Default)]
struct Collector {
    echoed: String,
    /// 每个快照事件的光标行文本（None = 该事件无光标行）。
    cursor_rows: Vec<Option<String>>,
}

impl Collector {
    fn drain(
        &mut self,
        rx: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
    ) {
        while let Ok(env) = rx.try_recv() {
            match &env.event {
                UiEvent::SerialChunk { text, .. } => self.echoed.push_str(text),
                UiEvent::SerialScreen { screen } => {
                    let row = screen
                        .cursor_row
                        .and_then(|idx| screen.rows.get(idx as usize).map(|r| r.text.clone()));
                    self.cursor_rows.push(row);
                }
                _ => {}
            }
        }
    }

    /// 最近 2s 无新文本 = 对端静默（启动洪流结束）。
    fn wait_quiet(
        &mut self,
        rx: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
    ) {
        let mut last_growth = Instant::now();
        let mut last_len = self.echoed.len();
        while last_growth.elapsed() < Duration::from_secs(2) {
            self.drain(rx);
            if self.echoed.len() != last_len {
                last_len = self.echoed.len();
                last_growth = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 等待 shell/login 提示符出现（Android init 到控制台 shell 可能需要数分钟；
    /// 期间周期性发 CR 唤醒——shell 到位后提示符会随 CR 重印）。
    /// `wake` 为 None 时不发唤醒（由调用方自行负责）。
    fn wait_prompt(
        &mut self,
        rx: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
        wake: Option<&DeviceService>,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut last_wake = Instant::now();
        while Instant::now() < deadline {
            self.drain(rx);
            let tail: String = {
                let chars: Vec<char> = self.echoed.chars().collect();
                let start = chars.len().saturating_sub(80);
                chars[start..].iter().collect()
            };
            if tail.contains("# ")
                || tail.contains("$ ")
                || tail.contains("login:")
                || tail.contains("~#")
                || tail.contains("/ #")
            {
                return true;
            }
            if let Some(svc) = wake
                && last_wake.elapsed() >= Duration::from_secs(15)
            {
                let _ = svc.write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), b"\r");
                last_wake = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }
}

#[test]
#[ignore = "真机键入路径延迟与正确性：显式运行（COM8 独占，GUI 需先释放；对端需在回显状态）"]
fn com8_io_latency_live() {
    let port = std::env::var("DUTHUB_SERIAL_TEST_PORT").unwrap_or_else(|_| PORT.to_owned());
    let dir = std::env::current_dir()
        .unwrap()
        .join("target/test-tmp/device/com8-io-latency")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    let service = DeviceService::custom(1024, dir, 0).with_scrollback(1_000_000);
    let mut rx = service.subscribe();

    service
        .open_session(DUT, &port, BAUD, SessionKind::Real)
        .expect("串口会话启动失败");
    assert!(
        (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            service
                .read_log(&format!("sess-{DUT}"), 0, usize::MAX)
                .iter()
                .any(|e| {
                    matches!(&e.event,
                    UiEvent::SerialChunk { text, .. } if text.contains("已打开"))
                })
        }),
        "5s 内未见 {port} 打开注记（端口可能被占用）"
    );
    println!("{port} 已打开 @ {BAUD}，等待对端就绪……");

    let mut col = Collector::default();

    // 0a) 等启动洪流结束（静默 2s）；期间若在刷内核日志，键入不会被回显
    col.wait_quiet(&mut rx);
    service
        .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), b"\r")
        .expect("唤醒提示符失败");
    // 0b) 控制台连接时不会重印提示符（回显流为空）：等 shell 到位（Android
    //     init 需要数分钟），期间周期性 CR 唤醒
    assert!(
        col.wait_prompt(&mut rx, Some(&service)),
        "300s 内未等到 shell/login 提示符——对端不在可交互状态，无法测键入路径"
    );
    // 0c) Ctrl-C 清掉可能残留的半行输入，再排空一次
    service
        .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[0x03])
        .expect("TX 入队失败");
    std::thread::sleep(Duration::from_millis(400));
    col.drain(&mut rx);
    println!("对端就绪（提示符已见），开始键入路径测量");

    // 1) 逐键延迟："ab cd"（含空格）+ "123"——空格与普通字符同路径。
    //    基线先排空，再发送，按字符标记匹配（不把洪流缓冲误当回显）。
    let keys: Vec<&[u8]> = vec![b"a", b"b", b" ", b"c", b"d", b"1", b"2", b"3"];
    let mut latencies: Vec<(String, Latency)> = Vec::new();
    for k in &keys {
        col.drain(&mut rx); // 排空基线
        let ch = String::from_utf8_lossy(k).to_string();
        let t0 = Instant::now();
        service
            .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), k)
            .expect("TX 入队失败");
        let base_echo = col.echoed.len();
        let base_snaps = col.cursor_rows.len();
        let mut got = Latency {
            echo_us: 0,
            snapshot_us: None,
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            col.drain(&mut rx);
            if got.echo_us == 0
                && col.echoed[base_echo.min(col.echoed.len())..].contains(ch.as_str())
            {
                got.echo_us = t0.elapsed().as_micros();
            }
            if got.snapshot_us.is_none() && col.cursor_rows.len() > base_snaps {
                got.snapshot_us = Some(t0.elapsed().as_micros());
            }
            if got.echo_us > 0 && got.snapshot_us.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            got.echo_us > 0,
            "键入 {ch:?} 后 3s 未见该字符的回显——对端回显状态变化？回显尾: {:?}",
            {
                let chars: Vec<char> = col.echoed.chars().collect();
                let start = chars.len().saturating_sub(60);
                chars[start..].iter().collect::<String>()
            }
        );
        latencies.push((ch, got));
        std::thread::sleep(Duration::from_millis(120)); // 键间隔，模拟手打
    }
    println!("\n=== 逐键延迟（TX → 回显 | TX → 快照）===");
    for (ch, l) in &latencies {
        println!(
            "  {ch:?}: {:>6.1} ms | {}",
            l.echo_us as f64 / 1000.0,
            match l.snapshot_us {
                Some(us) => format!("{:>6.1} ms", us as f64 / 1000.0),
                None => "（无快照事件）".to_string(),
            }
        );
    }
    let mut echo_vals: Vec<u128> = latencies.iter().map(|(_, l)| l.echo_us).collect();
    echo_vals.sort();
    let avg = echo_vals.iter().sum::<u128>() / echo_vals.len().max(1) as u128;
    let p95 = echo_vals[(echo_vals.len() * 95).div_ceil(100) - 1];
    println!(
        "回显延迟: min {:.1} / avg {:.1} / p95 {:.1} / max {:.1} ms",
        echo_vals[0] as f64 / 1000.0,
        avg as f64 / 1000.0,
        p95 as f64 / 1000.0,
        echo_vals[echo_vals.len() - 1] as f64 / 1000.0
    );

    // 2) 空格正确性（双向）：
    //    a) 回显流里 "ab" 之后必须紧跟空格字符；
    //    b) 快照光标行必须出现尾随空格保留（A1 修复的实机面）：空格刚敲下、
    //       光标落在尾随空白上时，行尾的空格不得被裁掉（行形如 "# ab "）。
    // 用 rfind 取最后一次键入的 "ab"（此前的唤醒 CR 可能误执行过残留命令，
    // 报错文本里也含 "ab"，不能按首次出现匹配）
    let ab_pos = col.echoed.rfind("ab").expect("回显流中找不到 ab");
    let after_ab = &col.echoed[ab_pos + 2..];
    assert!(
        after_ab.starts_with(' '),
        "空格回显缺失：ab 后的回显是 {after_ab:?}"
    );
    let space_snap = col
        .cursor_rows
        .iter()
        .flatten()
        .any(|row| row.trim_end().ends_with("ab") && row.ends_with(' '));
    assert!(
        space_snap,
        "快照未出现含尾随空格的 \"ab \" 行（A1 修复未在真机生效？）：{:?}",
        col.cursor_rows.last()
    );
    println!("空格正确性 ✓（回显含空格 + 快照保留 \"ab \" 尾随空白）");

    // 3) 退格正确性：删掉行尾最后一个字符（'3'），快照行恰好少一个字符、
    //    其余内容原样保留（删除是单字符精确擦除，不多删不少删）。
    let row_before_bs = col
        .cursor_rows
        .iter()
        .flatten()
        .rev()
        .find(|row| row.contains("ab"))
        .expect("退格前未见光标行")
        .clone();
    service
        .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[0x7f])
        .expect("TX 入队失败");
    let base_snaps = col.cursor_rows.len();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bs_echo = false;
    while Instant::now() < deadline {
        col.drain(&mut rx);
        if col.echoed[ab_pos..].contains('\u{8}') {
            bs_echo = true;
        }
        if bs_echo && col.cursor_rows.len() > base_snaps {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(bs_echo, "退格后未见 \\x08 回显（对端未按 BS-SP-BS 擦除）");
    let after_bs = col
        .cursor_rows
        .iter()
        .flatten()
        .rev()
        .find(|row| row.contains("ab"))
        .expect("退格后未见光标行");
    assert_eq!(
        after_bs.trim_end(),
        row_before_bs
            .trim_end()
            .strip_suffix('3')
            .unwrap_or("")
            .trim_end(),
        "退格应恰好删掉行尾一个字符：{row_before_bs:?} → {after_bs:?}"
    );
    println!("退格正确性 ✓（\\x08 回显 + {row_before_bs:?} → {after_bs:?} 恰好删一个字符）");

    // 4) 突发排空（TX 合并）：先补齐 8 字符，再连发 8 个退格（无间隔入队，
    //    absorb_keystrokes 应合并为一次写事务），测从首帧退格到回显排空的总时长。
    let fill: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h"];
    for k in &fill {
        service
            .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), k)
            .expect("TX 入队失败");
        std::thread::sleep(Duration::from_millis(120));
    }
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    for _ in 0..8 {
        service
            .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[0x7f])
            .expect("TX 入队失败");
    }
    let enqueued_us = t0.elapsed().as_micros();
    let base = col.echoed.len();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bs_count = 0usize;
    while Instant::now() < deadline {
        col.drain(&mut rx);
        bs_count = col.echoed[base.min(col.echoed.len())..]
            .matches('\u{8}')
            .count();
        if bs_count >= 8
            && col
                .cursor_rows
                .last()
                .and_then(Option::as_deref)
                .is_some_and(|row| row.ends_with("ab cd12"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        bs_count >= 8,
        "突发 8 退格只见到 {bs_count} 次擦除回显（5s）"
    );
    assert!(
        col.cursor_rows
            .last()
            .and_then(Option::as_deref)
            .is_some_and(|row| row.ends_with("ab cd12")),
        "突发退格后未回到预期输入行"
    );
    println!(
        "\n=== 突发排空（TX 合并）===\n  8 个退格入队耗时 {enqueued_us} µs（合并后应为 1 次 USB 写事务）；\n  8 次擦除回显全部到达总耗时 {} ms",
        t0.elapsed().as_micros() as f64 / 1000.0
    );

    measure_repeated_edits(&service, &mut col, &mut rx);

    // 5) 清理：Ctrl-C 复位当前输入行（不残留未完成命令），关闭会话（日志排空）
    service
        .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[0x03])
        .expect("TX 入队失败");
    std::thread::sleep(Duration::from_millis(300));
    service.close_session(DUT).expect("关闭会话失败");
    println!("会话已关闭（写线程排空，日志完整收尾）");
}

fn measure_repeated_edits(
    service: &DeviceService,
    collector: &mut Collector,
    rx: &mut tokio::sync::broadcast::Receiver<duthub_core::ipc::EventEnvelope>,
) {
    const PAIRS: usize = 120;
    collector.drain(rx);
    let mut samples = Vec::with_capacity(2 * PAIRS);
    for _ in 0..PAIRS {
        for (key, echo, expected) in [(b' ', ' ', "ab cd12 "), (0x7f, '\u{8}', "ab cd12")] {
            let echo_start = collector.echoed.len();
            let frame_start = collector.cursor_rows.len();
            let started = Instant::now();
            service
                .write_bytes(DUT, duthub_core::lease::WriteIdentity::gui(), &[key])
                .expect("键入排队失败");
            let mut echo_at = None;
            let mut frame_at = None;
            while started.elapsed() < Duration::from_secs(2) {
                collector.drain(rx);
                if echo_at.is_none() && collector.echoed[echo_start..].contains(echo) {
                    echo_at = Some(started.elapsed());
                }
                if frame_at.is_none()
                    && collector.cursor_rows[frame_start..]
                        .iter()
                        .flatten()
                        .any(|row| row.ends_with(expected))
                {
                    frame_at = Some(started.elapsed());
                }
                if echo_at.is_some() && frame_at.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let (Some(echo_at), Some(frame_at)) = (echo_at, frame_at) else {
                panic!(
                    "按键 {key:#04x} 回显或目标帧缺失，预期行尾 {expected:?}，实际 {:?}",
                    collector.cursor_rows.last()
                );
            };
            samples.push((key, echo_at, frame_at));
            std::thread::sleep(Duration::from_millis(18));
        }
    }
    for (key, label) in [(b' ', "空格"), (0x7f, "退格")] {
        let mut echo = Vec::with_capacity(PAIRS);
        let mut frame = Vec::with_capacity(PAIRS);
        for (_, echo_at, frame_at) in samples.iter().filter(|(k, _, _)| *k == key) {
            echo.push(*echo_at);
            frame.push(*frame_at);
        }
        echo.sort_unstable();
        frame.sort_unstable();
        let p95 = (PAIRS * 95).div_ceil(100) - 1;
        println!(
            "{label} {PAIRS} 次，回显 p95 {:.1}ms，目标帧 p95 {:.1}ms，目标帧最大 {:.1}ms",
            echo[p95].as_secs_f64() * 1000.0,
            frame[p95].as_secs_f64() * 1000.0,
            frame[PAIRS - 1].as_secs_f64() * 1000.0
        );
    }
}
