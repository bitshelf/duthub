//! Tools attached to the active serial session.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use duthub_adb::{AdbClient, AndroidPreview, TargetUnavailable, choose_target};
use duthub_core::ipc::ScreenSnapshot;
use duthub_core::lease::{ControlLeaseManager, LeaseAuthority, WriteCtx, WriteIdentity};
use duthub_core::model::{AdbConfig, CaptureInput, DutConfig, DutMediaConfig, ToolsSettings};
use duthub_core::process::resolve_executable;
use duthub_device::term::{TermGrid, create_term, snapshot_screen};
use gpui_kit::base::Selectable;
use gpui_kit::{
    AnyElement, Context, Div, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ObjectFit, ParentElement, Pixels, Point, RenderImage,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, StyledImage, Window, div, img,
    px,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::components::{BtnVariant, btn, icon_btn, panel_head, subtle};
use crate::media::Source;
use crate::theme::Palette;
use crate::workspace::{ViewMode, Workbench};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Agent,
    Shell,
    Monitor,
}

#[derive(Default)]
pub struct SidePanel {
    pub visible: Option<Panel>,
    dut_id: Option<String>,
    shell: Option<Shell>,
    shell_binding: Option<AdbConfig>,
    shell_probe: Option<gpui_kit::Task<()>>,
    shell_connecting: bool,
    shell_focus_pending: bool,
    resolved_adb: Option<String>,
    android_preview: Option<AndroidPreview>,
    adb_preview: Option<AdbPreview>,
    adb_preview_error: Option<String>,
    focus: Option<FocusHandle>,
    shell_scroll: ScrollHandle,
    shell_selection: Option<crate::terminal_selection::TerminalSelection>,
    source: Option<Source>,
    monitor_input: Option<CaptureInput>,
    /// Android/ADB preview frame (Shell panel). The HDMI monitor frame lives
    /// in `Workbench::frame_pump` — one consumer, both windows read clones
    /// (`docs/design/CAPTURE_STREAM_BACKENDS.md` §6.3).
    adb_frame: Option<Arc<RenderImage>>,
    /// Shell/ADB error text; the monitor error comes from the frame pump.
    error: Option<String>,
}

struct Shell {
    pty: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    tx: Option<mpsc::SyncSender<Vec<u8>>>,
    rx: Arc<Mutex<Option<ScreenSnapshot>>>,
    screen: Option<ScreenSnapshot>,
    grid: PtySize,
    resize: Arc<Mutex<PtySize>>,
    grant: WriteCtx,
    renewed: Instant,
    exited: bool,
}

fn shell_options(binding: &AdbConfig) -> Option<(&'static str, &'static str, &str)> {
    let target = binding.serial.trim();
    if target.is_empty() && !binding.auto_detect() {
        return None;
    }
    Some(if binding.tool == "hdc" {
        ("hdc", "-t", target)
    } else {
        ("adb", "-s", target)
    })
}

struct ShellAccess {
    lease: Arc<ControlLeaseManager>,
    dut_id: String,
    grant: WriteCtx,
}

impl Shell {
    fn start(
        spec: &str,
        selector: &str,
        serial: &str,
        access: ShellAccess,
    ) -> Result<Self, String> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let exe = resolve_executable(spec).map_err(|err| err.to_string())?;
        let mut command = CommandBuilder::new(exe);
        command.args([selector, serial, "shell"]);
        command.env("TERM", "xterm-256color");
        let grid = PtySize {
            cols: 44,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(grid)
            .map_err(|err| err.to_string())?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| err.to_string())?;
        let mut writer = pair.master.take_writer().map_err(|err| err.to_string())?;
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|err| err.to_string())?;
        drop(pair.slave);
        let (tx, input) = mpsc::sync_channel::<Vec<u8>>(32);
        let stopped = Arc::new(AtomicBool::new(false));
        let closed = stopped.clone();
        let grant = access.grant.clone();
        std::thread::spawn(move || {
            while let Ok(bytes) = input.recv() {
                let _io = access.lease.serial_io().blocking_read();
                if closed.load(Ordering::Acquire)
                    || access
                        .lease
                        .validate_ctx(&access.dut_id, &access.grant)
                        .is_err()
                {
                    break;
                }
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let rx = Arc::new(Mutex::new(None));
        let output = rx.clone();
        let resize = Arc::new(Mutex::new(grid));
        let target_size = resize.clone();
        std::thread::spawn(move || {
            let mut term = create_term();
            let mut parser = Processor::<StdSyncHandler>::new();
            let mut bytes = [0; 8192];
            let mut old_size = None;
            while let Ok(count @ 1..) = reader.read(&mut bytes) {
                let size = *target_size.lock().unwrap();
                if old_size != Some(size) {
                    term.resize(TermGrid::new(size.cols as usize, size.rows as usize));
                    old_size = Some(size);
                }
                parser.advance(&mut term, &bytes[..count]);
                *output.lock().unwrap() = Some(snapshot_screen(&term));
            }
        });
        Ok(Self {
            pty: pair.master,
            child: Some(child),
            stopped,
            tx: Some(tx),
            rx,
            screen: None,
            grid,
            resize,
            grant,
            renewed: Instant::now(),
            exited: false,
        })
    }

    fn update_size(
        &mut self,
        width: Pixels,
        height: Pixels,
        cell_width: Pixels,
        row_height: Pixels,
    ) {
        let size = PtySize {
            cols: ((f32::from(width) - 28.) / f32::from(cell_width)).clamp(20., 200.) as u16,
            rows: ((f32::from(height) - 120.) / f32::from(row_height)).clamp(8., 200.) as u16,
            ..self.grid
        };
        if size != self.grid && !self.exited {
            *self.resize.lock().unwrap() = size;
            if self.pty.resize(size).is_ok() {
                self.grid = size;
            }
        }
    }

    fn tick(&mut self) -> bool {
        let screen = self.rx.lock().unwrap().take();
        let changed = screen.is_some();
        if changed {
            self.screen = screen;
        }
        if !self.exited
            && self
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
        {
            self.exited = true;
            self.tx.take();
            return true;
        }
        changed
    }
}

fn usb_executable<'a>(tools: &'a ToolsSettings, tool: &'static str) -> &'a str {
    let configured = if tool == "hdc" {
        &tools.hdc_executable
    } else {
        &tools.adb_executable
    };
    if configured.trim().is_empty() {
        tool
    } else {
        configured.trim()
    }
}

type PreviewFrame = Result<Arc<RenderImage>, String>;

struct AdbPreview {
    latest: Arc<Mutex<Option<PreviewFrame>>>,
    _stop: mpsc::Sender<()>,
}

impl AdbPreview {
    fn start(source: AndroidPreview) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let frames = latest.clone();
        let (stop, stopped) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let frame = source
                    .capture()
                    .map_err(|err| err.to_string())
                    .and_then(decode_preview);
                let failed = frame.is_err();
                *frames.lock().unwrap() = Some(frame);
                if failed
                    || !matches!(
                        stopped.recv_timeout(Duration::from_millis(400)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    )
                {
                    break;
                }
            }
        });
        Self {
            latest,
            _stop: stop,
        }
    }
}

