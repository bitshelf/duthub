//! A single `FFmpeg` capture per source. Independent HTTP readers and the recorder share its output.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use duthub_core::dut_quota::{DutQuota, GIB, QuotaFile};
use duthub_core::model::{CaptureInput, CaptureSettings, DutMediaConfig};
use duthub_core::process::{ProcessRunner, RealProcessRunner, resolve_executable};
use tokio::sync::{Semaphore, broadcast, oneshot};
use uuid::Uuid;

pub type MediaUrls = Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Source {
    Hdmi,
    Camera,
    Device(u32),
}

impl Source {
    pub fn sources(media: &DutMediaConfig) -> impl Iterator<Item = Self> {
        let enabled = media.is_enabled();
        Self::configured_sources(media).filter(move |_| enabled)
    }

    pub fn configured_sources(media: &DutMediaConfig) -> impl Iterator<Item = Self> {
        [Self::Hdmi, Self::Camera]
            .into_iter()
            .filter(|&source| source.input(media) != &CaptureInput::default())
            .chain(media.devices.iter().map(|device| Self::Device(device.id)))
    }

    pub fn input(self, media: &DutMediaConfig) -> &CaptureInput {
        static EMPTY: std::sync::LazyLock<CaptureInput> =
            std::sync::LazyLock::new(CaptureInput::default);
        match self {
            Self::Hdmi => &media.hdmi,
            Self::Camera => &media.camera,
            Self::Device(id) => media
                .devices
                .iter()
                .find(|device| device.id == id)
                .map_or(&*EMPTY, |device| &device.input),
        }
    }

    pub fn input_mut(self, media: &mut DutMediaConfig) -> Option<&mut CaptureInput> {
        match self {
            Self::Hdmi => Some(&mut media.hdmi),
            Self::Camera => Some(&mut media.camera),
            Self::Device(id) => media
                .devices
                .iter_mut()
                .find(|device| device.id == id)
                .map(|device| &mut device.input),
        }
    }

    pub fn key(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Hdmi => "hdmi".into(),
            Self::Camera => "camera".into(),
            Self::Device(id) => format!("capture-{id}").into(),
        }
    }

    pub fn named_label(self, media: &DutMediaConfig) -> &str {
        let name = self.input(media).name.trim();
        if name.is_empty() { self.label() } else { name }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hdmi => crate::i18n::label("hdmiCapture"),
            Self::Camera => crate::i18n::label("usbCamera"),
            Self::Device(_) => crate::i18n::label("captureDevicesTitle"),
        }
    }
}

struct Stream {
    tx: broadcast::Sender<Bytes>,
    clients: Arc<Semaphore>,
    ended: AtomicBool,
    ready: AtomicBool,
}

pub struct Capture {
    pub input: CaptureInput,
    ffmpeg: PathBuf,
    child: Child,
    preview: Option<Child>,
    stream: Arc<Stream>,
    shutdown: Option<oneshot::Sender<()>>,
    recording: Option<Recording>,
    error_tail: Arc<Mutex<String>>,
    started: Instant,
    pub url: String,
}

struct Recording {
    stop: Arc<AtomicBool>,
    task: std::thread::JoinHandle<Result<PathBuf, String>>,
}

fn capture_file(
    root: &Path,
    dut_id: &str,
    name: &str,
    quota: &Arc<DutQuota>,
    extension: &str,
) -> std::io::Result<(QuotaFile, PathBuf)> {
    let filename = format!("{}.{extension}", Uuid::new_v4());
    let (name, owner) = duthub_core::dut_artifacts::open_dut_for_write(root, dut_id, name)?;
    Ok((
        quota.create(dut_id, &owner, &filename)?,
        root.join(name).join(filename),
    ))
}

impl Capture {
    pub fn start(settings: &CaptureSettings, input: &CaptureInput) -> Result<Self, String> {
        let audio = (!input.audio.trim().is_empty()).then_some(if cfg!(windows) {
            "0:a:0"
        } else {
            "1:a:0"
        });
        Self::start_with_args(settings, input, &device_args(input)?, audio)
    }

