use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use duthub_core::model::{CaptureInput, CaptureSettings, DutConfig};
use gpui_kit::component::WindowExt;
use gpui_kit::{
    Context, Div, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::components::{BtnVariant, btn, copy_text, field, panel_head, subtle, toggle};
use crate::data::DutsState;
use crate::media::{self, Capture, MediaUrls, Source};
use crate::theme::Palette;
use crate::workspace::{Dd, In, Workbench};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaField {
    Ffmpeg,
    Ffplay,
    Listen,
    Video(Source),
    Audio(Source),
    Size(Source),
    Rate(Source),
    Port(Source),
}

impl MediaField {
    pub fn key(self) -> &'static str {
        match self {
            Self::Ffmpeg => "ffmpeg-exe",
            Self::Ffplay => "ffplay-exe",
            Self::Listen => "media-listen-ip",
            Self::Video(Source::Hdmi) => "hdmi-video",
            Self::Audio(Source::Hdmi) => "hdmi-audio",
            Self::Size(Source::Hdmi) => "hdmi-size",
            Self::Rate(Source::Hdmi) => "hdmi-rate",
            Self::Port(Source::Hdmi) => "hdmi-port",
            Self::Video(Source::Camera) => "camera-video",
            Self::Audio(Source::Camera) => "camera-audio",
            Self::Size(Source::Camera) => "camera-size",
            Self::Rate(Source::Camera) => "camera-rate",
            Self::Port(Source::Camera) => "camera-port",
        }
    }

    pub fn is_dut(self) -> bool {
        !matches!(self, Self::Ffmpeg | Self::Ffplay | Self::Listen)
    }

    pub fn value<'a>(self, settings: &'a CaptureSettings, dut: Option<&'a DutConfig>) -> &'a str {
        match self {
            Self::Ffmpeg => &settings.ffmpeg,
            Self::Ffplay => &settings.ffplay,
            Self::Listen => &settings.listen_ip,
            Self::Video(source) => dut
                .map(|dut| binding(dut, source).video.as_str())
                .unwrap_or_default(),
            Self::Audio(source) => dut
                .map(|dut| binding(dut, source).audio.as_str())
                .unwrap_or_default(),
            Self::Size(source) => dut
                .map(|dut| binding(dut, source).video_size.as_str())
                .unwrap_or_default(),
            Self::Rate(source) => dut
                .map(|dut| binding(dut, source).frame_rate.as_str())
                .unwrap_or_default(),
            Self::Port(source) => dut
                .map(|dut| binding(dut, source).port.as_str())
                .unwrap_or_default(),
        }
    }

    pub fn edit<'a>(
        self,
        settings: &'a mut CaptureSettings,
        dut: Option<&'a mut DutConfig>,
    ) -> Option<&'a mut String> {
        Some(match self {
            Self::Ffmpeg => &mut settings.ffmpeg,
            Self::Ffplay => &mut settings.ffplay,
            Self::Listen => &mut settings.listen_ip,
            Self::Video(source) => &mut binding_mut(dut?, source).video,
            Self::Audio(source) => &mut binding_mut(dut?, source).audio,
            Self::Size(source) => &mut binding_mut(dut?, source).video_size,
            Self::Rate(source) => &mut binding_mut(dut?, source).frame_rate,
            Self::Port(source) => &mut binding_mut(dut?, source).port,
        })
    }
}

fn binding(dut: &DutConfig, source: Source) -> &CaptureInput {
    source.input(&dut.media)
}

fn binding_mut(dut: &mut DutConfig, source: Source) -> &mut CaptureInput {
    match source {
        Source::Hdmi => &mut dut.media.hdmi,
        Source::Camera => &mut dut.media.camera,
    }
}

pub struct MediaView {
    pub urls: MediaUrls,
    dir: PathBuf,
    sessions: BTreeMap<(String, Source), Capture>,
    statuses: BTreeMap<(String, Source), String>,
    output: String,
    sender: mpsc::Sender<String>,
    messages: mpsc::Receiver<String>,
    devices_tx: mpsc::Sender<Result<String, String>>,
    devices_rx: mpsc::Receiver<Result<String, String>>,
    devices: Option<Result<String, String>>,
    scanning: bool,
    mode_probe: Option<ModeProbe>,
    mode_result: Option<ModeResult>,
}

#[derive(Clone, PartialEq, Eq)]
struct ModeTarget {
    dut_id: String,
    source: Source,
    video: String,
    ffmpeg: String,
}

struct ModeProbe {
    target: ModeTarget,
    response: mpsc::Receiver<Result<media::VideoModes, String>>,
}

struct ModeResult {
    target: ModeTarget,
    completed: Instant,
    modes: Result<media::VideoModes, String>,
}

fn video_identity(video: &str) -> String {
    let video = video.trim();
    if cfg!(windows) {
        return video.to_ascii_lowercase();
    }
    if video.starts_with("/dev/")
        && let Ok(path) = std::fs::canonicalize(video)
    {
        return path.to_string_lossy().into_owned();
    }
    video.to_owned()
}

