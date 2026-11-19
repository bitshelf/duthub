//! Tools attached to the active serial session.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use duthub_adb::{AdbClient, AndroidPreview, TargetUnavailable, choose_target};
use duthub_core::ipc::ScreenSnapshot;
use duthub_core::lease::{LeaseAuthority, WriteCtx, WriteIdentity};
use duthub_core::model::{AdbConfig, CaptureInput, DutConfig, DutMediaConfig, ToolsSettings};
use duthub_core::process::resolve_executable;
use duthub_device::term::{TermGrid, create_term, snapshot_screen};
use gpui_kit::base::Selectable;
use gpui_kit::{
    AnyElement, Context, Div, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ObjectFit, ParentElement, Pixels, RenderImage, StatefulInteractiveElement, Styled, StyledImage,
    Window, div, img, px,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::components::{BtnVariant, btn, icon_btn, panel_head, subtle};
use crate::media::{InlinePreview, Source};
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
    source: Option<Source>,
    monitor_input: Option<CaptureInput>,
    preview: Option<InlinePreview>,
    owns_capture: bool,
    frame: Option<Arc<RenderImage>>,
    error: Option<String>,
}

struct Shell {
    pty: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
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

impl Shell {
    fn start(spec: &str, selector: &str, serial: &str, grant: WriteCtx) -> Result<Self, String> {
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
        std::thread::spawn(move || {
            while let Ok(bytes) = input.recv() {
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
            child,
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
        if !self.exited && self.child.try_wait().ok().flatten().is_some() {
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
        self.tx.take();
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Workbench {
    fn panel_dut(&self) -> Option<&DutConfig> {
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
            | self
                .side_panel
                .preview
                .as_mut()
                .is_some_and(InlinePreview::has_update)
    }

    fn start_shell(&mut self, dut: &DutConfig, window: &mut Window, cx: &mut Context<Self>) {
        let binding = self.duts.saved_usb(&dut.id).cloned();
        self.side_panel.shell_binding = binding.clone();
        self.side_panel.shell = None;
        self.side_panel.android_preview = None;
        self.side_panel.adb_preview = None;
        self.side_panel.resolved_adb = None;
        if let Some(frame) = self.side_panel.frame.take() {
            let _ = window.drop_image(frame);
        }
        let Some(binding) = binding else {
            return;
        };
        let Some((tool, selector, serial)) = shell_options(&binding) else {
            return;
        };
        let spec = usb_executable(&self.settings.tools, tool).to_owned();
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
            Shell::start(spec, selector, serial, grant)
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
    ) {
        self.close_side_panel(window);
        self.side_panel.visible = Some(Panel::Monitor);
        self.side_panel.dut_id = Some(dut_id.clone());
        self.side_panel.source = Some(source);
        let result =
            self.media
                .open_inline(&self.duts, &dut_id, source, &self.settings.tools.capture);
        self.side_panel.monitor_input = Some(input);
        match result {
            Ok((preview, owns_capture)) => {
                self.side_panel.preview = Some(preview);
                self.side_panel.owns_capture = owns_capture;
            }
            Err(err) => self.side_panel.error = Some(err),
        }
    }

    fn monitor_target(&self, preferred: Option<Source>) -> Option<(&str, Source, &CaptureInput)> {
        if self.view != ViewMode::Serial {
            return None;
        }
        let dut = self.panel_dut()?;
        let first = monitor_source(self.duts.saved_media(&dut.id)?, preferred)?.0;
        std::iter::once(first)
            .chain(Source::ALL.into_iter().filter(|source| *source != first))
            .find_map(|source| {
                let input = crate::media_page::saved_capture(&self.duts, &dut.id, source).ok()?;
                Some((dut.id.as_str(), source, input))
            })
    }

    pub(crate) fn sync_monitor_panel(&mut self, window: &mut Window) {
        if self.side_panel.visible != Some(Panel::Monitor) {
            return;
        }
        let preferred = self
            .panel_dut()
            .filter(|dut| Some(dut.id.as_str()) == self.side_panel.dut_id.as_deref())
            .and(self.side_panel.source);
        let Some((id, source, input)) = self.monitor_target(preferred) else {
            self.close_side_panel(window);
            return;
        };
        if self.side_panel.dut_id.as_deref() != Some(id)
            || self.side_panel.source != Some(source)
            || self.side_panel.monitor_input.as_ref() != Some(input)
        {
            self.start_monitor(id.to_owned(), source, input.clone(), window);
        }
    }

    fn stop_preview(&mut self) {
        self.side_panel.preview = None;
        if std::mem::take(&mut self.side_panel.owns_capture)
            && let (Some(id), Some(source)) = (&self.side_panel.dut_id, self.side_panel.source)
        {
            self.media.close_inline_capture(id, source);
        }
    }

    pub fn open_side_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        if panel == Panel::Monitor {
            if let Some((id, source, input)) = self.monitor_target(None) {
                self.start_monitor(id.to_owned(), source, input.clone(), window);
                cx.notify();
            }
            return;
        }
        self.close_side_panel(window);
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

    pub fn close_side_panel(&mut self, window: &mut Window) {
        self.stop_preview();
        if let Some(frame) = self.side_panel.frame.take() {
            let _ = window.drop_image(frame);
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
                            this.close_side_panel(window);
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
        let Some(panel) = self
            .side_panel
            .visible
            .filter(|panel| *panel != Panel::Agent)
        else {
            return crate::agent::render_agent_pane(self, p, window, cx).into_any_element();
        };
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
                .child(
                    icon_btn("close-side-tool", "x", p)
                        .tooltip(crate::i18n::t(lang, "close"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_side_panel(window);
                            cx.notify();
                        })),
                ),
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
                        body.child(self.render_adb_preview(p, window))
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
        if let Some(shell) = &mut self.side_panel.shell {
            shell.update_size(width, window.viewport_size().height, cell_width, row_height);
            let focus = self.side_panel.focus.as_ref().expect("shell focus").clone();
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
                .overflow_y_scroll()
                .track_focus(&focus)
                .on_click(cx.listener(move |_, _, win, cx| win.focus(&focus, cx)))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if this.terminal_zoom_key(event, cx) {
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
                for row in &screen.rows[first..] {
                    viewport = viewport.child(
                        div()
                            .whitespace_nowrap()
                            .h(row_height)
                            .child(row.text.clone()),
                    );
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
                    if let Some(frame) = this.side_panel.frame.take() {
                        let _ = window.drop_image(frame);
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

    fn render_adb_preview(&mut self, p: &'static Palette, window: &mut Window) -> Div {
        let update = self
            .side_panel
            .adb_preview
            .as_ref()
            .and_then(|preview| preview.latest.lock().unwrap().take());
        match update {
            Some(Ok(frame)) => {
                if let Some(previous) = self.side_panel.frame.replace(frame) {
                    let _ = window.drop_image(previous);
                }
            }
            Some(Err(error)) => {
                self.side_panel.adb_preview_error = Some(crate::i18n::message(
                    "adbPreviewFailed",
                    &[("error", &error)],
                ));
                if let Some(frame) = self.side_panel.frame.take() {
                    let _ = window.drop_image(frame);
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
        if let Some(frame) = &self.side_panel.frame {
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lang = self.lang;
        let mut tabs = div().flex().gap(px(6.));
        for source in self
            .duts
            .saved_media(&dut.id)
            .into_iter()
            .flat_map(bound_sources)
            .map(|(source, _)| source)
            .filter(|source| crate::media_page::saved_capture(&self.duts, &dut.id, *source).is_ok())
        {
            let current = self.side_panel.source == Some(source);
            tabs = tabs.child(
                btn(
                    source.key(),
                    source.label(),
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
                        this.start_monitor(id.to_owned(), selected, input.clone(), win);
                        cx.notify();
                    }
                })),
            );
        }
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(px(10.))
            .p(px(12.))
            .child(tabs);
        if let Some(preview) = self.side_panel.preview.as_mut() {
            let (next, error) = preview.poll();
            if let Some(frame) = next
                && let Some(old) = self.side_panel.frame.replace(frame)
            {
                let _ = window.drop_image(old);
            }
            if let Some(error) = error {
                let detail = self
                    .media
                    .capture_error(&dut.id, self.side_panel.source)
                    .unwrap_or(error);
                self.side_panel.error = Some(detail);
                self.stop_preview();
                if let Some(frame) = self.side_panel.frame.take() {
                    let _ = window.drop_image(frame);
                }
            }
        }
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
        if let Some(frame) = &self.side_panel.frame {
            viewport = viewport.child(
                img(frame.clone())
                    .absolute()
                    .inset_0()
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            );
        } else if self.side_panel.preview.is_some() {
            viewport = viewport.child(subtle(crate::i18n::t(lang, "waitingForVideo"), p));
        } else if let Some(error) = self.side_panel.error.clone() {
            viewport = viewport.child(div().p(px(12.)).text_sm().text_color(p.red).child(error));
        }
        body.child(viewport).into_any_element()
    }
}

fn bound_sources(media: &DutMediaConfig) -> impl Iterator<Item = (Source, &CaptureInput)> {
    [(Source::Hdmi, &media.hdmi), (Source::Camera, &media.camera)]
        .into_iter()
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
            let grant = WriteCtx {
                lease_id: "fixture".into(),
                generation: 0,
            };
            let (tool, selector, serial) = shell_options(&dut.adb).unwrap();
            let mut shell =
                Shell::start(usb_executable(&tools, tool), selector, serial, grant).unwrap();
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
