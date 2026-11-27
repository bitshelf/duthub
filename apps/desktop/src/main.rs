//! duthub-desktop：GPUI 桌面端（UI 1:1 复刻原型 duthub.html）。
//!
//! `--self-test`：无交互冒烟——打开窗口、渲染若干帧、验证帧计数 > 0 后自动退出，
//! 退出码 0/1 供回归脚本判定（docs/README.md）。
//! release 构建 `--features no-console` 去掉控制台窗口（双击直接开 GUI）。

#![cfg_attr(feature = "no-console", windows_subsystem = "windows")]

mod a2a_runtime;
mod agent;
mod alerts;
mod boot_learning;
mod components;
mod data;
mod device_watch;
mod garble_ui;
mod highlight;
mod i18n;
mod icons;
mod mcp_runtime;
mod media;
mod media_page;
mod monitor_window;
mod pages;
mod provider_models;
mod side_panel;
mod terminal_selection;
mod theme;
mod workspace;

use std::process::exit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gpui_kit::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use workspace::Workbench;

/// Single-instance guard (tray design §2.3).
///
/// The tray icon identity is derived from the executable path, so two copies
/// of the same binary would fight over one notification-area icon — the second
/// instance could end up hidden with no reachable UI.
///
/// The Win32 part lives in the tray port
/// (`duthub_desktop::tray::single_instance`) so this file never links the
/// `windows` crate: a named `CreateMutexW`
/// (`Local\duthub-<hash of the lowercased exe path>`) is created before any
/// window exists, and a second instance sees `ERROR_ALREADY_EXISTS`, gets a
/// `MessageBoxW` notice and exits. The guard handle is kept alive by `main`
/// for the life of the process (dropping it would let a second instance start
/// while this one is hidden in the tray). `--self-test` skips the guard.
fn main() {
    // Contain the process tree before gpui or any child process exists: a
    // crash or a forced kill closes the leaked job handle, which terminates
    // ffmpeg/ffplay descendants and releases the capture device. The job nests
    // under whatever job launched us (Windows 8+ allows job hierarchies);
    // failure is fail-open — warn and keep starting, since Capture::Drop still
    // covers graceful exits.
    if let Err(error) = duthub_desktop::agent_host::job::contain_current_process() {
        eprintln!("进程容器 Job Object 创建失败（强杀时无法回收 ffmpeg 等子进程）: {error}");
    }
    let self_test = std::env::args().any(|arg| arg == "--self-test");
    // --trace：开启 Chrome Trace（Perfetto 兼容）性能记录，输出到工作目录 traces/
    let trace_enabled = std::env::args().any(|arg| arg == "--trace");
    // --open-port COM31 [--open-baud 115200]：启动即打开真实串口会话（快速调试入口）
    let args: Vec<String> = std::env::args().collect();
    let secondary_display = args.iter().any(|arg| arg == "--secondary-display");
    let release_gui_lease = args.iter().any(|arg| arg == "--release-gui-lease");
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
    let frames = Arc::new(AtomicUsize::new(0));
    let core_lines = Arc::new(AtomicUsize::new(0));

    // 配置目录（bootstrap）与设置只加载一次（load_settings 对损坏文件有改名备份副作用，
    // 不能在 main 与 Workbench 各读一遍，否则损坏告警 toast 会丢失）。
    let Ok(exe_dir) = duthub_core::store::exe_dir() else {
        eprintln!("无法定位可执行文件目录");
        exit(1);
    };
    let explicit_root =
        duthub_core::store::explicit_workspace(&exe_dir, std::env::args_os().skip(1))
            .unwrap_or_else(|error| {
                eprintln!("{error}");
                exit(1)
            });
    let config_root = explicit_root.as_deref().unwrap_or(&exe_dir);
    if explicit_root.is_some() && !duthub_core::store::dir_writable(config_root) {
        eprintln!("Workspace is not writable: {}", config_root.display());
        exit(1);
    }
    let data_dir = match duthub_core::store::initialize_config_dir(config_root) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("{error}");
            exit(1);
        }
    };
    let (mut settings, mut settings_err) = match duthub_core::store::load_settings(&data_dir) {
        Ok(s) => (s, None),
        Err(err) => (
            duthub_core::store::default_settings(),
            Some(err.to_string()),
        ),
    };
    crate::i18n::Lang::from_setting(&settings.appearance.language).activate();
    if !self_test {
        for warning in duthub_core::store::hydrate_mcp_tokens(&data_dir, &mut settings) {
            let localized = match warning {
                duthub_core::store::McpTokenHydrateWarning::MigrateFailed {
                    credential_id,
                    error,
                } => crate::i18n::message(
                    "mcpTokenMigrateFailed",
                    &[("id", &credential_id), ("error", &error)],
                ),
                duthub_core::store::McpTokenHydrateWarning::MissingInKeyring { credential_id } => {
                    crate::i18n::message("mcpTokenMissingInKeyring", &[("id", &credential_id)])
                }
                duthub_core::store::McpTokenHydrateWarning::ReadFailed {
                    credential_id,
                    error,
                } => crate::i18n::message(
                    "mcpTokenReadFailed",
                    &[("id", &credential_id), ("error", &error)],
                ),
                duthub_core::store::McpTokenHydrateWarning::RewriteFailed { error } => {
                    crate::i18n::message("mcpTokenRewriteFailed", &[("error", &error)])
                }
            };
            eprintln!("[keyring] {localized}");
            if settings_err.is_none() {
                settings_err = Some(localized);
            }
        }
    }
    // Single instance (before any window exists; `--self-test` stays exempt so
    // several harness runs can overlap). The handle is held until `main`
    // returns — it must outlive a window hidden to the notification area.
    let _instance_guard = if self_test {
        None
    } else if let Ok(guard) = duthub_desktop::tray::single_instance::acquire() { Some(guard) } else {
        crate::i18n::Lang::from_setting(&settings.appearance.language).activate();
        duthub_desktop::tray::single_instance::show_already_running(
            crate::i18n::label("alreadyRunningTitle"),
            crate::i18n::label("alreadyRunningBody"),
        );
        exit(0);
    };
    // 工作目录解析 + 创建 + 可写校验（不可用回退默认并提示）
    let (artifact_root, workdir_notice) = match duthub_core::store::ensure_portable_work_dir(
        config_root,
        settings
            .workspace_dir
            .as_deref()
            .filter(|_| explicit_root.is_none()),
        None,
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
            // F10：桌面租约链必须与 DeviceService 同口径启用 GUI 会话生命周期
            //（crates/duthub-device/src/lib.rs:91-95），否则 GUI 自动租约会被 TTL 过期。
            let audit = duthub_core::AuditSink::at_dir(work_dir.join("audit"));
            let lease = Arc::new(
                duthub_core::ControlLeaseManager::new(Some(audit.clone()))
                    .with_ttl(std::time::Duration::from_secs(u64::from(
                        settings.agent_access.lease_ttl_secs.max(1),
                    )))
                    .with_gui_session_lifetime(),
            );
            lease.set_auth_enabled(settings.agent_access.enabled);
            let artifact_quota = duthub_core::dut_quota::DutQuota::new(
                u64::from(settings.dut_storage_gib()) * duthub_core::dut_quota::GIB,
            );
            // Shared multi-thread Tokio runtime for DeviceService, ControlService,
            // and A2aRuntime (eliminates redundant per-subsystem runtimes).
            let shared_runtime = Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("duthub-async")
                    .enable_all()
                    .build()
                    .expect("shared desktop tokio runtime"),
            );
            let device_builder = duthub_device::DeviceService::with_runtime(
                shared_runtime.handle().clone(),
                1024,
                work_dir.join("logs"),
                25,
            )
            .with_portable_archives(
                artifact_root.clone(),
                true,
                artifact_quota.clone(),
            );
            let device_builder = match settings.serial.scrollback_rows {
                Some(rows) => device_builder.with_scrollback(rows as usize),
                None => device_builder,
            };
            let device = Arc::new(
                device_builder
                    .with_lease_authority(lease.clone())
                    .with_audit_sink(audit.clone()),
            );

            let frames_in_view = frames.clone();
            let core_lines_in_view = core_lines.clone();
            let opened = gpui_kit::open_window(options, cx, |_window, cx| {
                cx.new(|cx| {
                    let mut view = Workbench::new(
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
                            self_test,
                            runtime: shared_runtime.clone(),
                        },
                    );
                    if release_gui_lease {
                        for session in &view.sessions {
                            lease.release_for_holder(
                                &session.dut_id,
                                &duthub_core::LeaseHolder::Gui,
                            );
                        }
                    }
                    if self_test {
                        view.set_config_page(Some(workspace::ConfigPage::AgentModel));
                    }
                    view
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