fn duplicate_video(video: &str) -> String {
    crate::i18n::message("captureDuplicateVideo", &[("device", video.trim())])
}

pub(crate) fn validate_bindings(duts: &[DutConfig]) -> Result<(), String> {
    let mut videos = std::collections::HashSet::new();
    for dut in duts {
        for source in Source::ALL {
            let input = binding(dut, source);
            if !input.is_enabled() {
                continue;
            }
            if !input.has_video() {
                return Err(crate::i18n::message(
                    "captureDutNeedsVideo",
                    &[("dut", &dut.name), ("source", source.label())],
                ));
            }
            if !videos.insert(video_identity(&input.video)) {
                return Err(duplicate_video(&input.video));
            }
        }
    }
    Ok(())
}

pub(crate) fn saved_capture<'a>(
    duts: &'a DutsState,
    dut_id: &str,
    source: Source,
) -> Result<&'a CaptureInput, String> {
    let input = duts
        .saved_media(dut_id)
        .map(|media| source.input(media))
        .filter(|input| input.has_video())
        .ok_or_else(|| crate::i18n::label("captureNeedsVideo").to_owned())?;
    let video = video_identity(&input.video);
    for (other_id, other_media) in duts.saved_media_bindings() {
        for other_source in Source::ALL {
            let other = other_source.input(other_media);
            if (other_id, other_source) != (dut_id, source)
                && other.has_video()
                && video_identity(&other.video) == video
            {
                return Err(duplicate_video(&input.video));
            }
        }
    }
    Ok(input)
}

impl MediaView {
    pub fn retry_mode_probe(&mut self) {
        if self.mode_result.as_ref().is_some_and(|result| {
            result.modes.is_err() || result.completed.elapsed() > Duration::from_secs(15)
        }) {
            self.mode_result = None;
        }
    }

    fn probe_modes(
        &mut self,
        target: ModeTarget,
        settings: &CaptureSettings,
    ) -> Option<Result<&media::VideoModes, &str>> {
        if self
            .mode_result
            .as_ref()
            .is_some_and(|done| done.target == target)
        {
            return self
                .mode_result
                .as_ref()
                .map(|result| result.modes.as_ref().map_err(String::as_str));
        }
        if self.mode_probe.is_none() {
            let (send, response) = mpsc::channel();
            let video = target.video.clone();
            let settings = settings.clone();
            self.mode_probe = Some(ModeProbe { target, response });
            std::thread::spawn(move || {
                let result = (|| {
                    if cfg!(windows) {
                        let devices = directshow_video_devices(&media::list_devices(&settings)?);
                        let endpoint = video_endpoint(&devices, &video)?;
                        media::video_modes(&settings, endpoint)
                    } else {
                        media::video_modes(&settings, &video)
                    }
                })();
                let _ = send.send(result);
            });
        }
        None
    }

    fn start_bound_capture(
        &self,
        duts: &DutsState,
        key: &(String, Source),
        input: &CaptureInput,
        settings: &CaptureSettings,
    ) -> Result<Capture, String> {
        if !cfg!(windows) {
            return Capture::start(settings, input);
        }
        // A friendly DirectShow name can identify several capture cards. Resolve
        // it against the current probe and always open the exact endpoint.
        let devices = directshow_video_devices(&media::list_devices(settings)?);
        let endpoint = video_endpoint(&devices, &input.video)?;
        for (dut_id, saved) in duts.saved_media_bindings() {
            for source in Source::ALL {
                let other = source.input(saved);
                if (dut_id, source) != (key.0.as_str(), key.1)
                    && other.has_video()
                    && video_endpoint(&devices, &other.video)
                        .is_ok_and(|id| id.eq_ignore_ascii_case(endpoint))
                {
                    return Err(duplicate_video(&input.video));
                }
            }
        }
        let mut resolved = input.clone();
        resolved.video = endpoint.to_owned();
        let mut capture = Capture::start(settings, &resolved)?;
        capture.input = input.clone();
        Ok(capture)
    }

    pub fn capture_error(&self, dut_id: &str, source: Option<Source>) -> Option<String> {
        let capture = self.sessions.get(&(dut_id.to_owned(), source?))?;
        capture.has_ended().then(|| capture.status())
    }
    pub fn open_inline(
        &mut self,
        duts: &DutsState,
        dut_id: &str,
        source: Source,
        settings: &CaptureSettings,
    ) -> Result<(media::InlinePreview, bool), String> {
        self.reconcile_bindings(duts);
        let input = saved_capture(duts, dut_id, source)?;
        let key = (dut_id.to_owned(), source);
        if let Some(capture) = self
            .sessions
            .get(&key)
            .filter(|capture| !capture.has_ended())
        {
            return capture.inline_preview().map(|preview| (preview, false));
        }
        self.stop_capture(&key);
        let capture = self.start_bound_capture(duts, &key, input, settings)?;
        let preview = capture.inline_preview()?;
        self.sessions.insert(key, capture);
        Ok((preview, true))
    }