fn decode_preview(png: Vec<u8>) -> Result<Arc<RenderImage>, String> {
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(80 * 1024 * 1024);
    reader.limits(limits);
    let mut pixels = reader
        .decode()
        .map_err(|err| err.to_string())?
        .thumbnail(1280, 1280)
        .into_rgba8();
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(smallvec::smallvec![
        image::Frame::new(pixels)
    ])))
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        self.tx.take();
        if !self.exited
            && let Some(mut child) = self.child.take()
        {
            std::thread::spawn(move || {
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

impl Workbench {
    pub(crate) fn panel_dut(&self) -> Option<&DutConfig> {
        let id = &self.sessions.get(self.active_session)?.dut_id;
        self.duts.file.duts.iter().find(|dut| &dut.id == id)
    }

    pub fn tick_side_panel(&mut self) -> bool {
        let mut changed = false;
        if let Some(shell) = &mut self.side_panel.shell {
            changed = shell.tick();
            if !shell.exited && shell.renewed.elapsed() >= Duration::from_secs(5) {
                shell.renewed = Instant::now();
                let renewed = self
                    .device
                    .lease_manager()
                    .zip(self.side_panel.dut_id.as_ref())
                    .is_some_and(|(lease, dut)| lease.renew(dut, &shell.grant.lease_id).is_ok());
                if !renewed {
                    self.side_panel.shell = None;
                    self.side_panel.error = Some(crate::i18n::label("usbControlReleased").into());
                    changed = true;
                }
            }
        }
        changed
            | self
                .side_panel
                .adb_preview
                .as_ref()
                .is_some_and(|preview| preview.latest.lock().unwrap().is_some())
    }

    fn start_shell(&mut self, dut: &DutConfig, window: &mut Window, cx: &mut Context<Self>) {
        let binding = self.duts.saved_usb(&dut.id).cloned();
        self.side_panel.shell_binding = binding.clone();
        self.side_panel.shell = None;
        self.side_panel.shell_selection = None;
        self.side_panel.android_preview = None;
        self.side_panel.adb_preview = None;
        self.side_panel.resolved_adb = None;
        if let Some(frame) = self.side_panel.adb_frame.take() {
            cx.drop_image(frame, Some(window));
        }
        let Some(binding) = binding else {
            return;
        };
        let Some((tool, selector, serial)) = shell_options(&binding) else {
            return;
        };
        let configured = usb_executable(&self.settings.tools, tool).to_owned();
        // Downloaded tool first: explicit path > .duthub/tools install > PATH.
        let spec = duthub_desktop::tool_resolver::effective_spec(&configured, tool);
        if tool == "hdc" {
            self.open_resolved_shell(&dut.id, &spec, selector, serial, cx);
            return;
        }
        let others: Vec<_> = self
            .duts
            .other_usb_bindings(&dut.id)
            .filter(|binding| binding.tool != "hdc")
            .collect();
        if binding.auto_detect() && others.iter().any(|binding| binding.auto_detect()) {
            self.side_panel.error = Some(crate::i18n::label("adbAutoConflict").into());
            return;
        }
        let reserved: Vec<_> = others
            .iter()
            .map(|config| config.serial.trim().to_owned())
            .collect();
        let manual = (!binding.auto_detect()).then(|| serial.to_owned());
        let dut_id = dut.id.clone();
        let executable = spec.clone();
        let probe = cx.background_executor().spawn(async move {
            let client = AdbClient::real();
            let devices = client.devices_l(&executable).map_err(|error| {
                crate::i18n::message("usbDiscoveryFailed", &[("error", &error.to_string())])
            })?;
            let target = choose_target(&devices, manual.as_deref(), &reserved).map_err(
                |error| match error {
                    TargetUnavailable::Missing => manual.as_deref().map_or_else(
                        || crate::i18n::label("noAdbDetected").into(),
                        |serial| crate::i18n::message("adbBoundMissing", &[("serial", serial)]),
                    ),
                    TargetUnavailable::NotReady(state) => {
                        crate::i18n::message("usbBindingNotReady", &[("state", &state)])
                    }
                    TargetUnavailable::Ambiguous => crate::i18n::label("adbAutoAmbiguous").into(),
                },
            )?;
            let preview = client.android_preview(&executable, target);
            Ok::<_, String>((target.to_owned(), preview))
        });
        self.side_panel.shell_connecting = true;
        self.side_panel.shell_probe = Some(cx.spawn(async move |view, cx| {
            let result = probe.await;
            let _ = view.update(cx, |this, cx| {
                if this.side_panel.visible != Some(Panel::Shell)
                    || this.side_panel.dut_id.as_deref() != Some(dut_id.as_str())
                {
                    return;
                }
                this.side_panel.shell_connecting = false;
                match result {
                    Ok((serial, preview)) => {
                        this.open_resolved_shell(&dut_id, &spec, selector, &serial, cx);
                        this.side_panel.resolved_adb = Some(serial);
                        this.side_panel.android_preview = preview;
                    }
                    Err(error) => this.side_panel.error = Some(error),
                }
                cx.notify();
            });
        }));
    }

    fn open_resolved_shell(
        &mut self,
        dut_id: &str,
        spec: &str,
        selector: &str,
        serial: &str,
        cx: &mut Context<Self>,
    ) {
        let result = (|| {
            let lease = self
                .device
                .lease_manager()
                .ok_or("Control service unavailable")?;
            let grant = lease
                .admit_write(dut_id, &WriteIdentity::gui())
                .map_err(|err| err.message())?
                .ok_or("Missing GUI lease")?;
            Shell::start(
                spec,
                selector,
                serial,
                ShellAccess {
                    lease,
                    dut_id: dut_id.to_owned(),
                    grant,
                },
            )
        })();
        match result {
            Ok(shell) => {
                self.side_panel.shell = Some(shell);
                self.side_panel
                    .focus
                    .get_or_insert_with(|| cx.focus_handle());
                self.side_panel.shell_focus_pending = true;
            }
            Err(err) => self.side_panel.error = Some(err),
        }
    }

    fn start_monitor(
        &mut self,
        dut_id: String,
        source: Source,
        input: CaptureInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // While the preview lives in the float window nothing may re-target or
        // re-open it (§6.2 `Floating + PanelSwitched` = no change).
        if self.frame_pump.placement() == duthub_desktop::capture_ui::MonitorPlacement::Floating {
            return;
        }
        self.close_side_panel(window, cx);
        self.side_panel.visible = Some(Panel::Monitor);
        self.side_panel.dut_id = Some(dut_id.clone());
        self.side_panel.source = Some(source);
        let settings = self.media.capture_settings().clone();
        let result = self
            .media
            .open_inline(&self.duts, &dut_id, source, &settings);
        self.side_panel.monitor_input = Some(input);
        match result {
            Ok((preview, _created)) => {
                // The pump owns the preview and the decoded frame from here on.
                self.frame_pump
                    .attach(cx, Some(window), (dut_id, source), preview);
                self.frame_pump
                    .set_placement(duthub_desktop::capture_ui::MonitorPlacement::Docked);
            }
            Err(err) => self.frame_pump.set_error(Some(err)),
        }
    }

    fn monitor_target(&self, preferred: Option<Source>) -> Option<(&str, Source, &CaptureInput)> {
        if self.view != ViewMode::Serial {
            return None;
        }
        let dut = self.panel_dut()?;
        let media = self.duts.saved_media(&dut.id)?;
        let first = monitor_source(media, preferred)?.0;
        std::iter::once(first)
            .chain(Source::sources(media).filter(|source| *source != first))
            .find_map(|source| {
                let input = crate::media_page::saved_capture(&self.duts, &dut.id, source).ok()?;
                Some((dut.id.as_str(), source, input))
            })
    }

    pub(crate) fn sync_monitor_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.side_panel.visible != Some(Panel::Monitor) {
            return;
        }
        // The float window owns the preview; retargeting it here would open a
        // second capture behind it (§6.2).
        if self.frame_pump.placement() == duthub_desktop::capture_ui::MonitorPlacement::Floating {
            return;
        }
        let preferred = self
            .panel_dut()
            .filter(|dut| Some(dut.id.as_str()) == self.side_panel.dut_id.as_deref())
            .and(self.side_panel.source);
        let Some((id, source, input)) = self.monitor_target(preferred) else {
            self.close_side_panel(window, cx);
            return;
        };
        if self.side_panel.dut_id.as_deref() != Some(id)
            || self.side_panel.source != Some(source)
            || self
                .side_panel
                .monitor_input
                .as_ref()
                .is_none_or(|saved| !saved.same_capture(input))
        {
            self.start_monitor(id.to_owned(), source, input.clone(), window, cx);
        }
    }

    pub fn open_side_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        if panel == Panel::Monitor {
            if self.frame_pump.placement() == duthub_desktop::capture_ui::MonitorPlacement::Floating
            {
                // Keep the right-hand Monitor pane collapsed while floating so
                // the terminal retains full width; focus the existing float
                // window instead of re-opening a placeholder pane.
                self.focus_float_window(cx);
                cx.notify();
                return;
            }
            if let Some((id, source, input)) = self.monitor_target(None) {
                self.start_monitor(id.to_owned(), source, input.clone(), window, cx);
                cx.notify();
            }
            return;
        }
        // Switching to another panel: `PanelSwitched` (not a plain close) so
        // the transition table stays the single source of truth.
        self.close_side_panel_with(
            window,
            cx,
            duthub_desktop::capture_ui::MonitorEvent::PanelSwitched,
        );
        if panel == Panel::Agent && self.sessions.get(self.active_session).is_some() {
            self.side_panel.visible = Some(panel);
            self.view = ViewMode::Serial;
        } else if let Some(dut) = self.panel_dut().cloned() {
            self.side_panel.visible = Some(panel);
            self.side_panel.dut_id = Some(dut.id.clone());
            match panel {
                Panel::Shell => self.start_shell(&dut, window, cx),
                Panel::Agent | Panel::Monitor => unreachable!(),
            }
        }
        cx.notify();
    }

    /// Close the side panel through the placement state machine (§6.2).
    ///
    /// `Docked` releases the `Monitor` lease (the ledger decides whether the
    /// capture stops); `Floating` leaves the preview and the lease with the
    /// float window instead of killing its frame source.
    pub fn close_side_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_side_panel_with(
            window,
            cx,
            duthub_desktop::capture_ui::MonitorEvent::SidePanelClosed,
        );
    }

    /// Shared body of `SidePanelClosed` / `PanelSwitched`; the table maps both
    /// to the same outcome, but each call site names its real event.
    fn close_side_panel_with(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        event: duthub_desktop::capture_ui::MonitorEvent,
    ) {
        if let Some(frame) = self.side_panel.adb_frame.take() {
            cx.drop_image(frame, Some(window));
        }
        let (placement, action) =
            duthub_desktop::capture_ui::next(self.frame_pump.placement(), event);
        self.frame_pump.set_placement(placement);
        if action.contains(duthub_desktop::capture_ui::MonitorAction::StopPreview) {
            self.release_monitor(Some(window), cx);
        }
        self.side_panel = SidePanel::default();
    }

    pub fn render_side_strip(
        &self,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let lang = self.lang;
        let dut = self.panel_dut();
        let shell = dut
            .and_then(|dut| self.duts.saved_usb(&dut.id))
            .and_then(shell_options)
            .map(|(tool, _, _)| tool);
        let monitor = self.monitor_target(None).is_some();
        let manual = self
            .device
            .lease_manager()
            .is_some_and(|lease| lease.manual_control());
        let items = [
            (
                Panel::Shell,
                "side-shell",
                if shell == Some("adb") {
                    "android"
                } else {
                    "terminal"
                },
                if shell == Some("hdc") {
                    "HDC Shell"
                } else {
                    "ADB Shell"
                },
                shell.is_some() && self.view == ViewMode::Serial,
            ),
            (
                Panel::Monitor,
                "side-monitor",
                "eye",
                crate::i18n::t(lang, "deviceMonitor"),
                monitor,
            ),
            (Panel::Agent, "side-agent", "spark", "Agent", true),
        ];
        let mut strip = div().flex().items_center().gap(px(5.)).flex_none();
        for (panel, id, icon, label, enabled) in items {
            if !enabled {
                continue;
            }
            let active = self.side_panel.visible == Some(panel);
            strip = strip.child(
                icon_btn(id, icon, p)
                    .tooltip(label)
                    .selected(active)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if active {
                            this.close_side_panel(window, cx);
                            cx.notify();
                        } else {
                            this.open_side_panel(panel, window, cx);
                        }
                    })),
            );
        }
        strip.child(
            icon_btn("manual-control", "shield", p)
                .tooltip(if manual {
                    crate::i18n::t(lang, "releaseManualControl")
                } else {
                    crate::i18n::t(lang, "takeManualControlOfAllDuts")
                })
                .selected(manual)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(lease) = this.device.lease_manager() {
                        match lease.set_manual_control(!manual) {
                            Ok(()) => this.notify_toast(if manual {
                                crate::i18n::t(lang, "manualControlReleased")
                            } else {
                                crate::i18n::t(lang, "manualControlEnabledAgentWritesAre")
                            }),
                            Err(err) => this.notify_toast(err.message()),
                        }
                    }
                    cx.notify();
                })),
        )
    }

    pub fn render_side_content(
        &mut self,
        width: Pixels,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // W4c：demo Agent 面板已卸载——`render_agent_pane` 不再挂载。无打开
        // 面板时右栏宽度为 0（`pane_widths` 只在 `visible.is_some()` 时预留），
        // 空占位即可；Panel::Agent 渲染接真实 SessionView 的 Inspector。
        let Some(panel) = self.side_panel.visible else {
            return div().into_any_element();
        };
        if panel == Panel::Agent {
            return crate::pages::agent_inspector_panel(self, p).into_any_element();
        }
        let dut = self.panel_dut().cloned();
        let binding = dut.as_ref().and_then(|dut| self.duts.saved_usb(&dut.id));
        if panel == Panel::Shell
            && (self.side_panel.dut_id.as_deref() != dut.as_ref().map(|dut| dut.id.as_str())
                || self.side_panel.shell_binding.as_ref() != binding)
        {
            self.open_side_panel(panel, window, cx);
        }
        if self.side_panel.shell_focus_pending && self.side_panel.adb_preview.is_none() {
            self.side_panel.shell_focus_pending = false;
            if let Some(focus) = &self.side_panel.focus {
                window.focus(focus, cx);
            }
        }
        let lang = self.lang;
        let title = match panel {
            Panel::Shell => {
                if dut
                    .as_ref()
                    .and_then(|dut| self.duts.saved_usb(&dut.id))
                    .and_then(shell_options)
                    .is_some_and(|(tool, _, _)| tool == "hdc")
                {
                    "HDC Shell"
                } else {
                    "ADB Shell"
                }
            }
            _ => crate::i18n::t(lang, "deviceMonitor"),
        };
        let mut head_actions = div().flex().items_center().gap(px(2.));
        if panel == Panel::Monitor && self.frame_pump.key().is_some() {
            head_actions = head_actions
                .child(
                    icon_btn("monitor-float", "copy", p)
                        .tooltip(crate::i18n::t(lang, "monitorFloat"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.frame_pump.placement()
                                != duthub_desktop::capture_ui::MonitorPlacement::Floating
                            {
                                this.handle_monitor_event(
                                    duthub_desktop::capture_ui::MonitorEvent::OpenFloat,
                                    cx,
                                );
                            } else {
                                this.focus_float_window(cx);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    icon_btn("monitor-fullscreen", "maximize-2", p)
                        .tooltip(crate::i18n::t(lang, "monitorFullscreen"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_monitor_fullscreen(cx);
                            cx.notify();
                        })),
                );
        }
        head_actions = head_actions.child(
            icon_btn("close-side-tool", "x", p)
                .tooltip(crate::i18n::t(lang, "close"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_side_panel(window, cx);
                    cx.notify();
                })),
        );
        let mut body = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(p.surface)
            .child(
                panel_head(
                    if panel == Panel::Shell {
                        "terminal"
                    } else {
                        "eye"
                    },
                    title,
                    p,
                )
                .child(head_actions),
            );
        if let Some(dut) = dut.as_ref() {
            body = body.child(
                div()
                    .px(px(14.))
                    .pb(px(6.))
                    .text_sm()
                    .text_color(p.muted)
                    .child(self.side_panel.resolved_adb.as_ref().map_or_else(
                        || dut.name.clone(),
                        |serial| format!("{} · {serial}", dut.name),
                    )),
            );
            body = match panel {
                Panel::Shell => {
                    if self.side_panel.android_preview.is_some() {
                        body = body.child(self.render_adb_view_tabs(p, cx));
                    }
                    if self.side_panel.adb_preview.is_some() {
                        body.child(self.render_adb_preview(p, window, cx))
                    } else {
                        body.child(self.render_shell(dut, width, p, window, cx))
                    }
                }
                Panel::Monitor => body.child(self.render_monitor(dut, p, window, cx)),
                Panel::Agent => unreachable!(),
            };
        }
        if panel == Panel::Shell
            && let Some(error) = self.side_panel.error.clone()
        {
            body = body.child(div().p(px(12.)).text_sm().text_color(p.red).child(error));
            let dut_id = self.side_panel.dut_id.clone().unwrap_or_default();
            body = body.child(
                btn(
                    "show-usb-binding",
                    crate::i18n::label("connectionTitle"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.show_dut_connections(&dut_id);
                    cx.notify();
                })),
            );
        }
        body.into_any_element()
    }

    fn shell_visible_rows(&self) -> &[duthub_core::ipc::ScreenRow] {
        let Some(screen) = self
            .side_panel
            .shell
            .as_ref()
            .and_then(|shell| shell.screen.as_ref())
        else {
            return &[];
        };
        let first = screen.rows.len().saturating_sub(400);
        &screen.rows[first..]
    }

    fn hit_test_shell_point(
        &self,
        position: Point<Pixels>,
        closest: bool,
        window: &Window,
    ) -> Option<crate::terminal_selection::TextPoint> {
        use crate::terminal_selection::{RowHit, TextPoint, hit_test_row};

        let rows = self.shell_visible_rows();
        if rows.is_empty() {
            return None;
        }
        let bounds = self.side_panel.shell_scroll.bounds();
        if bounds.size.height <= px(0.) {
            return None;
        }
        let offset = self.side_panel.shell_scroll.offset();
        let (font_family, font_size, _) = self.terminal_font(window);
        let row_height = font_size * 1.5;
        let padding = px(12.);
        let rel_y = f32::from(position.y - bounds.origin.y - padding) - f32::from(offset.y);
        match hit_test_row(rel_y, f32::from(row_height), rows.len())? {
            RowHit::Above => Some(TextPoint { row: 0, col: 0 }),
            RowHit::Below => {
                let last_row = rows.len() - 1;
                Some(TextPoint {
                    row: last_row,
                    col: rows[last_row].text.chars().count(),
                })
            }
            RowHit::Row(row) => {
                let rel_x = position.x - bounds.origin.x - padding - offset.x;
                let col = crate::workspace::terminal_col_for_x(
                    &rows[row].text,
                    rel_x,
                    &font_family,
                    font_size,
                    closest,
                    window,
                );
                Some(TextPoint { row, col })
            }
        }
    }

    fn begin_shell_selection(&mut self, ev: &MouseDownEvent, window: &Window) {
        use crate::terminal_selection::TerminalSelection;

        let closest = ev.click_count <= 1;
        let Some(point) = self.hit_test_shell_point(ev.position, closest, window) else {
            self.side_panel.shell_selection = None;
            return;
        };
        let row_text = self
            .shell_visible_rows()
            .get(point.row)
            .map(|row| row.text.clone())
            .unwrap_or_default();
        self.side_panel.shell_selection = Some(match ev.click_count {
            0 | 1 => TerminalSelection::start_char(point),
            2 => TerminalSelection::start_word(point.row, point.col, &row_text),
            _ => TerminalSelection::start_line(point.row, &row_text),
        });
    }

    pub(crate) fn update_shell_selection(
        &mut self,
        position: Point<Pixels>,
        left_pressed: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(granularity) = self
            .side_panel
            .shell_selection
            .filter(|selection| selection.dragging)
            .map(|selection| selection.granularity)
        else {
            return;
        };
        if !left_pressed {
            self.finish_shell_selection(cx);
            return;
        }
        let closest = granularity == crate::terminal_selection::SelectionGranularity::Char;
        let Some(point) = self.hit_test_shell_point(position, closest, window) else {
            return;
        };
        let row_text = self
            .shell_visible_rows()
            .get(point.row)
            .map(|row| row.text.clone())
            .unwrap_or_default();
        if let Some(selection) = self.side_panel.shell_selection.as_mut()
            && selection.update(point, &row_text)
        {
            cx.notify();
        }
    }

    pub(crate) fn finish_shell_selection(&mut self, cx: &mut Context<Self>) {
        let mut finished_non_empty = false;
        if let Some(selection) = self.side_panel.shell_selection.as_mut()
            && selection.dragging
        {
            selection.dragging = false;
            if selection.is_empty() {
                self.side_panel.shell_selection = None;
            } else {
                finished_non_empty = true;
            }
            if finished_non_empty && self.settings.serial.copy_on_select {
                self.copy_shell_selection(false, cx);
            }
            cx.notify();
        }
    }

    fn copy_shell_selection(&mut self, clear_after: bool, cx: &mut Context<Self>) -> bool {
        let Some(selection) = self
            .side_panel
            .shell_selection
            .filter(|selection| !selection.is_empty())
        else {
            return false;
        };
        let rows = self.shell_visible_rows();
        let Some(text) = selection.extract_text(rows.len(), |row| {
            rows.get(row)
                .map(|item| std::borrow::Cow::Borrowed(item.text.as_str()))
        }) else {
            return false;
        };
        crate::components::copy_text(cx, &text);
        if clear_after {
            self.side_panel.shell_selection = None;
        }
        true
    }

    fn paste_shell(&mut self, cx: &mut Context<Self>) {
        let Some(shell) = self.side_panel.shell.as_ref() else {
            return;
        };
        let valid = self
            .device
            .lease_manager()
            .zip(self.side_panel.dut_id.as_ref())
            .is_some_and(|(lease, dut)| lease.validate_ctx(dut, &shell.grant).is_ok());
        if !valid {
            self.side_panel.shell = None;
            self.side_panel.shell_selection = None;
            self.side_panel.error = Some(crate::i18n::label("usbControlReleased").into());
            cx.notify();
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed_paste = shell
            .screen
            .as_ref()
            .is_some_and(|screen| screen.bracketed_paste);
        let Some(bytes) = crate::workspace::prepare_paste_bytes(&text, bracketed_paste) else {
            return;
        };
        if let Some(tx) = &shell.tx
            && tx.try_send(bytes).is_err()
        {
            self.notify_toast(crate::i18n::t(
                self.lang,
                "shellInputIsTemporarilyUnavailable",
            ));
        }
    }

    fn render_shell(
        &mut self,
        dut: &DutConfig,
        width: Pixels,
        p: &'static Palette,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lang = self.lang;
        let needs_restart = self
            .side_panel
            .shell
            .as_ref()
            .is_none_or(|shell| shell.exited);
        let mut body = div().flex().flex_col().flex_1().min_h_0();
        let (font_family, font_size, cell_width) = self.terminal_font(window);
        let row_height = font_size * 1.5;
        let selection_bg = crate::workspace::terminal_selection_bg(p, self.theme_mode);
        let selection = self.side_panel.shell_selection;
        if let Some(shell) = &mut self.side_panel.shell {
            shell.update_size(width, window.viewport_size().height, cell_width, row_height);
            let focus = self.side_panel.focus.as_ref().expect("shell focus").clone();
            let focus_left = focus.clone();
            let focus_middle = focus.clone();
            let focus_right = focus.clone();
            let mut viewport = div()
                .id("side-shell-terminal")
                .flex_1()
                .min_h_0()
                .bg(p.term)
                .text_color(p.text)
                .font_family(font_family)
                .text_size(font_size)
                .line_height(row_height)
                .p(px(12.))
                .cursor_text()
                .overflow_y_scroll()
                .track_scroll(&self.side_panel.shell_scroll)
                .track_focus(&focus)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, win, cx| {
                        win.focus(&focus_left, cx);
                        this.begin_shell_selection(ev, win);
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _ev: &MouseDownEvent, win, cx| {
                        win.focus(&focus_middle, cx);
                        this.side_panel.shell_selection = None;
                        this.paste_shell(cx);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _ev: &MouseDownEvent, win, cx| {
                        win.focus(&focus_right, cx);
                        if this.settings.serial.paste_on_right_click {
                            this.side_panel.shell_selection = None;
                            this.paste_shell(cx);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }),
                )
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if this.terminal_zoom_key(event, cx) {
                        return;
                    }
                    let k = &event.keystroke;
                    let copy_shortcut = (k.modifiers.control && k.modifiers.shift && k.key == "c")
                        || (k.modifiers.platform
                            && !k.modifiers.control
                            && !k.modifiers.alt
                            && k.key == "c")
                        || (k.modifiers.control
                            && !k.modifiers.shift
                            && !k.modifiers.alt
                            && k.key == "insert");
                    let has_selection = this
                        .side_panel
                        .shell_selection
                        .is_some_and(|sel| !sel.is_empty());
                    let smart_ctrl_c = k.modifiers.control
                        && !k.modifiers.shift
                        && !k.modifiers.alt
                        && !k.modifiers.platform
                        && k.key == "c"
                        && has_selection;
                    if copy_shortcut || smart_ctrl_c {
                        if this.copy_shell_selection(smart_ctrl_c, cx) {
                            cx.notify();
                        }
                        cx.stop_propagation();
                        return;
                    }
                    let paste_shortcut = (k.modifiers.control && k.modifiers.shift && k.key == "v")
                        || (k.modifiers.platform
                            && !k.modifiers.control
                            && !k.modifiers.alt
                            && k.key == "v")
                        || (k.modifiers.shift
                            && !k.modifiers.control
                            && !k.modifiers.alt
                            && k.key == "insert");
                    if paste_shortcut {
                        this.side_panel.shell_selection = None;
                        this.paste_shell(cx);
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    let Some(shell) = this.side_panel.shell.as_ref() else {
                        return;
                    };
                    let valid = this
                        .device
                        .lease_manager()
                        .zip(this.side_panel.dut_id.as_ref())
                        .is_some_and(|(lease, dut)| lease.validate_ctx(dut, &shell.grant).is_ok());
                    if !valid {
                        this.side_panel.shell = None;
                        this.side_panel.shell_selection = None;
                        this.side_panel.error =
                            Some(crate::i18n::label("usbControlReleased").into());
                        cx.notify();
                        return;
                    }
                    if let (Some(tx), Some(bytes)) = (
                        &shell.tx,
                        key_bytes(event, shell.screen.as_ref(), || {
                            cx.read_from_clipboard().and_then(|item| item.text())
                        }),
                    ) {
                        this.side_panel.shell_selection = None;
                        if tx.try_send(bytes).is_err() {
                            this.notify_toast(crate::i18n::t(
                                this.lang,
                                "shellInputIsTemporarilyUnavailable",
                            ));
                        }
                        cx.stop_propagation();
                    }
                }));
            if let Some(screen) = &shell.screen {
                let first = screen.rows.len().saturating_sub(400);
                for (row_idx, row) in screen.rows[first..].iter().enumerate() {
                    let sel_range =
                        selection.and_then(|sel| sel.row_range(row_idx, row.text.chars().count()));
                    if sel_range.is_some() {
                        let runs =
                            crate::terminal_selection::split_plain_selection(&row.text, sel_range);
                        viewport = viewport.child(
                            div().flex().whitespace_nowrap().h(row_height).children(
                                runs.into_iter().map(|(text, selected)| {
                                    let mut cell = div().flex_shrink_0().whitespace_nowrap();
                                    if selected {
                                        cell = cell.bg(selection_bg);
                                    }
                                    cell.child(SharedString::from(text.to_owned()))
                                }),
                            ),
                        );
                    } else {
                        viewport = viewport.child(
                            div()
                                .whitespace_nowrap()
                                .h(row_height)
                                .child(row.text.clone()),
                        );
                    }
                }
            }
            body = body.child(viewport);
        }
        if self.side_panel.shell_connecting {
            body = body.child(
                div()
                    .p(px(12.))
                    .child(subtle(crate::i18n::label("detectingDevices"), p)),
            );
        }
        if needs_restart
            && !self.side_panel.shell_connecting
            && self
                .duts
                .saved_usb(&dut.id)
                .and_then(shell_options)
                .is_some()
        {
            let dut = dut.clone();
            body = body.child(
                btn(
                    "restart-shell",
                    crate::i18n::t(lang, "reconnect"),
                    &BtnVariant::Normal,
                    p,
                )
                .on_click(cx.listener(move |this, _, win, cx| {
                    this.side_panel.error = None;
                    this.start_shell(&dut, win, cx);
                    cx.notify();
                })),
            );
        }
        body.into_any_element()
    }

    fn render_adb_view_tabs(&self, p: &'static Palette, cx: &mut Context<Self>) -> Div {
        let preview = self.side_panel.adb_preview.is_some();
        div()
            .flex()
            .px(px(12.))
            .py(px(6.))
            .gap(px(6.))
            .child(
                btn(
                    "adb-shell-tab",
                    "Shell",
                    if preview {
                        &BtnVariant::Tiny
                    } else {
                        &BtnVariant::TinyPrimary
                    },
                    p,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.side_panel.adb_preview = None;
                    if let Some(frame) = this.side_panel.adb_frame.take() {
                        cx.drop_image(frame, Some(window));
                    }
                    this.side_panel.shell_focus_pending = true;
                    cx.notify();
                })),
            )
            .child(
                btn(
                    "adb-preview-tab",
                    crate::i18n::label("adbPreview"),
                    if preview {
                        &BtnVariant::TinyPrimary
                    } else {
                        &BtnVariant::Tiny
                    },
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if (this.side_panel.adb_preview.is_none()
                        || this.side_panel.adb_preview_error.is_some())
                        && let Some(target) = this.side_panel.android_preview.clone()
                    {
                        this.side_panel.adb_preview_error = None;
                        this.side_panel.adb_preview = Some(AdbPreview::start(target));
                    }
                    cx.notify();
                })),
            )
    }

    fn render_adb_preview(
        &mut self,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let update = self
            .side_panel
            .adb_preview
            .as_ref()
            .and_then(|preview| preview.latest.lock().unwrap().take());
        match update {
            Some(Ok(frame)) => {
                if let Some(previous) = self.side_panel.adb_frame.replace(frame) {
                    cx.drop_image(previous, Some(window));
                }
            }
            Some(Err(error)) => {
                self.side_panel.adb_preview_error = Some(crate::i18n::message(
                    "adbPreviewFailed",
                    &[("error", &error)],
                ));
                if let Some(frame) = self.side_panel.adb_frame.take() {
                    cx.drop_image(frame, Some(window));
                }
            }
            None => {}
        }
        let mut viewport = div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .bg(p.term);
        if let Some(frame) = &self.side_panel.adb_frame {
            viewport = viewport.child(
                img(frame.clone())
                    .absolute()
                    .inset_0()
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            );
        } else {
            viewport = viewport.child(subtle(
                self.side_panel
                    .adb_preview_error
                    .as_deref()
                    .unwrap_or_else(|| crate::i18n::label("waitingForVideo")),
                p,
            ));
        }
        viewport
    }

    fn render_monitor(
        &mut self,
        dut: &DutConfig,
        p: &'static Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lang = self.lang;
        let floating =
            self.frame_pump.placement() == duthub_desktop::capture_ui::MonitorPlacement::Floating;
        let mut source_tabs = div().flex().flex_wrap().items_center().gap(px(6.));
        for source in self
            .duts
            .saved_media(&dut.id)
            .into_iter()
            .flat_map(bound_sources)
            .filter(|(source, _)| {
                crate::media_page::saved_capture(&self.duts, &dut.id, *source).is_ok()
            })
        {
            let (source, input) = source;
            let current = self.side_panel.source == Some(source);
            source_tabs = source_tabs.child(
                btn(
                    source.key().into_owned(),
                    if input.name.trim().is_empty() {
                        source.label()
                    } else {
                        input.name.trim()
                    },
                    if current {
                        &BtnVariant::TinyPrimary
                    } else {
                        &BtnVariant::Tiny
                    },
                    p,
                )
                .on_click(cx.listener(move |this, _, win, cx| {
                    if let Some((id, selected, input)) = this.monitor_target(Some(source))
                        && selected == source
                    {
                        this.start_monitor(id.to_owned(), selected, input.clone(), win, cx);
                        cx.notify();
                    }
                })),
            );
        }
        let tabs = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap(px(6.))
            .child(source_tabs);
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(px(10.))
            .p(px(12.))
            .child(tabs);
        let frame = self.frame_pump.frame();
        let error = self.frame_pump.error().map(str::to_owned);
        let has_preview = self.frame_pump.key().is_some();
        let mut viewport = div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .rounded(px(8.))
            .bg(p.term);
        if floating {
            // Floating: the docked pane is a placeholder; the float window has
            // the only picture (§6.2/§6.3, no double upload).
            viewport = viewport.child(subtle(crate::i18n::label("monitorFloating"), p));
        } else if let Some(frame) = frame {
            viewport = viewport.child(
                img(frame)
                    .absolute()
                    .inset(px(0.))
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            );
        } else if has_preview {
            viewport = viewport.child(subtle(crate::i18n::t(lang, "waitingForVideo"), p));
        } else if let Some(error) = error {
            viewport = viewport.child(
                div()
                    .w_full()
                    .min_w_0()
                    .p(px(12.))
                    .text_sm()
                    .text_color(p.red)
                    .whitespace_normal()
                    .overflow_hidden()
                    .child(error),
            );
        }
        body.child(viewport).into_any_element()
    }

    // ------------------------------------------------------------------
    // HDMI float window (§6.4)
    // ------------------------------------------------------------------

    /// Toolbar "还原": close the float window, hand the preview back to the
    /// monitor pane and activate the main window. Called from the float view
    /// before it removes its own window, so the close observer treats it as
    /// programmatic.
    pub(crate) fn prepare_redock(&mut self, cx: &mut Context<Self>) {
        let (placement, action) = duthub_desktop::capture_ui::next(
            self.frame_pump.placement(),
            duthub_desktop::capture_ui::MonitorEvent::Redock,
        );
        self.frame_pump.set_placement(placement);
        if action.contains(duthub_desktop::capture_ui::MonitorAction::CloseFloatWindow) {
            self.frame_pump.set_float(None);
            self.float_window = None;
        }
        if action.contains(duthub_desktop::capture_ui::MonitorAction::FocusMainWindow) {
            self.focus_main_window(cx);
        }
        if let Some((dut_id, source)) = self.frame_pump.key().cloned() {
            if let Some(frame) = self.side_panel.adb_frame.take() {
                cx.drop_image(frame, None);
            }
            self.side_panel = SidePanel::default();
            self.side_panel.monitor_input =
                crate::media_page::saved_capture(&self.duts, &dut_id, source)
                    .ok()
                    .cloned();
            self.side_panel.visible = Some(Panel::Monitor);
            self.side_panel.dut_id = Some(dut_id);
            self.side_panel.source = Some(source);
            if self.sessions.get(self.active_session).is_some() {
                self.view = ViewMode::Serial;
            }
        }
        cx.notify();
    }
}