    pub(super) fn start_with_args(
        settings: &CaptureSettings,
        input: &CaptureInput,
        args: &[String],
        audio: Option<&str>,
    ) -> Result<Self, String> {
        let output_filter = output_resolution_filter(&input.video_size)?;
        let ffmpeg = executable(&settings.ffmpeg, "ffmpeg")?;
        let ip: IpAddr = if settings.listen_ip.trim().is_empty() {
            Ipv4Addr::LOCALHOST.into()
        } else {
            settings
                .listen_ip
                .trim()
                .parse()
                .map_err(|_| crate::i18n::label("mediaInvalidListenIp"))?
        };
        if ip.is_unspecified() || ip.is_multicast() {
            return Err(crate::i18n::label("mediaSpecificLocalIpRequired").into());
        }
        let port = if input.port.trim().is_empty() {
            0
        } else {
            input
                .port
                .trim()
                .parse::<u16>()
                .map_err(|_| crate::i18n::label("mediaInvalidPortRange"))?
        };
        let listener = TcpListener::bind(SocketAddr::new(ip, port))
            .map_err(|e| crate::i18n::message("mediaBindFailed", &[("error", &e.to_string())]))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let path = format!("/live/{}.ts", Uuid::new_v4().simple());
        let url = format!("http://{addr}{path}");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let listener = {
            let _guard = runtime.enter();
            tokio::net::TcpListener::from_std(listener).map_err(|e| {
                crate::i18n::message("mediaStreamStartFailed", &[("error", &e.to_string())])
            })?
        };
        let mut ffmpeg_command = command(&ffmpeg);
        ffmpeg_command
            .args(["-hide_banner", "-loglevel", "warning", "-nostdin"])
            .args(args)
            .args(["-map", "0:v:0"]);
        if let Some(filter) = output_filter {
            ffmpeg_command.args(["-vf", &filter]);
        }
        if let Some(track) = audio {
            ffmpeg_command.args(["-map", track, "-c:a", "aac", "-b:a", "128k"]);
        } else {
            ffmpeg_command.arg("-an");
        }
        let mut child = ffmpeg_command
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-crf",
                "18",
                "-tune",
                "zerolatency",
                "-pix_fmt",
                "yuv420p",
                "-force_key_frames",
                "expr:gte(t,n_forced*1)",
                "-f",
                "mpegts",
                "-muxdelay",
                "0",
                "-flush_packets",
                "1",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                crate::i18n::message("mediaFfmpegStartFailed", &[("error", &e.to_string())])
            })?;
        let (tx, _) = broadcast::channel(512);
        let stream = Arc::new(Stream {
            tx,
            clients: Arc::new(Semaphore::new(16)),
            ended: false.into(),
            ready: false.into(),
        });
        let router = Router::new()
            .route(&path, get(pull))
            .with_state(stream.clone());
        let (shutdown, stop) = oneshot::channel();
        std::thread::spawn(move || {
            runtime.block_on(async move {
                let _ = axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = stop.await;
                    })
                    .await;
            });
        });

        let error_tail = Arc::new(Mutex::new(String::new()));
        let errors = error_tail.clone();
        let mut stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut bytes = [0; 1024];
            while let Ok(count @ 1..) = stderr.read(&mut bytes) {
                let mut message = errors.lock().unwrap();
                if message.len() > 4096 {
                    message.clear();
                }
                message.push_str(&String::from_utf8_lossy(&bytes[..count]));
            }
        });
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let producer = stream.clone();
        std::thread::spawn(move || {
            // Preserve MPEG-TS packet boundaries for late subscribers.
            let mut packet = [0; 188 * 7];
            while stdout.read_exact(&mut packet).is_ok() {
                producer.ready.store(true, Ordering::Release);
                let _ = producer.tx.send(Bytes::copy_from_slice(&packet));
            }
            producer.ended.store(true, Ordering::Release);
        });
        Ok(Self {
            input: input.clone(),
            ffmpeg,
            child,
            preview: None,
            stream,
            shutdown: Some(shutdown),
            recording: None,
            error_tail,
            started: Instant::now(),
            url,
        })
    }

    pub fn ready(&self) -> bool {
        self.stream.ready.load(Ordering::Acquire) && !self.stream.ended.load(Ordering::Acquire)
    }

    pub fn has_ended(&self) -> bool {
        self.stream.ended.load(Ordering::Acquire)
    }

    pub fn status(&self) -> String {
        if self.stream.ended.load(Ordering::Acquire)
            || (!self.ready() && self.started.elapsed() > Duration::from_secs(10))
        {
            let error = self.error_tail.lock().unwrap();
            if error.trim().is_empty() {
                return crate::i18n::label("captureNoSignal").into();
            }
            return error.trim().to_string();
        }
        if self.ready() {
            crate::i18n::label("capturing").into()
        } else {
            crate::i18n::label("waitingForVideo").into()
        }
    }

    pub fn preview(&mut self, settings: &CaptureSettings, title: &str) -> Result<(), String> {
        if self.preview_is_open() {
            return Ok(());
        }
        let ffplay = if settings.ffplay.trim().is_empty() {
            let name = if cfg!(windows) {
                "ffplay.exe"
            } else {
                "ffplay"
            };
            let sibling = self.ffmpeg.with_file_name(name);
            if sibling.is_file() {
                sibling
            } else {
                executable("", "ffplay")?
            }
        } else {
            executable(&settings.ffplay, "ffplay")?
        };
        self.preview = Some(
            command(&ffplay)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "warning",
                    "-autoexit",
                    "-window_title",
                    title,
                    "-probesize",
                    "131072",
                    "-analyzeduration",
                    "750000",
                    "-i",
                    &self.url,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| {
                    crate::i18n::message("mediaPreviewOpenFailed", &[("error", &e.to_string())])
                })?,
        );
        Ok(())
    }

    pub(crate) fn preview_is_open(&mut self) -> bool {
        self.preview
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    pub fn screenshot(
        &self,
        root: &Path,
        dut_id: &str,
        name: &str,
        quota: Arc<DutQuota>,
    ) -> impl FnOnce() -> Result<PathBuf, String> + use<> {
        let ffmpeg = self.ffmpeg.clone();
        let url = self.url.clone();
        let (root, dut_id, name) = (root.to_owned(), dut_id.to_owned(), name.to_owned());
        move || {
            let args = [
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-i",
                &url,
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-c:v",
                "png",
                "-f",
                "image2pipe",
                "pipe:1",
            ]
            .map(str::to_string);
            let png = duthub_core::process::run_binary(
                &ffmpeg,
                &args,
                Duration::from_secs(12),
                32 * 1024 * 1024,
            )
            .map_err(|error| error.to_string())?;
            if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
                return Err(crate::i18n::label("mediaScreenshotNotPng").into());
            }
            let (mut output, path) = capture_file(&root, &dut_id, &name, &quota, "png")
                .map_err(|error| error.to_string())?;
            output.write_all(&png).map_err(|error| error.to_string())?;
            Ok(path)
        }
    }

    pub fn inline_preview(&self) -> Result<InlinePreview, String> {
        InlinePreview::start(&self.ffmpeg, &self.url)
    }

    pub fn record(
        &mut self,
        root: &Path,
        dut_id: &str,
        name: &str,
        quota: Arc<DutQuota>,
    ) -> Result<PathBuf, String> {
        let (file, path) =
            capture_file(root, dut_id, name, &quota, "ts").map_err(|error| error.to_string())?;
        let first_path = path.clone();
        let (root, dut_id, name) = (root.to_owned(), dut_id.to_owned(), name.to_owned());
        let stop = Arc::new(AtomicBool::new(false));
        let should_stop = stop.clone();
        let stream = self.stream.clone();
        let mut rx = stream.tx.subscribe();
        let task = std::thread::spawn(move || {
            let mut output = BufWriter::new(file);
            let mut clip_bytes = 0;
            let mut path = path;
            loop {
                match rx.try_recv() {
                    Ok(chunk) => {
                        let limit = (quota.limit() / 4).min(GIB);
                        if clip_bytes > 0 && clip_bytes + chunk.len() as u64 > limit {
                            output.flush().map_err(|error| error.to_string())?;
                            drop(output);
                            let (file, new_path) =
                                capture_file(&root, &dut_id, &name, &quota, "ts")
                                    .map_err(|error| error.to_string())?;
                            output = BufWriter::new(file);
                            path = new_path;
                            clip_bytes = 0;
                        }
                        output
                            .write_all(&chunk)
                            .map_err(|error| error.to_string())?;
                        clip_bytes += chunk.len() as u64;
                    }
                    Err(broadcast::error::TryRecvError::Empty) => {
                        if should_stop.load(Ordering::Acquire)
                            || stream.ended.load(Ordering::Acquire)
                        {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(15));
                    }
                    Err(e) => {
                        return Err(crate::i18n::message(
                            "mediaRecordingInterrupted",
                            &[("error", &e.to_string())],
                        ));
                    }
                }
                if should_stop.load(Ordering::Acquire) {
                    break;
                }
            }
            output.flush().map_err(|error| error.to_string())?;
            Ok(path)
        });
        self.recording = Some(Recording { stop, task });
        Ok(first_path)
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    pub fn stop_recording(&self) {
        if let Some(recording) = &self.recording {
            recording.stop.store(true, Ordering::Release);
        }
    }

    pub fn poll_recording(&mut self) -> Option<Result<PathBuf, String>> {
        if self.recording.as_ref()?.task.is_finished() {
            let recording = self.recording.take().unwrap();
            Some(
                recording.task.join().unwrap_or_else(|_| {
                    Err(crate::i18n::label("mediaRecordingWorkerExited").into())
                }),
            )
        } else {
            None
        }
    }
}