    pub fn close_inline_capture(&mut self, dut_id: &str, source: Source) {
        let key = (dut_id.to_owned(), source);
        if self
            .sessions
            .get(&key)
            .is_some_and(|capture| !capture.is_recording())
        {
            self.stop_capture(&key);
        }
    }

    pub fn new(dir: PathBuf) -> Self {
        let (sender, messages) = mpsc::channel();
        let (devices_tx, devices_rx) = mpsc::channel();
        Self {
            urls: Default::default(),
            dir,
            sessions: BTreeMap::new(),
            statuses: BTreeMap::new(),
            output: String::new(),
            sender,
            messages,
            devices_tx,
            devices_rx,
            devices: None,
            scanning: false,
            mode_probe: None,
            mode_result: None,
        }
    }

    fn stop_capture(&mut self, key: &(String, Source)) {
        {
            let mut urls = self.urls.lock().unwrap();
            if let Some(sources) = urls.get_mut(&key.0) {
                sources.remove(key.1.key());
                if sources.is_empty() {
                    urls.remove(&key.0);
                }
            }
        }
        self.sessions.remove(key);
    }

    pub fn reconcile_bindings(&mut self, duts: &DutsState) {
        let stale: Vec<_> = self
            .sessions
            .iter()
            .filter(|(key, capture)| {
                saved_capture(duts, &key.0, key.1).ok() != Some(&capture.input)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.stop_capture(&key);
        }
    }

    pub fn tick(&mut self, duts: &DutsState) -> bool {
        self.reconcile_bindings(duts);
        let statuses: BTreeMap<_, _> = self
            .sessions
            .iter()
            .map(|(key, capture)| (key.clone(), capture.status()))
            .collect();
        let mut changed = statuses != self.statuses;
        if changed {
            self.statuses = statuses;
            let mut urls = self.urls.lock().unwrap();
            urls.clear();
            for ((dut, source), capture) in &self.sessions {
                if capture.ready() {
                    urls.entry(dut.clone())
                        .or_default()
                        .insert(source.key().into(), capture.url.clone());
                }
            }
        }
        for session in self.sessions.values_mut() {
            if let Some(result) = session.poll_recording() {
                self.output = match result {
                    Ok(path) => format!("录屏已保存到 {}", path.display()),
                    Err(err) => err,
                };
                changed = true;
            }
        }
        for message in self.messages.try_iter() {
            self.output = message;
            changed = true;
        }
        if let Ok(result) = self.devices_rx.try_recv() {
            self.scanning = false;
            self.devices = Some(result);
            changed = true;
        }
        if let Some(probe) = &self.mode_probe {
            let result = match probe.response.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(String::new())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.mode_result = Some(ModeResult {
                    target: probe.target.clone(),
                    completed: Instant::now(),
                    modes: result,
                });
                self.mode_probe = None;
                changed = true;
            }
        }
        changed
    }

    fn scan_devices(&mut self, settings: CaptureSettings) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        let send = self.devices_tx.clone();
        std::thread::spawn(move || {
            let _ = send.send(media::list_devices(&settings));
        });
    }
}

pub fn render(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let settings_panel = div()
        .border_1()
        .border_color(p.line)
        .rounded(px(8.))
        .child(panel_head("eye", crate::i18n::label("captureTools"), p))
        .child(
            div()
                .p(px(16.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(field(
                    crate::i18n::label("ffmpegPath"),
                    w.path_input(
                        In::Media(MediaField::Ffmpeg),
                        "ffmpeg / %FFMPEG_HOME%/bin/ffmpeg.exe",
                        p,
                        window,
                        cx,
                    ),
                    None,
                    p,
                ))
                .child(field(
                    crate::i18n::label("ffplayPath"),
                    w.path_input(
                        In::Media(MediaField::Ffplay),
                        crate::i18n::label("ffplayPathHint"),
                        p,
                        window,
                        cx,
                    ),
                    None,
                    p,
                ))
                .child(text(
                    w,
                    MediaField::Listen,
                    crate::i18n::label("listenIp"),
                    crate::i18n::label("listenIpHint"),
                    p,
                    window,
                    cx,
                ))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            btn(
                                "save-media",
                                crate::i18n::label("saveCaptureSettings"),
                                &BtnVariant::TinyPrimary,
                                p,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.persist_settings();
                                cx.notify();
                            })),
                        )
                        .child(
                            btn(
                                "list-media",
                                crate::i18n::label("listCaptureDevices"),
                                &BtnVariant::Tiny,
                                p,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.media.scan_devices(this.settings.tools.capture.clone());
                                    cx.notify();
                                },
                            )),
                        ),
                ),
        );
    let mut page = div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .max_w(px(980.))
        .w_full()
        .mx_auto()
        .p(px(24.))
        .child(settings_panel);
    page = page.child(
        div()
            .text_sm()
            .text_color(p.muted)
            .child(crate::i18n::label("captureDevicesForDut")),
    );
    if w.media.scanning {
        page = page.child(crate::i18n::label("detectingDevices"));
    } else if let Some(devices) = &w.media.devices {
        match devices {
            Ok(raw) => {
                for (audio, key) in [(false, "videoDevice"), (true, "audioDevice")] {
                    let names = detected_devices(raw, audio);
                    page = page.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(5.))
                            .p(px(12.))
                            .bg(p.surface)
                            .text_sm()
                            .text_color(p.text)
                            .child(crate::i18n::label(key))
                            .child(div().text_color(p.muted).child(if names.is_empty() {
                                crate::i18n::label("noCaptureDevices").into()
                            } else {
                                names.join("\n")
                            })),
                    );
                }
            }
            Err(error) => page = page.child(div().text_color(p.red).child(error.clone())),
        }
    }
    if !w.media.output.is_empty() {
        page = page.child(
            div()
                .p(px(12.))
                .bg(p.surface)
                .text_size(px(12.))
                .text_color(p.text)
                .whitespace_normal()
                .child(SharedString::from(w.media.output.clone())),
        );
    }
    page
}

