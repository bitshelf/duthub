//! duthub-desktop：GPUI 桌面端（UI 1:1 复刻原型 duthub.html）。
//!
//! `--self-test`：无交互冒烟——打开窗口、渲染若干帧、验证帧计数 > 0 后自动退出，
//! 退出码 0/1 供回归脚本判定（docs/IMPLEMENTATION_PLAN.md §2.4）。
//! release 构建 `--features no-console` 去掉控制台窗口（双击直接开 GUI）。

#![cfg_attr(feature = "no-console", windows_subsystem = "windows")]

mod agent;
mod alerts;
mod components;
mod data;
mod i18n;
mod icons;
mod mcp_runtime;
mod media;
mod media_page;
mod pages;
mod provider_models;
mod side_panel;
mod theme;
mod workspace;

use std::process::exit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use workspace::Workbench;

fn main() {
    let self_test = std::env::args().any(|arg| arg == "--self-test");
    // --trace：开启 Chrome Trace（Perfetto 兼容）性能记录，输出到工作目录 traces/
    let trace_enabled = std::env::args().any(|arg| arg == "--trace");
    // --open-port COM31 [--open-baud 115200]：启动即打开真实串口会话（快速调试入口）
    let args: Vec<String> = std::env::args().collect();
    let secondary_display = args.iter().any(|arg| arg == "--secondary-display");
    let open_port = args
        .iter()
        .position(|a| a == "--open-port")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let open_baud = args
        .iter()
        .position(|a| a == "--open-baud")
        .and_then(|i| args.get(i + 1))
        .and_then(|b| b.parse::<u32>().ok())
        .unwrap_or(115_200);
    // --workdir <目录> 覆盖保存的工作目录。默认以可执行文件目录为根。
    let cli_workdir = args
        .iter()
        .position(|a| a == "--workdir")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let frames = Arc::new(AtomicUsize::new(0));
    let core_lines = Arc::new(AtomicUsize::new(0));

    // 配置目录（bootstrap）与设置只加载一次（load_settings 对损坏文件有改名备份副作用，
    // 不能在 main 与 Workbench 各读一遍，否则损坏告警 toast 会丢失）。
    let Ok(exe_dir) = duthub_core::store::exe_dir() else {
        eprintln!("无法定位可执行文件目录");
        exit(1);
    };
    let data_dir = match duthub_core::store::initialize_config_dir(&exe_dir) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("{error}");
            exit(1);
        }
    };
    let (settings, settings_err) = match duthub_core::store::load_settings(&data_dir) {
        Ok(s) => (s, None),
        Err(err) => (
            duthub_core::store::default_settings(),
            Some(err.to_string()),
        ),
    };
    // 工作目录解析 + 创建 + 可写校验（不可用回退默认并提示）
    let (artifact_root, workdir_notice) = match duthub_core::store::ensure_portable_work_dir(
        &exe_dir,
        settings.workspace_dir.as_deref(),
        cli_workdir.as_deref(),
    ) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            exit(1);
        }
    };
    let work_dir = artifact_root.join("duthub-data");
    for component in ["audit", "runtime"] {
        if let Err(error) = duthub_core::store::ensure_portable_child(&work_dir, component) {
            eprintln!("{error}");
            exit(1);
        }
    }

    // 性能调试：--trace 开启 Chrome Trace 层（输出 <work_dir>/traces/*.json，
    // 直接拖进 ui.perfetto.dev 查看）；RUST_LOG 控制常规日志过滤（默认 info）
    let _trace_guard = if trace_enabled {
        let trace_dir = work_dir.join("traces");
        if let Err(error) = duthub_core::store::ensure_portable_child(&work_dir, "traces") {
            eprintln!("{error}");
            exit(1);
        }
        let (chrome_layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
            .file(trace_dir.join("duthub-trace.json"))
            .include_args(true)
            .build();
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        tracing_subscriber::registry()
            .with(filter)
            .with(chrome_layer)
            .init();
        eprintln!(
            "[trace] Chrome Trace 记录中 → {}",
            trace_dir.join("duthub-trace.json").display()
        );
        Some(guard)
    } else {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
        tracing_subscriber::registry().with(filter).init();
        None
    };

    gpui_kit::application()
        .with_assets(icons::IconAssets)
        .run(move |cx: &mut App| {
            // kit 全局初始化（theme/component 层），必须在开窗前调用
            gpui_kit::init(cx);

            // 隐藏系统标题栏：单行集成工具行，右侧自绘 Windows 风格控制钮
            // （WCA::Drag 命中区保留系统拖动与双击最大化行为）
            let display_id = if secondary_display {
                let primary = cx.primary_display().map(|display| display.id());
                cx.displays().into_iter().map(|display| display.id())
                    .find(|&display| Some(display) != primary)
            } else {
                None
            };
            let window_size = if display_id.is_some() {
                size(px(720.), px(450.))
            } else {
                size(px(1180.), px(760.))
            };
            let options = WindowOptions {
                display_id,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    display_id,
                    window_size,
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some(String::new().into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            };

            // 串口运行时（独立 tokio runtime；日志在工作目录 logs/ 下）。
            // Agent 接入（MCP 层）：租约管理器 + 审计接收端与写入通道同源接线
            //（TTL/auth 开关取 settings.agentAccess；settings 进程内只加载一次，坑 #9）。
            let audit = duthub_core::AuditSink::at_dir(work_dir.join("audit"));
            let lease = Arc::new(
                duthub_core::ControlLeaseManager::new(Some(audit.clone())).with_ttl(
                    std::time::Duration::from_secs(u64::from(
                        settings.agent_access.lease_ttl_secs.max(1),
                    )),
                ),
            );
            lease.set_auth_enabled(settings.agent_access.enabled);
            let artifact_quota = duthub_core::dut_quota::DutQuota::new(
                u64::from(settings.dut_storage_gib()) * duthub_core::dut_quota::GIB,
            );
            let device = Arc::new(
                duthub_device::DeviceService::custom(1024, work_dir.join("logs"), 25)
                    .with_portable_archives(artifact_root.clone(), settings.terminal.save_logs.unwrap_or(true), artifact_quota.clone())
                    .with_lease_authority(lease.clone())
                    .with_audit_sink(audit.clone()),
            );

            let frames_in_view = frames.clone();
            let core_lines_in_view = core_lines.clone();
            let opened = gpui_kit::open_window(options, cx, |_window, cx| {
                cx.new(|cx| {
                    Workbench::new(
                        cx,
                        device.clone(),
                        frames_in_view,
                        core_lines_in_view,
                        open_port.clone(),
                        open_baud,
                        workspace::StartupContext {
                            data_dir,
                            work_dir,
                            artifact_root,
                            artifact_quota,
                            settings,
                            notice: settings_err.or(workdir_notice),
                        },
                    )
                })
            });

            if let Err(err) = opened {
                eprintln!("打开主窗口失败: {err:#}");
                exit(1);
            }
            cx.activate(true);

            if self_test {
                let frames = frames.clone();
                let core_lines = core_lines.clone();
                cx.spawn(async move |cx: &mut gpui_kit::AsyncApp| {
                    cx.background_executor()
                        .timer(Duration::from_millis(1200))
                        .await;
                    let rendered = frames.load(Ordering::Relaxed);
                    let from_core = core_lines.load(Ordering::Relaxed);
                    if rendered == 0 {
                        eprintln!("SELF-TEST FAILED: frames={rendered}");
                        exit(1);
                    }
                    println!(
                        "SELF-TEST OK: {rendered} frames · {from_core} core lines · duthub-desktop v{} (gpui-kit)",
                        env!("CARGO_PKG_VERSION")
                    );
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });
}

// 保持 Mutex 导入使用（self-test 变量在非 self-test 构建中也编译）
#[allow(unused)]
fn _keep_imports(_: Option<Mutex<()>>) {}