pub struct InlinePreview {
    child: Child,
    frame: Arc<Mutex<Option<Arc<gpui_kit::RenderImage>>>>,
    error: Arc<Mutex<String>>,
}

impl InlinePreview {
    fn start(ffmpeg: &Path, url: &str) -> Result<Self, String> {
        let mut child = command(ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-nostdin",
                "-rw_timeout", "5000000", "-analyzeduration", "1000000", "-probesize", "500000",
                "-i", url, "-an", "-vf",
                "fps=10,scale=640:360:force_original_aspect_ratio=decrease,pad=640:360:(ow-iw)/2:(oh-ih)/2",
                "-threads", "1", "-pix_fmt", "bgra", "-f", "rawvideo", "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| err.to_string())?;
        let mut stdout = child.stdout.take().expect("preview stdout is piped");
        let stderr = child.stderr.take().expect("preview stderr is piped");
        let frame = Arc::new(Mutex::new(None));
        let latest = frame.clone();
        std::thread::spawn(move || {
            loop {
                let mut pixels = vec![0; 640 * 360 * 4];
                if stdout.read_exact(&mut pixels).is_err() {
                    break;
                }
                let buffer =
                    image::RgbaImage::from_raw(640, 360, pixels).expect("fixed frame size");
                let frames = smallvec::smallvec![image::Frame::new(buffer)];
                *latest.lock().unwrap() = Some(Arc::new(gpui_kit::RenderImage::new(frames)));
            }
        });
        let error = Arc::new(Mutex::new(String::new()));
        let diagnostic = error.clone();
        std::thread::spawn(move || {
            let mut message = String::new();
            let _ = stderr.take(8192).read_to_string(&mut message);
            *diagnostic.lock().unwrap() = message;
        });
        Ok(Self {
            child,
            frame,
            error,
        })
    }

    pub fn poll(&mut self) -> (Option<Arc<gpui_kit::RenderImage>>, Option<String>) {
        let frame = self.frame.lock().unwrap().take();
        let error = self.child.try_wait().ok().flatten().map(|status| {
            let detail = self.error.lock().unwrap();
            format!("FFmpeg {status} {}", detail.trim())
        });
        (frame, error)
    }

    pub fn has_update(&mut self) -> bool {
        self.frame.lock().unwrap().is_some() || self.child.try_wait().ok().flatten().is_some()
    }
}

impl Drop for InlinePreview {
    fn drop(&mut self) {
        terminate(&mut self.child);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop_recording();
        self.stream.ended.store(true, Ordering::Release);
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(preview) = self.preview.as_mut() {
            terminate(preview);
        }
        terminate(&mut self.child);
        if let Some(recording) = self.recording.take() {
            let _ = recording.task.join();
        }
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

async fn pull(State(stream): State<Arc<Stream>>) -> Response {
    let Ok(permit) = stream.clients.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let rx = stream.tx.subscribe();
    let chunks = futures_util::stream::unfold(
        (stream, rx, permit),
        |(stream, mut rx, permit)| async move {
            while !stream.ended.load(Ordering::Acquire) {
                match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
                    Ok(Ok(data)) => return Some((Ok::<_, Infallible>(data), (stream, rx, permit))),
                    Ok(Err(_)) => break,
                    Err(_) => {}
                }
            }
            None
        },
    );
    (
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(chunks),
    )
        .into_response()
}

fn command(path: &Path) -> Command {
    let mut command = Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW (SDL creates the preview window).
    }
    command
}

fn executable(spec: &str, default: &str) -> Result<PathBuf, String> {
    executable_with_env(spec, default, |name| std::env::var(name).ok())
}

fn executable_with_env(
    spec: &str,
    default: &str,
    lookup_env: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    static VAR: OnceLock<regex::Regex> = OnceLock::new();
    let spec = spec.trim();
    let spec = if spec.is_empty() { default } else { spec };
    if Path::new(spec).is_absolute() && Path::new(spec).is_file() {
        return resolve_executable(spec).map_err(|e| e.to_string());
    }
    // A downloaded install shadows PATH for the canonical bare name only
    // (explicit paths, other names and %VAR% expansions resolve as before).
    if let Some(path) = duthub_desktop::tool_resolver::downloaded_preferred(spec, default) {
        return Ok(path);
    }
    let mut missing = None;
    let expanded = VAR.get_or_init(|| regex::Regex::new(r"%([A-Za-z_][A-Za-z0-9_]*)%|\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)").unwrap())
        .replace_all(spec, |caps: &regex::Captures<'_>| {
            let name = [1, 2, 3].into_iter().find_map(|i| caps.get(i)).expect("each variable syntax captures the name").as_str();
            lookup_env(name).unwrap_or_else(|| { missing = Some(name.to_string()); String::new() })
        });
    if let Some(name) = missing {
        return Err(crate::i18n::message("mediaEnvVarUnset", &[("name", &name)]));
    }
    resolve_executable(&expanded)
        .and_then(|path| {
            path.canonicalize()
                .map_err(|e| duthub_core::DeviceError::Invalid(e.to_string()))
        })
        .map_err(|e| {
            crate::i18n::message(
                "mediaExecutableNotFound",
                &[("path", expanded.as_ref()), ("error", &e.to_string())],
            )
        })
}