pub(crate) fn render_bindings(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let mut panel = div().flex().flex_col().gap(px(12.));
    if let Some(dut) = w.duts.selected().cloned() {
        panel = panel.child(source_panel(w, &dut, Source::Hdmi, p, window, cx));
        panel = panel.child(source_panel(w, &dut, Source::Camera, p, window, cx));
    }
    if !w.media.output.is_empty() {
        panel = panel.child(
            div()
                .text_size(px(12.))
                .text_color(p.muted)
                .child(w.media.output.clone()),
        );
    }
    panel
}

#[derive(Debug)]
struct VideoDevice {
    name: String,
    endpoint: Option<String>,
}

fn directshow_video_devices(output: &str) -> Vec<VideoDevice> {
    let mut devices: Vec<VideoDevice> = Vec::new();
    let mut video_pending = false;
    for line in output.lines() {
        if line.trim_end().ends_with("(video)") {
            if let Some((name, _)) = line
                .split_once('"')
                .and_then(|(_, tail)| tail.split_once('"'))
            {
                devices.push(VideoDevice {
                    name: name.into(),
                    endpoint: None,
                });
                video_pending = true;
            }
        } else if std::mem::take(&mut video_pending)
            && let Some((_, tail)) = line.split_once("Alternative name \"")
            && let Some((endpoint, _)) = tail.split_once('"')
            && let Some(device) = devices.last_mut()
        {
            device.endpoint = Some(endpoint.to_owned());
        }
    }
    devices
}

fn video_endpoint<'a>(devices: &'a [VideoDevice], configured: &str) -> Result<&'a str, String> {
    let configured = configured.trim();
    let mut matches = devices.iter().filter(|device| {
        device.name.eq_ignore_ascii_case(configured)
            || device
                .endpoint
                .as_deref()
                .is_some_and(|id| id.eq_ignore_ascii_case(configured))
    });
    let Some(device) = matches.next() else {
        return Err(crate::i18n::message(
            "captureVideoMissing",
            &[("device", configured)],
        ));
    };
    if matches.next().is_some() {
        return Err(crate::i18n::message(
            "captureAmbiguousVideo",
            &[("device", configured)],
        ));
    }
    Ok(device.endpoint.as_deref().unwrap_or(&device.name))
}

fn detected_devices(output: &str, audio: bool) -> Vec<String> {
    let mut devices = Vec::new();
    for line in output.lines() {
        let kind = if audio { "(audio)" } else { "(video)" };
        if line.trim_end().ends_with(kind)
            && let Some((_, tail)) = line.split_once('"')
            && let Some((label, _)) = tail.split_once('"')
        {
            devices.push(label.to_owned());
        } else if !audio && let Some(list) = line.strip_prefix("V4L2: ") {
            devices.extend(
                list.split('、')
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_owned),
            );
        } else if audio
            && line.contains(": capture")
            && let Some((address, _)) = line.split_once(':')
            && let Some((card, device)) = address.split_once('-')
            && let (Ok(card), Ok(device)) =
                (card.trim().parse::<u32>(), device.trim().parse::<u32>())
        {
            devices.push(format!("hw:{card},{device}"));
        }
    }
    devices.sort();
    devices.dedup();
    devices
}

