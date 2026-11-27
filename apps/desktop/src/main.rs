//! duthub-desktop: GPUI desktop interface (a precise 1:1 replica of the duthub.html interface).
//!
//! `--self-test`: No interactive testing — opens a window, renders several frames, and
//! automatically exits when the frame count is greater than 0. The exit code 0/1 is used by
//! regression scripts for determination (docs/README.md). `release` build with `--features
//! no-console` removes the console window (directly opens GUI by double-clicking).

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

use anyhow::{Context as _, ensure};
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
struct BootstrapPaths {
    config_root: std::path::PathBuf,
    explicit_root: Option<std::path::PathBuf>,
    data_dir: std::path::PathBuf,
}

/// Resolves the workspace root and initializes `.duthub/` exactly once at startup.
fn bootstrap_data_dir() -> anyhow::Result<BootstrapPaths> {
    let exe_dir = duthub_core::store::exe_dir()
        .map_err(anyhow::Error::msg)
        .context("Could not locate the executable directory")?;
    let explicit_root =
        duthub_core::store::explicit_workspace(&exe_dir, std::env::args_os().skip(1))
            .map_err(anyhow::Error::msg)?;
    let config_root = explicit_root.clone().unwrap_or(exe_dir);
    ensure!(
        explicit_root.is_none() || duthub_core::store::dir_writable(&config_root),
        "Workspace is not writable: {}",
        config_root.display()
    );
    let data_dir =
        duthub_core::store::initialize_config_dir(&config_root).map_err(anyhow::Error::msg)?;
    Ok(BootstrapPaths {
        config_root,
        explicit_root,
        data_dir,
    })
}

fn main() {
    // Contain the process tree before gpui or any child process exists: a
    // crash or a forced kill closes the leaked job handle, which terminates
    // ffmpeg/ffplay descendants and releases the capture device. The job nests
    // under whatever job launched us (Windows 8+ allows job hierarchies);
    // failure is fail-open — warn and keep starting, since Capture::Drop still
    // covers graceful exits.
    if let Err(error) = duthub_desktop::agent_host::job::contain_current_process() {
        eprintln!(
            "Failed to create the Job Object. Forced termination may leave FFmpeg and other children running: {error}"
        );
    }
    let self_test = std::env::args().any(|arg| arg == "--self-test");
    // --trace: Enable Chrome Trace (compatible with Perfetto) functionality. Output data to the
    // traces/ directory in the working directory.
    let trace_enabled = std::env::args().any(|arg| arg == "--trace");
    // --open-port COM31 [--open-baud 115200]: Enables the real serial port session (for quick
    // debugging purposes)
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
    // --workdir <Directory> Specifies the directory where the work is saved. By default, it uses
    // the directory containing the executable file as the root directory.
    let frames = Arc::new(AtomicUsize::new(0));
    let core_lines = Arc::new(AtomicUsize::new(0));

    let BootstrapPaths {
        config_root,
        explicit_root,
        data_dir,
    } = match bootstrap_data_dir() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error:#}");
            exit(1);
        }
    };
    let config_root = config_root.as_path();
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
    } else if let Ok(guard) = duthub_desktop::tray::single_instance::acquire() {
        Some(guard)
    } else {
        crate::i18n::Lang::from_setting(&settings.appearance.language).activate();
        duthub_desktop::tray::single_instance::show_already_running(
            crate::i18n::label("alreadyRunningTitle"),
            crate::i18n::label("alreadyRunningBody"),
        );
        exit(0);
    };
    // Working directory parsing + creation + writeable verification (disabled by default; prompts
    // are available for those who need them).
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

    // Enable Chrome Trace with `--trace`. Open the JSON trace from the work directory's traces
    // subdirectory in ui.perfetto.dev. `RUST_LOG` sets the standard log filter and defaults to
    // info.
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
            "[trace] Recording Chrome Trace to {}",
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
            // Global initialization of the kit (at the theme/component level). This must be called
            // before opening the window.
            gpui_kit::init(cx);

            // Hide the system title bar: Single-line integration tool bar, with control buttons in
            // Windows-style design on the right side.
            // (WCA::Drag area retains the system's drag-and-drop and double-click maximize
            // functions.)
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

            // Serial port is running (independent tokio runtime; logs/ directory for logs storage).
            // Agent integration (MCP layer): Lease manager + audit receiving end and writing
            // channel are connected to the same source.
            // (TTL/auth settings are determined by settings.agentAccess; settings is loaded only
            // once within the process, issue #9).
            // F10: The desktop lease chain must enable GUI session lifecycle along with
            // DeviceService.
            // (crates/duthub-device/src/lib.rs:91-95); otherwise, the automatic GUI lease will
            // expire due to TTL expiration.
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
                        open_port.as_deref(),
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
                eprintln!("Failed to open the main window: {err:#}");
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