fn output_resolution_filter(value: &str) -> Result<Option<String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let size = value
        .split_once(['x', 'X', '×'])
        .and_then(|(width, height)| {
            let parse = |side: &str| {
                side.trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|size| *size >= 2 && size.is_multiple_of(2))
            };
            Some((parse(width)?, parse(height)?))
        });
    let (width, height) =
        size.ok_or_else(|| crate::i18n::label("invalidOutputResolution").to_owned())?;
    Ok(Some(format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease:force_divisible_by=2,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1"
    )))
}

fn device_args(input: &CaptureInput) -> Result<Vec<String>, String> {
    let video = input.video.trim();
    let audio = input.audio.trim();
    if !input.has_video() {
        return Err(crate::i18n::label("captureNeedsVideo").into());
    }
    let windows = cfg!(windows);
    let mut args = ["-f", if windows { "dshow" } else { "v4l2" }]
        .map(str::to_string)
        .to_vec();
    if windows {
        args.extend(["-rtbufsize", "512M"].map(str::to_string));
    }
    if !input.frame_rate.trim().is_empty() {
        args.extend(["-framerate", input.frame_rate.trim()].map(str::to_string));
    }
    args.push("-i".into());
    args.push(if windows {
        if audio.is_empty() {
            format!("video={video}")
        } else {
            format!("video={video}:audio={audio}")
        }
    } else {
        video.into()
    });
    if !windows && !audio.is_empty() {
        args.extend(["-f", "alsa", "-i"].map(str::to_string));
        args.push(audio.into());
    }
    Ok(args)
}