fn device_field(
    w: &mut Workbench,
    id: MediaField,
    label: &'static str,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    let audio = matches!(id, MediaField::Audio(_));
    let editable = text(
        w,
        id,
        label,
        crate::i18n::label("captureDeviceHint"),
        p,
        window,
        cx,
    );
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(div().flex_1().min_w_0().child(editable))
        .child(
            btn(
                format!("pick-{}", id.key()),
                crate::i18n::label("chooseDevice"),
                &BtnVariant::Tiny,
                p,
            )
            .on_click(cx.listener(move |w, _, window, cx| {
                if window.has_active_dialog(cx) {
                    return;
                }
                let Some(dut_id) = w.duts.selected().map(|dut| dut.id.clone()) else {
                    return;
                };
                w.media.scan_devices(w.settings.tools.capture.clone());
                let view = cx.entity();
                window.open_dialog(cx, move |dialog, _, cx| {
                    let target = dut_id.clone();
                    let body = view.update(cx, |w, cx| {
                        let mut list = div().flex().flex_col().gap(px(8.));
                        let choices = if w.media.scanning {
                            list = list.child(crate::i18n::label("detectingDevices"));
                            Vec::new()
                        } else {
                            match &w.media.devices {
                                Some(Ok(raw)) => {
                                    let devices: Vec<_> = if cfg!(windows) && !audio {
                                        let videos = directshow_video_devices(raw);
                                        videos
                                            .iter()
                                            .enumerate()
                                            .filter_map(|(index, video)| {
                                                let same_name = videos
                                                    .iter()
                                                    .filter(|other| {
                                                        other.name.eq_ignore_ascii_case(&video.name)
                                                    })
                                                    .count();
                                                if same_name > 1 {
                                                    let endpoint = video.endpoint.as_ref()?;
                                                    Some((
                                                        format!("{} · {}", video.name, index + 1),
                                                        endpoint.clone(),
                                                    ))
                                                } else {
                                                    Some((
                                                        video.name.clone(),
                                                        video
                                                            .endpoint
                                                            .clone()
                                                            .unwrap_or_else(|| video.name.clone()),
                                                    ))
                                                }
                                            })
                                            .collect()
                                    } else {
                                        detected_devices(raw, audio)
                                            .into_iter()
                                            .map(|name| (name.clone(), name))
                                            .collect()
                                    };
                                    if devices.is_empty() {
                                        list = list.child(crate::i18n::label("noCaptureDevices"));
                                    }
                                    devices
                                }
                                Some(Err(err)) => {
                                    list = list.child(err.clone());
                                    Vec::new()
                                }
                                None => Vec::new(),
                            }
                        };
                        for (i, (display, choice)) in std::iter::once((
                            crate::i18n::label("unbindDevice").to_owned(),
                            String::new(),
                        ))
                        .chain(choices)
                        .enumerate()
                        {
                            let target = target.clone();
                            list = list.child(
                                div()
                                    .id(SharedString::from(format!("capture-device-{i}")))
                                    .px(px(12.))
                                    .py(px(8.))
                                    .rounded(px(6.))
                                    .bg(p.raised)
                                    .text_color(p.text)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(p.hover))
                                    .child(display)
                                    .on_click(cx.listener(move |w, _, window, cx| {
                                        let dut = w.duts.connection_mut(&target);
                                        if let Some(value) =
                                            id.edit(&mut w.settings.tools.capture, dut)
                                        {
                                            value.clone_from(&choice);
                                            w.duts.mark_connection_dirty();
                                        }
                                        window.close_dialog(cx);
                                        cx.notify();
                                    })),
                            );
                        }
                        list
                    });
                    dialog.title(label).child(body)
                });
                cx.notify();
            })),
        )
}

fn text(
    w: &mut Workbench,
    id: MediaField,
    label: &str,
    placeholder: &str,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    let value = id
        .value(&w.settings.tools.capture, w.duts.selected())
        .to_owned();
    field(
        label,
        w.text_field(In::Media(id), &value, placeholder, true, p, window, cx),
        None,
        p,
    )
}