fn bound_sources(media: &DutMediaConfig) -> impl Iterator<Item = (Source, &CaptureInput)> {
    Source::sources(media)
        .map(|source| (source, source.input(media)))
        .filter(|(_, input)| input.has_video())
}

fn monitor_source(
    media: &DutMediaConfig,
    preferred: Option<Source>,
) -> Option<(Source, &CaptureInput)> {
    bound_sources(media)
        .find(|(source, _)| Some(*source) == preferred)
        .or_else(|| bound_sources(media).next())
}

fn key_bytes(
    event: &KeyDownEvent,
    screen: Option<&ScreenSnapshot>,
    clipboard: impl FnOnce() -> Option<String>,
) -> Option<Vec<u8>> {
    let k = &event.keystroke;
    if k.modifiers.alt {
        return None;
    }
    if (k.modifiers.control && k.modifiers.shift || k.modifiers.platform) && k.key == "v" {
        let text = clipboard()?;
        return Some(if screen.is_some_and(|screen| screen.bracketed_paste) {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        });
    }
    if k.modifiers.platform {
        return None;
    }
    if k.modifiers.control {
        return match k.key.as_bytes() {
            [key @ b'a'..=b'z'] => Some(vec![key - b'a' + 1]),
            _ => None,
        };
    }
    let bytes: &[u8] = match k.key.as_str() {
        "enter" => b"\r",
        "backspace" => b"\x7f",
        "tab" => b"\t",
        "escape" => b"\x1b",
        "delete" => b"\x1b[3~",
        "up" | "down" | "right" | "left" | "home" | "end" => {
            let mode = if screen.is_some_and(|screen| screen.application_cursor) {
                b'O'
            } else {
                b'['
            };
            let code = match k.key.as_str() {
                "up" => b'A',
                "down" => b'B',
                "right" => b'C',
                "left" => b'D',
                "home" => b'H',
                _ => b'F',
            };
            return Some(vec![0x1b, mode, code]);
        }
        _ => return crate::components::typed_char(event).map(String::into_bytes),
    };
    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_uses_the_active_duts_saved_video_binding() {
        let mut file: duthub_core::DutsFile =
            serde_json::from_str(include_str!("../../../docs/examples/duts.json")).unwrap();
        let mut other = file.duts[0].clone();
        other.id = "camera-dut".into();
        other.media.camera.video = "camera input".into();
        file.duts[0].media = DutMediaConfig::default();
        file.duts[0].media.camera.audio = "audio only".into();
        let first_id = file.duts[0].id.clone();
        file.duts.push(other);
        let mut duts = crate::data::DutsState::from_file(file);
        assert!(monitor_source(duts.saved_media(&first_id).unwrap(), None).is_none());
        assert_eq!(
            monitor_source(duts.saved_media("camera-dut").unwrap(), Some(Source::Hdmi))
                .map(|(source, input)| (source, input.video.as_str())),
            Some((Source::Camera, "camera input"))
        );
        duts.file.duts[0].media.hdmi.video = "HDMI input".into();
        assert!(monitor_source(duts.saved_media(&first_id).unwrap(), None).is_none());
        duts.sync_saved_media();
        assert_eq!(
            monitor_source(duts.saved_media(&first_id).unwrap(), None)
                .map(|(source, input)| (source, input.video.as_str())),
            Some((Source::Hdmi, "HDMI input"))
        );
    }

    #[test]
    #[ignore = "uses the echo executable explicitly named in DUTHUB_TEST_SHELL_BIN"]
    fn shell_uses_the_bound_device_and_sends_terminal_input() {
        let mut dut: DutConfig = serde_json::from_value(serde_json::json!({
            "id": "fixture", "name": "fixture", "serial": {"host": "local", "port": "fixture"},
            "adb": {"mode": "usb", "serial": "fixture-target"}
        }))
        .unwrap();
        let exe = std::env::var("DUTHUB_TEST_SHELL_BIN").unwrap();
        let tools = ToolsSettings {
            adb_executable: exe.clone(),
            hdc_executable: exe,
            ..ToolsSettings::default()
        };
        for (tool, selector) in [("adb", "-s"), ("hdc", "-t")] {
            dut.adb.tool = tool.into();
            assert_eq!(
                shell_options(&dut.adb),
                Some((tool, selector, "fixture-target"))
            );
            let lease = Arc::new(ControlLeaseManager::new(None));
            lease
                .acquire(&dut.id, &duthub_core::lease::LeaseHolder::Gui, None)
                .unwrap();
            let grant = lease
                .admit_write(&dut.id, &WriteIdentity::gui())
                .unwrap()
                .unwrap();
            let access = ShellAccess {
                lease,
                dut_id: dut.id.clone(),
                grant,
            };
            let (tool, selector, serial) = shell_options(&dut.adb).unwrap();
            let mut shell =
                Shell::start(usb_executable(&tools, tool), selector, serial, access).unwrap();
            shell
                .tx
                .as_ref()
                .unwrap()
                .send(b"probe\r".to_vec())
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                shell.tick();
                if let Some(screen) = &shell.screen {
                    let text = screen
                        .rows
                        .iter()
                        .map(|row| row.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.contains("fixture received: probe") {
                        assert!(text.contains("fixture-target"), "{text}");
                        assert!(text.contains(selector), "{text}");
                        break;
                    }
                }
                assert!(Instant::now() < deadline, "{tool} did not echo input");
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}