pub fn list_devices(settings: &CaptureSettings) -> Result<String, String> {
    if cfg!(windows) {
        let ffmpeg = executable(&settings.ffmpeg, "ffmpeg")?;
        let args = [
            "-hide_banner",
            "-list_devices",
            "true",
            "-f",
            "dshow",
            "-i",
            "dummy",
        ]
        .map(str::to_string);
        let result = RealProcessRunner
            .run(&ffmpeg, &args, Duration::from_secs(8))
            .map_err(|e| e.to_string())?;
        // DirectShow prints the device list to stderr and exits with an error for the dummy input.
        Ok(result.stderr)
    } else {
        let mut video: Vec<_> = fs::read_dir("/dev")
            .map_err(|e| e.to_string())?
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("video"))
            .map(|entry| entry.path().display().to_string())
            .collect();
        video.sort();
        let audio = fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
        Ok(format!(
            "V4L2: {}\n{}\n{}",
            video.join("、"),
            crate::i18n::label("mediaAlsaDeviceHint"),
            audio.trim()
        ))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct VideoModes {
    pub sizes: Vec<(u32, u32)>,
    pub rates: Vec<String>,
}

pub(crate) fn video_modes(
    settings: &CaptureSettings,
    video_endpoint: &str,
) -> Result<VideoModes, String> {
    let (exe, args) = if cfg!(windows) {
        (
            executable(&settings.ffmpeg, "ffmpeg")?,
            vec![
                "-hide_banner".into(),
                "-f".into(),
                "dshow".into(),
                "-list_options".into(),
                "true".into(),
                "-i".into(),
                format!("video={video_endpoint}"),
            ],
        )
    } else {
        (
            resolve_executable("v4l2-ctl").map_err(|err| err.to_string())?,
            vec![
                "--device".into(),
                video_endpoint.into(),
                "--list-formats-ext".into(),
            ],
        )
    };
    let output = RealProcessRunner
        .run(&exe, &args, Duration::from_secs(8))
        .map_err(|err| err.to_string())?;
    // DirectShow exits after writing the capability listing to stderr.
    let modes = if cfg!(windows) {
        parse_video_modes(&output.stderr, true)
    } else {
        parse_video_modes(&output.stdout, false)
    };
    if modes.sizes.is_empty() && modes.rates.is_empty() {
        let detail = output
            .stderr
            .lines()
            .find(|line| {
                line.contains("Could not") || line.contains("failed") || line.contains("busy")
            })
            .or_else(|| output.stderr.trim().lines().last())
            .unwrap_or_default();
        return Err(detail.to_owned());
    }
    Ok(modes)
}

fn parse_video_modes(output: &str, directshow: bool) -> VideoModes {
    let mut sizes = Vec::new();
    let mut rates = Vec::new();
    for line in output.lines() {
        let parse_size = |value: &str| -> Option<(u32, u32)> {
            let (width, height) = value.split_once('x')?;
            let (width, height) = (width.parse::<u32>().ok()?, height.parse::<u32>().ok()?);
            (width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2))
                .then_some((width, height))
        };
        let parse_rate = |value: &str| -> Option<f64> {
            value
                .parse::<f64>()
                .ok()
                .filter(|rate| rate.is_finite() && *rate > 0.)
        };
        if directshow {
            let mut bounds = Vec::new();
            for value in line.split_whitespace() {
                if let Some(size) = value.strip_prefix("s=").and_then(parse_size) {
                    sizes.push(size);
                }
                if let Some(rate) = value.strip_prefix("fps=").and_then(parse_rate) {
                    bounds.push(rate);
                }
            }
            if let [min, max] = bounds.as_slice() {
                rates.extend([*min, *max]);
                rates.extend(
                    [15., 24., 25., 30., 50., 60., 120.]
                        .into_iter()
                        .filter(|rate| (*min..=*max).contains(rate)),
                );
            }
        } else if let Some(size) = line
            .trim()
            .strip_prefix("Size: Discrete ")
            .and_then(parse_size)
        {
            sizes.push(size);
        } else if let Some(rate) = line
            .trim()
            .strip_prefix("Interval:")
            .and_then(|line| line.rsplit_once('('))
            .and_then(|(_, tail)| tail.strip_suffix(" fps)"))
            .and_then(parse_rate)
        {
            rates.push(rate);
        }
    }
    sizes.sort_unstable();
    sizes.dedup();
    rates.sort_by(f64::total_cmp);
    rates.dedup_by(|rate, previous| (*rate - *previous).abs() < 0.001);
    VideoModes {
        sizes,
        rates: rates.into_iter().map(|rate| rate.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_resolution_is_not_sent_to_the_camera() {
        let input = CaptureInput {
            video: "fixture".into(),
            video_size: "1024×600".into(),
            frame_rate: "30".into(),
            ..Default::default()
        };
        let args = device_args(&input).unwrap();
        assert!(
            !args.iter().any(|arg| arg == "-thread_queue_size"),
            "FFmpeg 9+ rejects -thread_queue_size on inputs: {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-video_size" || arg == &input.video_size)
        );
        assert!(args.windows(2).any(|args| args == ["-framerate", "30"]));
        let filter = output_resolution_filter(&input.video_size)
            .unwrap()
            .unwrap();
        assert!(filter.starts_with("scale=1024:600:"));
        assert!(filter.contains("pad=1024:600:"));
        assert_eq!(output_resolution_filter(" ").unwrap(), None);
        assert!(output_resolution_filter("1023x600").is_err());
        assert!(output_resolution_filter("1024x600,negate").is_err());
    }

    #[test]
    fn parses_device_sizes_and_frame_rates() {
        let dshow = parse_video_modes(
            "pixel_format=yuyv422 min s=1920x1080 fps=10 max s=1920x1080 fps=60.0002\n\
             pixel_format=yuyv422 min s=640x480 fps=5 max s=640x480 fps=7\n\
             vcodec=mjpeg min s=1280x720 fps=29.9701 max s=1280x720 fps=29.9701",
            true,
        );
        assert_eq!(dshow.sizes, [(640, 480), (1280, 720), (1920, 1080)]);
        assert_eq!(
            dshow.rates,
            [
                "5", "7", "10", "15", "24", "25", "29.9701", "30", "50", "60"
            ]
        );
        let v4l = parse_video_modes(
            "Size: Discrete 640x480\n  Interval: Discrete 0.033s (30.000 fps)\n\
             Size: Discrete 1280x720\n  Interval: Discrete 0.067s (15.000 fps)",
            false,
        );
        assert_eq!(v4l.sizes, [(640, 480), (1280, 720)]);
        assert_eq!(v4l.rates, ["15", "30"]);
    }

    #[test]
    #[ignore = "uses the capture source explicitly named in DUTHUB_TEST_CAPTURE_VIDEO"]
    fn capture_stream_outputs_configured_resolution() {
        let input = CaptureInput {
            video: std::env::var("DUTHUB_TEST_CAPTURE_VIDEO").expect("capture device name"),
            video_size: "1024x600".into(),
            ..Default::default()
        };
        let capture = Capture::start(&CaptureSettings::default(), &input).unwrap();
        assert_preview_frames(&capture);
        let probe = capture.ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        let args = [
            "-v",
            "error",
            "-analyzeduration",
            "1000000",
            "-probesize",
            "500000",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "json",
            &capture.url,
        ]
        .map(str::to_owned);
        let result = RealProcessRunner
            .run(&probe, &args, Duration::from_secs(12))
            .unwrap();
        assert_eq!(result.exit_code, Some(0), "{}", result.stderr);
        let info: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(info["streams"][0]["width"], 1024);
        assert_eq!(info["streams"][0]["height"], 600);
    }

    #[test]
    #[ignore = "requires FFmpeg with lavfi and libx264, and localhost TCP"]
    fn inline_preview_decodes_capture_frames() {
        let args = ["-re", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25"].map(str::to_string);
        let capture = Capture::start_with_args(
            &CaptureSettings::default(),
            &CaptureInput::default(),
            &args,
            None,
        )
        .unwrap();
        assert_preview_frames(&capture);
    }

    #[test]
    #[ignore = "uses the capture source explicitly named in DUTHUB_TEST_CAPTURE_VIDEO"]
    fn inline_preview_reads_configured_capture_device() {
        let input = CaptureInput {
            video: std::env::var("DUTHUB_TEST_CAPTURE_VIDEO").expect("capture device name"),
            audio: std::env::var("DUTHUB_TEST_CAPTURE_AUDIO").unwrap_or_default(),
            ..Default::default()
        };
        let capture = Capture::start(&CaptureSettings::default(), &input).unwrap();
        assert_preview_frames(&capture);
    }

    fn assert_preview_frames(capture: &Capture) {
        let mut preview = capture.inline_preview().unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let (frame, error) = preview.poll();
            assert!(error.is_none(), "{error:?}");
            if let Some(frame) = frame {
                assert_eq!(frame.as_bytes(0).unwrap().len(), 640 * 360 * 4);
                break;
            }
            assert!(Instant::now() < deadline, "{}", capture.status());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    #[ignore = "requires FFmpeg on PATH (sibling ffplay and env-var lookup share the binary)"]
    fn executable_resolves_via_path_absolute_and_env_var() {
        // Resolve the unqualified executable through PATH.
        let via_path = executable("", "ffmpeg").unwrap();
        assert!(via_path.is_file(), "{via_path:?}");
        // Try the absolute path after removing the Windows extended-path prefix.
        let plain = via_path
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string();
        let absolute = executable(&plain, "ffmpeg").unwrap();
        assert!(absolute.is_file(), "{absolute:?}");
        // Expand the variable without modifying the test process environment.
        let directory = Path::new(&plain)
            .parent()
            .expect("the resolved executable has a containing directory");
        let expanded =
            executable_with_env("%DUTHUB_MEDIA_TEST_HOME%/ffmpeg.exe", "ffmpeg", |name| {
                (name == "DUTHUB_MEDIA_TEST_HOME").then(|| directory.to_string_lossy().into_owned())
            })
            .expect("resolve ffmpeg through a configured directory");
        assert!(expanded.is_file(), "{expanded:?}");
        let error = executable_with_env("%DUTHUB_NO_SUCH_VAR_XYZ%/ffmpeg.exe", "ffmpeg", |_| None)
            .expect_err("report the missing variable");
        assert!(error.contains("DUTHUB_NO_SUCH_VAR_XYZ"), "{error}");
        // Preview falls back to ffplay next to the selected ffmpeg executable.
        let sibling = Path::new(&plain).with_file_name(if cfg!(windows) {
            "ffplay.exe"
        } else {
            "ffplay"
        });
        assert!(
            sibling.is_file(),
            "The FFmpeg directory should contain the file `ffplay: {sibling:?}`."
        );
    }

    #[test]
    #[ignore = "requires FFmpeg with lavfi, libx264 and AAC, and localhost TCP"]
    fn capture_honors_requested_port() {
        // First, use port 0 to find an available port. Then, request to connect to that port.
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let args = ["-re", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25"].map(str::to_string);
        let input = CaptureInput {
            port: port.to_string(),
            ..Default::default()
        };
        let capture =
            Capture::start_with_args(&CaptureSettings::default(), &input, &args, None).unwrap();
        assert!(
            capture.url.contains(&format!(":{port}/")),
            "The stream address must use the fixed port specified in the request: {}.",
            capture.url
        );
        let deadline = Instant::now() + Duration::from_secs(8);
        while !capture.ready() {
            assert!(Instant::now() < deadline, "{}", capture.status());
            std::thread::sleep(Duration::from_millis(50));
        }
        // Send an HTTP GET request to the fixed port, and verify the response from the stream
        // pulling service.
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let path = capture.url.split_once("live").unwrap();
        let request = format!(
            "GET /live{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
            path.1
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut head = [0; 512];
        let count = stream.read(&mut head).unwrap();
        let head = String::from_utf8_lossy(&head[..count]);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("video/mp2t"), "{head}");
    }

    #[test]
    #[ignore = "requires FFmpeg with lavfi, libx264 and AAC, and localhost TCP"]
    fn capture_shares_audio_and_video_with_two_http_clients_and_recorder() {
        fn decode(ffmpeg: &Path, input: &str) {
            let args = [
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                input,
                "-map",
                "0:v:0",
                "-map",
                "0:a:0",
                "-t",
                "0.5",
                "-f",
                "null",
                "-",
            ]
            .map(str::to_string);
            let result = RealProcessRunner
                .run(ffmpeg, &args, Duration::from_secs(12))
                .unwrap();
            assert_eq!(result.exit_code, Some(0), "{}", result.stderr);
        }

        let args = [
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=25[out0];sine=frequency=880:sample_rate=48000[out1]",
        ]
        .map(str::to_string);
        let mut capture = Capture::start_with_args(
            &CaptureSettings::default(),
            &CaptureInput::default(),
            &args,
            Some("0:a:0"),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !capture.ready() {
            assert!(Instant::now() < deadline, "{}", capture.status());
            std::thread::sleep(Duration::from_millis(50));
        }

        let dir = std::env::temp_dir().join(format!("duthub-capture-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let quota = DutQuota::new(GIB);
        let clip = capture
            .record(&dir, "fixture-dut", "TestDUT", quota.clone())
            .unwrap();
        let shot = std::thread::spawn(capture.screenshot(&dir, "fixture-dut", "TestDUT", quota));
        std::thread::scope(|scope| {
            scope.spawn(|| decode(&capture.ffmpeg, &capture.url));
            decode(&capture.ffmpeg, &capture.url);
        });
        capture.stop_recording();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(result) = capture.poll_recording() {
                assert_eq!(result.unwrap(), clip);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        let shot = shot.join().unwrap().unwrap();
        assert!(fs::read(&shot).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        decode(&capture.ffmpeg, clip.to_str().unwrap());
        fs::remove_file(clip).unwrap();
        fs::remove_file(shot).unwrap();
        fs::remove_file(dir.join("TestDUT/.duthub-dut-id")).unwrap();
        fs::remove_dir(dir.join("TestDUT")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