fn mode_field(
    w: &mut Workbench,
    dut: &DutConfig,
    id: MediaField,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    let (source, size, label, hint, default) = match id {
        MediaField::Size(source) => (
            source,
            true,
            "resolution",
            "resolutionHint",
            "keepCaptureSize",
        ),
        MediaField::Rate(source) => (
            source,
            false,
            "frameRate",
            "frameRateHint",
            "deviceDefaultRate",
        ),
        _ => unreachable!("a capture mode field is either resolution or frame rate"),
    };
    let dd = Dd::CaptureMode(id);
    let mut options = vec![(String::new(), crate::i18n::label(default).to_owned())];
    let mut status = None;
    if size {
        options.extend(
            [
                "640x480",
                "800x600",
                "1024x600",
                "1280x720",
                "1920x1080",
                "2560x1440",
                "3840x2160",
            ]
            .into_iter()
            .map(|value| {
                (
                    value.to_owned(),
                    crate::i18n::label("scaledOutputSize").to_owned(),
                )
            }),
        );
    }
    let settings = &w.settings.tools.capture;
    let video = binding(dut, source).video.trim();
    if w.open_dd.as_deref() == Some(dd.key()) {
        if video.is_empty() {
            if !size {
                status = Some(crate::i18n::label("chooseCaptureVideoForModes").to_owned());
            }
        } else {
            let target = ModeTarget {
                dut_id: dut.id.clone(),
                source,
                video: video.to_owned(),
                ffmpeg: settings.ffmpeg.clone(),
            };
            match w.media.probe_modes(target, settings) {
                Some(Ok(modes)) if size => {
                    for (width, height) in &modes.sizes {
                        let value = format!("{width}x{height}");
                        let detail = crate::i18n::label("detectedCaptureSize").to_owned();
                        if let Some((_, existing)) =
                            options.iter_mut().find(|(option, _)| *option == value)
                        {
                            *existing = detail;
                        } else {
                            options.push((value, detail));
                        }
                    }
                }
                Some(Ok(modes)) if !modes.rates.is_empty() => {
                    options.extend(
                        modes
                            .rates
                            .iter()
                            .map(|rate| (rate.clone(), "fps".to_owned())),
                    );
                }
                Some(Err(error)) if !error.is_empty() => {
                    status = Some(crate::i18n::message(
                        "captureModesUnavailableDetail",
                        &[("error", error)],
                    ));
                }
                Some(_) => status = Some(crate::i18n::label("captureModesUnavailable").to_owned()),
                None => status = Some(crate::i18n::label("loadingCaptureModes").to_owned()),
            }
        }
    }
    let value = id.value(settings, Some(dut)).to_owned();
    let select = w.combobox(
        In::Media(id),
        dd,
        &value,
        crate::i18n::label(hint),
        true,
        &options,
        window,
        cx,
    );
    field(
        crate::i18n::label(label),
        div()
            .child(select)
            .children(status.map(|status| subtle(&status, p))),
        None,
        p,
    )
}

#[derive(Clone, Copy)]
enum Action {
    Start,
    Stop,
    Preview,
    Screenshot,
    Record,
    StopRecording,
    Copy,
}

fn source_panel(
    w: &mut Workbench,
    dut: &DutConfig,
    source: Source,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    let key = (dut.id.clone(), source);
    let input = binding(dut, source);
    let enabled = input.is_enabled();
    let toggle_id = match source {
        Source::Hdmi => "enable-hdmi-capture",
        Source::Camera => "enable-camera-capture",
    };
    let dut_id = dut.id.clone();
    let header = panel_head("eye", source.label(), p).child(
        toggle(toggle_id, enabled, p)
            .label(crate::i18n::label("captureEnabled"))
            .on_click(cx.listener(move |w, checked, _, cx| {
                if let Some(dut) = w.duts.connection_mut(&dut_id) {
                    binding_mut(dut, source).enabled = Some(*checked);
                    w.duts.mark_connection_dirty();
                    cx.notify();
                }
            })),
    );
    let panel = div()
        .border_1()
        .border_color(p.line)
        .rounded(px(8.))
        .child(header);
    if !enabled {
        return panel;
    }
    let saved = w.duts.saved_media(&dut.id).map(|media| source.input(media));
    let pending = saved != Some(input);
    let capture = w.media.sessions.get(&key);
    let readiness = saved_capture(&w.duts, &dut.id, source);
    let status = if pending {
        crate::i18n::label("captureSaveFirst").into()
    } else if let Err(error) = &readiness {
        error.clone()
    } else {
        capture
            .map(Capture::status)
            .unwrap_or_else(|| crate::i18n::label("notStarted").into())
    };
    let url = capture
        .filter(|capture| !pending && capture.ready())
        .map(|capture| capture.url.clone());
    let recording = capture.is_some_and(Capture::is_recording);
    let actions = if pending || readiness.is_err() {
        Vec::new()
    } else if capture.is_none_or(Capture::has_ended) && !recording {
        vec![("start", crate::i18n::label("startCapture"), Action::Start)]
    } else if url.is_none() {
        vec![("stop", crate::i18n::label("stopCapture"), Action::Stop)]
    } else {
        let record = if recording {
            (
                "end-record",
                crate::i18n::label("stopRecording"),
                Action::StopRecording,
            )
        } else {
            ("record", crate::i18n::label("recordVideo"), Action::Record)
        };
        vec![
            ("preview", crate::i18n::label("preview"), Action::Preview),
            (
                "screenshot",
                crate::i18n::label("screenshot"),
                Action::Screenshot,
            ),
            record,
            ("copy", crate::i18n::label("copyStreamUrl"), Action::Copy),
            ("stop", crate::i18n::label("stopCapture"), Action::Stop),
        ]
    };
    let mut bar = div().flex().flex_wrap().gap(px(8.));
    for (id, label, action) in actions {
        let key = key.clone();
        bar = bar.child(
            btn(
                format!("capture-{}-{id}", source.key()),
                label,
                &BtnVariant::Tiny,
                p,
            )
            .on_click(cx.listener(move |w, _, _, cx| {
                if let Err(error) = run_action(w, &key, action, cx) {
                    w.media.output = error;
                }
                cx.notify();
            })),
        );
    }
    panel.child(
        div()
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(device_field(
                w,
                MediaField::Video(source),
                crate::i18n::label("videoDevice"),
                p,
                window,
                cx,
            ))
            .child(device_field(
                w,
                MediaField::Audio(source),
                crate::i18n::label("audioDevice"),
                p,
                window,
                cx,
            ))
            .child(mode_field(w, dut, MediaField::Size(source), p, window, cx))
            .child(mode_field(w, dut, MediaField::Rate(source), p, window, cx))
            .child(text(
                w,
                MediaField::Port(source),
                crate::i18n::label("streamPort"),
                crate::i18n::label("streamPortHint"),
                p,
                window,
                cx,
            ))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .child(SharedString::from(status)),
            )
            .children(url.map(|url| {
                div()
                    .text_size(px(12.))
                    .text_color(p.accent)
                    .child(SharedString::from(url))
            }))
            .child(bar),
    )
}

fn run_action(
    w: &mut Workbench,
    key: &(String, Source),
    action: Action,
    cx: &Context<Workbench>,
) -> Result<(), String> {
    if matches!(action, Action::Stop) {
        w.media.stop_capture(key);
        return Ok(());
    }
    w.media.reconcile_bindings(&w.duts);
    let input = saved_capture(&w.duts, &key.0, key.1)?;
    let dut = w
        .duts
        .file
        .duts
        .iter()
        .find(|dut| dut.id == key.0)
        .ok_or(crate::i18n::label("dutNotFound"))?;
    let settings = &w.settings.tools.capture;
    match action {
        Action::Start => {
            if w.media.sessions.get(key).is_none_or(Capture::has_ended) {
                w.media.stop_capture(key);
                let capture = w.media.start_bound_capture(&w.duts, key, input, settings)?;
                w.media.sessions.insert(key.clone(), capture);
            }
        }
        _ => {
            let saved_name = w
                .duts
                .saved_name(&dut.id)
                .ok_or(crate::i18n::label("dutNotFound"))?;
            let capture = w
                .media
                .sessions
                .get_mut(key)
                .ok_or(crate::i18n::label("startCaptureFirst"))?;
            match action {
                Action::Preview => {
                    capture.preview(settings, &format!("{} · {}", dut.name, key.1.label()))?
                }
                Action::Screenshot => {
                    let screenshot = capture.screenshot(
                        &w.media.dir,
                        &dut.id,
                        saved_name,
                        w.artifact_quota.clone(),
                    );
                    let sender = w.media.sender.clone();
                    std::thread::spawn(move || {
                        let status = screenshot()
                            .map(|path| format!("截图已保存到 {}", path.display()))
                            .unwrap_or_else(|e| e);
                        let _ = sender.send(status);
                    });
                }
                Action::Record => {
                    if !capture.is_recording() {
                        let path = capture.record(
                            &w.media.dir,
                            &dut.id,
                            saved_name,
                            w.artifact_quota.clone(),
                        )?;
                        w.media.output = format!("录屏中 · {}", path.display());
                    }
                }
                Action::StopRecording => capture.stop_recording(),
                Action::Copy => copy_text(cx, &capture.url),
                Action::Start | Action::Stop => unreachable!(),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_result_is_only_used_by_its_dut_source_and_video_device() {
        let mut view = MediaView::new(std::env::temp_dir());
        let target = ModeTarget {
            dut_id: "a".into(),
            source: Source::Hdmi,
            video: "fixture".into(),
            ffmpeg: String::new(),
        };
        let (sender, response) = mpsc::channel();
        view.mode_probe = Some(ModeProbe {
            target: target.clone(),
            response,
        });
        sender
            .send(Ok(media::VideoModes {
                sizes: vec![(1920, 1080)],
                rates: vec!["60".into()],
            }))
            .unwrap();
        assert!(view.tick(&DutsState::empty()));
        let settings = CaptureSettings::default();
        assert_eq!(
            view.probe_modes(target.clone(), &settings)
                .unwrap()
                .unwrap()
                .rates,
            ["60"]
        );
        let (_, response) = mpsc::channel();
        view.mode_probe = Some(ModeProbe {
            target: target.clone(),
            response,
        });
        for unrelated in [
            ModeTarget {
                dut_id: "b".into(),
                ..target.clone()
            },
            ModeTarget {
                source: Source::Camera,
                ..target.clone()
            },
            ModeTarget {
                video: "replacement".into(),
                ..target
            },
        ] {
            assert!(view.probe_modes(unrelated, &settings).is_none());
        }
    }

    #[test]
    #[ignore = "reads the capabilities of DUTHUB_TEST_CAPTURE_VIDEO"]
    fn probes_real_capture_device_options() {
        let name = std::env::var("DUTHUB_TEST_CAPTURE_VIDEO").unwrap();
        let settings = CaptureSettings::default();
        let modes = if cfg!(windows) {
            let devices = directshow_video_devices(&media::list_devices(&settings).unwrap());
            media::video_modes(&settings, video_endpoint(&devices, &name).unwrap())
        } else {
            media::video_modes(&settings, &name)
        }
        .unwrap();
        assert!(!modes.rates.is_empty());
        assert!(!modes.sizes.is_empty());
        println!(
            "{} resolutions, frame rates: {}",
            modes.sizes.len(),
            modes.rates.join(", ")
        );
    }

    #[test]
    fn saved_video_belongs_to_one_enabled_dut() {
        let file = serde_json::from_value(serde_json::json!({
            "version": 1,
            "duts": [
                {"id": "a", "name": "A", "serial": {"host": "local", "port": "DEMO"},
                 "media": {"hdmi": {"video": "capture-card"}}},
                {"id": "b", "name": "B", "serial": {"host": "local", "port": "DEMO"},
                 "media": {"camera": {"video": "capture-card", "enabled": false}}}
            ]
        }))
        .unwrap();
        let mut state = DutsState::from_file(file);
        assert!(validate_bindings(&state.file.duts).is_ok());
        assert!(saved_capture(&state, "a", Source::Hdmi).is_ok());
        assert!(saved_capture(&state, "a", Source::Camera).is_err());
        assert!(saved_capture(&state, "b", Source::Camera).is_err());
        state.file.duts[1].media.camera.enabled = Some(true);
        assert!(validate_bindings(&state.file.duts).is_err());
        assert!(
            saved_capture(&state, "b", Source::Camera).is_err(),
            "drafts cannot start capture"
        );
        state.sync_saved_media();
        assert!(saved_capture(&state, "a", Source::Hdmi).is_err());
        assert!(saved_capture(&state, "b", Source::Camera).is_err());
        state.file.duts[0].media.hdmi.enabled = Some(false);
        state.sync_saved_media();
        assert!(saved_capture(&state, "a", Source::Hdmi).is_err());
        assert!(saved_capture(&state, "b", Source::Camera).is_ok());
        let mut duplicate = state.file.duts[1].clone();
        duplicate.media = Default::default();
        state.file.duts.push(duplicate);
        state.sync_saved_media();
        assert!(saved_capture(&state, "b", Source::Camera).is_err());
    }

    #[test]
    fn directshow_matches_the_exact_endpoint_and_rejects_ambiguous_names() {
        let devices = directshow_video_devices(
            r#"
[in#0] "USB Capture" (video)
[in#0] Alternative name "@device_pnp_capture_one"
[in#0] "USB Capture" (video)
[in#0] Alternative name "@device_pnp_capture_two"
[in#0] "USB Capture" (audio)
[in#0] Alternative name "@device_cm_audio"
[in#0] "Integrated Camera" (video)
[in#0] Alternative name "@device_pnp_integrated"
"#,
        );
        assert!(video_endpoint(&devices, "usb capture").is_err());
        assert!(video_endpoint(&devices, "@device_cm_audio").is_err());
        assert_eq!(
            video_endpoint(&devices, "@device_pnp_capture_two").unwrap(),
            "@device_pnp_capture_two"
        );
        assert_eq!(
            video_endpoint(&devices, " Integrated Camera ").unwrap(),
            "@device_pnp_integrated"
        );
    }

    #[test]
    #[ignore = "requires FFmpeg with lavfi and libx264"]
    fn saving_disabled_binding_revokes_old_stream_without_reassigning_it() {
        let file = serde_json::from_value(serde_json::json!({
            "version": 1,
            "duts": [
                {"id": "a", "name": "A", "serial": {"host": "local", "port": "DEMO"},
                 "media": {"hdmi": {"video": "test-pattern"}}},
                {"id": "b", "name": "B", "serial": {"host": "local", "port": "DEMO"}}
            ]
        }))
        .unwrap();
        let mut duts = DutsState::from_file(file);
        let mut view = MediaView::new(std::env::temp_dir());
        let input = saved_capture(&duts, "a", Source::Hdmi).unwrap();
        let args = ["-re", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25"].map(str::to_owned);
        let capture =
            Capture::start_with_args(&CaptureSettings::default(), input, &args, None).unwrap();
        let url = capture.url.clone();
        view.sessions.insert(("a".into(), Source::Hdmi), capture);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            view.tick(&duts);
            if view
                .urls
                .lock()
                .unwrap()
                .get("a")
                .and_then(|sources| sources.get("hdmi"))
                == Some(&url)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "test capture never became ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        duts.file.duts[0].media.hdmi.enabled = Some(false);
        view.reconcile_bindings(&duts);
        assert!(
            view.urls.lock().unwrap().contains_key("a"),
            "draft leaves saved capture intact"
        );
        duts.sync_saved_media();
        view.reconcile_bindings(&duts);
        assert!(view.sessions.is_empty());
        assert!(
            view.urls.lock().unwrap().is_empty(),
            "neither DUT can obtain the old URL"
        );
    }
}
