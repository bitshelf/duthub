use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use duthub_core::dut_quota::DutQuota;
use duthub_core::media_op::{
    MediaArtifact, MediaOp, MediaOpError, MediaOpReply, MediaOpRequest, OpId, OpReceiver,
    RecordingHandle, RecordingId,
};
use duthub_core::model::{CaptureInput, CaptureSettings, DutConfig};
use duthub_desktop::capture_backend::{
    DemandEntry, DemandKind, HOLD_SECS, LeaseId, MEDIA_BUSY, MEDIA_CAPTURE_ENDED,
    MEDIA_CAPTURE_FAILED, MEDIA_CAPTURE_UNAVAILABLE, MEDIA_NOT_BOUND, MEDIA_OP_TIMEOUT,
    MEDIA_RECORDING_ENDED, MEDIA_RECORDING_IN_PROGRESS, MEDIA_RECORDING_UNKNOWN,
    MEDIA_START_FAILED, ReclaimDecision, artifact_bytes, effective_idle, png_dimensions,
};
use gpui_kit::component::Disableable;
use gpui_kit::component::WindowExt;
use gpui_kit::prelude::FluentBuilder;
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
    Name(Source),
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
            Self::Name(_) => "capture-name",
            Self::Video(Source::Device(_)) => "capture-video",
            Self::Audio(Source::Device(_)) => "capture-audio",
            Self::Size(Source::Device(_)) => "capture-size",
            Self::Rate(Source::Device(_)) => "capture-rate",
            Self::Port(Source::Device(_)) => "capture-port",
        }
    }

    pub fn control_key(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Size(Source::Device(id)) | Self::Rate(Source::Device(id)) => {
                format!("{}-{id}", self.key()).into()
            }
            _ => self.key().into(),
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
            Self::Name(source) => dut
                .map(|dut| binding(dut, source).name.as_str())
                .unwrap_or_default(),
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
            Self::Name(source) => &mut source.input_mut(&mut dut?.media)?.name,
            Self::Video(source) => {
                let input = source.input_mut(&mut dut?.media)?;
                // Editing the video binding replaces any saved per-device switch setting.
                input.enabled = None;
                &mut input.video
            }
            Self::Audio(source) => &mut source.input_mut(&mut dut?.media)?.audio,
            Self::Size(source) => &mut source.input_mut(&mut dut?.media)?.video_size,
            Self::Rate(source) => &mut source.input_mut(&mut dut?.media)?.frame_rate,
            Self::Port(source) => &mut source.input_mut(&mut dut?.media)?.port,
        })
    }
}

fn binding(dut: &DutConfig, source: Source) -> &CaptureInput {
    source.input(&dut.media)
}

pub struct MediaView {
    pub urls: MediaUrls,
    dir: PathBuf,
    /// Resolved capture settings injected by `Workbench` (§3.7): `ffmpeg` has
    /// already been through `tool_resolver`. Every capture start reads this
    /// single copy so the panel and the Agent path resolve the same executable.
    capture: CaptureSettings,
    /// App-wide artifact quota shared with the rest of the desktop (§3.7).
    /// `None` = ops fail with `MEDIA_CAPTURE_UNAVAILABLE` instead of bypassing
    /// quota accounting.
    quota: Option<Arc<DutQuota>>,
    /// MCP op bridge: request queue, in-flight ops and worker results (§5.5).
    ops: MediaOpPort,
    /// Lease ledger (§4.2). C2 records Agent-op leases and mirrors the
    /// recording state; C3 adds the panel/monitor/hold leases and reclaim.
    demand: BTreeMap<(String, Source), DemandEntry>,
    next_lease: LeaseId,
    /// Ids of the panel's "start capture" `Manual` pin (one per binding).
    manual_leases: BTreeMap<(String, Source), LeaseId>,
    /// Ids of the side-panel `Monitor` lease (idempotent per binding).
    monitor_leases: BTreeMap<(String, Source), LeaseId>,
    /// Reader holds (external ffplay preview, copied URL), re-armed on use.
    hold_leases: BTreeMap<((String, Source), HoldKind), LeaseId>,
    /// Active Agent recordings, keyed by `recordingId` (§5.5/§5.6).
    recordings: BTreeMap<RecordingId, ActiveRecording>,
    /// Ids invalidated by `stop_capture`: a later `recordStop` answers
    /// `MEDIA_RECORDING_ENDED` instead of leaking an artifact (§4.5).
    aborted: BTreeMap<RecordingId, Instant>,
    /// Keys whose late `poll_recording` result is discarded because the
    /// `recordStop` reply already timed out (§5.5).
    orphan_results: BTreeSet<(String, Source)>,
    next_recording: RecordingId,
    /// Largest elapsed whole second across active recordings; drives the
    /// "Agent recording" status repaint without refreshing every tick.
    recording_seconds: u64,
    sessions: BTreeMap<(String, Source), Capture>,
    draft_preview: Option<DraftPreview>,
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
    collapsed_sources: BTreeSet<(String, Source)>,
}

struct DraftPreview {
    capture: Capture,
    title: String,
    opened: bool,
    started: Instant,
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

// ---------------------------------------------------------------------------
// MCP op bridge (§5.5): request queue + in-flight table + worker results
// ---------------------------------------------------------------------------

/// Start bound: a fresh ephemeral capture must become `ready()` within this
/// window or the op alone fails (`MEDIA_START_FAILED`); the session stays alive
/// and is left to idle reclaim.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Snapshot job bound: 12s inside `Capture::screenshot` plus slack.
const JOB_TIMEOUT: Duration = Duration::from_secs(15);
/// Post-`recordStop` reply bound. This is **not** a recording length limit: a
/// recording ends only through `recordStop` or `stop_capture`.
const RECORD_STOP_GRACE: Duration = Duration::from_secs(5);
/// Tombstone lifetime for recording ids invalidated by `stop_capture`.
const ABORTED_TTL: Duration = Duration::from_secs(600);
const MAX_IN_FLIGHT: usize = 4;
const MAX_ACTIVE_RECORDINGS: usize = 4;
const OP_QUEUE_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InFlightKind {
    Snapshot,
    RecordStart,
    RecordStop(RecordingId),
}

/// Self-expiring reader holds (§4.1): neither can observe its consumer, so
/// both are bounded by `HOLD_SECS` and refreshed on every repeat use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HoldKind {
    ExternalPreview,
    CopyUrl,
}

impl HoldKind {
    fn demand(self) -> DemandKind {
        match self {
            Self::ExternalPreview => DemandKind::ExternalPreview,
            Self::CopyUrl => DemandKind::CopyUrl,
        }
    }
}

/// One op the GUI has taken but not answered yet.
///
/// The oneshot sender never leaves the GUI thread: workers report through the
/// std `done` channel and the GUI sends the reply exactly once.
struct InFlight {
    key: (String, Source),
    kind: InFlightKind,
    queued_at: Instant,
    started_at: Option<Instant>,
    /// AgentOp lease acquired for this op; `None` for `recordStop`, whose lease
    /// belongs to the recording registration.
    lease: Option<LeaseId>,
    job_deadline: Option<Instant>,
    note: Option<String>,
    reply: Option<tokio::sync::oneshot::Sender<MediaOpReply>>,
}

/// A worker thread's result (std channel, drained without blocking).
struct OpFinished {
    id: OpId,
    reply: MediaOpReply,
}

/// One active Agent recording (§5.6); removed only by `recordStop`,
/// `RECORD_STOP_GRACE` or `stop_capture` — never by elapsed time.
struct ActiveRecording {
    key: (String, Source),
    note: Option<String>,
    first_path: PathBuf,
    started_at: Instant,
    lease: LeaseId,
    stop_requested_at: Option<Instant>,
}

/// Internal op failure carrying the `MEDIA_*` code (§5.7).
#[derive(Debug)]
struct OpFailure {
    code: &'static str,
    message: String,
}

impl OpFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// GUI end of the bridge: the request queue, the in-flight table and the
/// results coming back from snapshot workers.
pub struct MediaOpPort {
    rx: mpsc::Receiver<MediaOpRequest>,
    tx: mpsc::SyncSender<MediaOpRequest>,
    next_id: Arc<AtomicU64>,
    done_rx: mpsc::Receiver<OpFinished>,
    done_tx: mpsc::Sender<OpFinished>,
    in_flight: BTreeMap<OpId, InFlight>,
}

/// Clone-able submit end handed to `RuntimeMcpState`. A full or closed queue
/// drops the request (closing its oneshot), so the tool sees
/// `MEDIA_OP_DROPPED` — never "this build has no capture capability".
#[derive(Clone)]
pub struct MediaOpSubmitter {
    tx: mpsc::SyncSender<MediaOpRequest>,
    next_id: Arc<AtomicU64>,
}

impl MediaOpSubmitter {
    /// Enqueue one op and return the receiver the MCP tool awaits.
    pub fn submit(&self, dut_id: String, source: Option<String>, op: MediaOp) -> OpReceiver {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (request, receiver) = MediaOpRequest::channel(id, dut_id, source, op);
        // A failed `try_send` hands the request back; dropping it closes the
        // oneshot, which the tool maps to `MEDIA_OP_DROPPED`.
        let _ = self.tx.try_send(request);
        receiver
    }
}

impl MediaOpPort {
    fn new() -> Self {
        let (tx, rx) = mpsc::sync_channel(OP_QUEUE_DEPTH);
        let (done_tx, done_rx) = mpsc::channel();
        Self {
            rx,
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            done_rx,
            done_tx,
            in_flight: BTreeMap::new(),
        }
    }

    /// Submit end for the MCP runtime (clones share one queue).
    pub fn submitter(&self) -> MediaOpSubmitter {
        MediaOpSubmitter {
            tx: self.tx.clone(),
            next_id: self.next_id.clone(),
        }
    }

    fn try_recv_request(&self) -> Option<MediaOpRequest> {
        self.rx.try_recv().ok()
    }

    fn insert(&mut self, id: OpId, entry: InFlight) {
        self.in_flight.insert(id, entry);
    }

    fn op_for_key(&self, key: &(String, Source)) -> Option<OpId> {
        self.in_flight
            .iter()
            .find(|(_, entry)| &entry.key == key)
            .map(|(id, _)| *id)
    }

    fn ids_for_key(&self, key: &(String, Source)) -> Vec<OpId> {
        self.in_flight
            .iter()
            .filter(|(_, entry)| &entry.key == key)
            .map(|(id, _)| *id)
            .collect()
    }

    fn op_by_kind(&self, kind: InFlightKind) -> Option<OpId> {
        self.in_flight
            .iter()
            .find(|(_, entry)| entry.kind == kind)
            .map(|(id, _)| *id)
    }

    /// Send the one reply this op is allowed once; returns the entry so the
    /// caller can release its lease. `None` = already answered/rejected.
    fn finish(&mut self, id: OpId, reply: MediaOpReply) -> Option<InFlight> {
        let mut entry = self.in_flight.remove(&id)?;
        if let Some(sender) = entry.reply.take() {
            let _ = sender.send(reply);
        }
        Some(entry)
    }

    /// Reject one op with a `MEDIA_*` code; the caller releases the lease.
    fn fail(
        &mut self,
        id: OpId,
        code: &'static str,
        message: impl Into<String>,
    ) -> Option<InFlight> {
        self.finish(
            id,
            MediaOpReply::Err(MediaOpError {
                code,
                message: message.into(),
            }),
        )
    }
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
        let mut ids = BTreeSet::new();
        if dut
            .media
            .devices
            .iter()
            .any(|device| !ids.insert(device.id))
        {
            return Err(crate::i18n::label("captureDuplicateId").to_owned());
        }
        for source in Source::sources(&dut.media) {
            let input = binding(dut, source);
            if !input.is_enabled() {
                continue;
            }
            if !input.has_video() {
                return Err(crate::i18n::message(
                    "captureDutNeedsVideo",
                    &[
                        ("dut", &dut.name),
                        ("source", source.named_label(&dut.media)),
                    ],
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
        .filter(|media| media.is_enabled())
        .map(|media| source.input(media))
        .filter(|input| input.has_video())
        .ok_or_else(|| crate::i18n::label("captureNeedsVideo").to_owned())?;
    validate_video_binding(duts, dut_id, source, input)?;
    Ok(input)
}

fn validate_video_binding(
    duts: &DutsState,
    dut_id: &str,
    source: Source,
    input: &CaptureInput,
) -> Result<(), String> {
    let video = video_identity(&input.video);
    for (other_id, other_media) in duts.saved_media_bindings() {
        for other_source in Source::sources(other_media) {
            let other = other_source.input(other_media);
            if (other_id, other_source) != (dut_id, source)
                && other.has_video()
                && video_identity(&other.video) == video
            {
                return Err(duplicate_video(&input.video));
            }
        }
    }
    Ok(())
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
            for source in Source::sources(saved) {
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

    fn open_connection_preview(
        &mut self,
        duts: &DutsState,
        dut: &DutConfig,
        source: Source,
    ) -> Result<(), String> {
        let input = binding(dut, source);
        if !dut.media.is_enabled() || !input.has_video() {
            return Err(crate::i18n::label("captureNeedsVideo").into());
        }
        validate_video_binding(duts, &dut.id, source, input)?;
        let key = (dut.id.clone(), source);
        let title = format!("{} · {}", dut.name, source.named_label(&dut.media));
        if let Some(capture) = self.sessions.get_mut(&key)
            && capture.input.same_capture(input)
            && !capture.has_ended()
        {
            capture.preview(&self.capture, &title)?;
            self.acquire_hold_lease(&key, HoldKind::ExternalPreview);
            return Ok(());
        }
        self.draft_preview = None;
        let mut settings = self.capture.clone();
        settings.listen_ip = "127.0.0.1".into();
        let mut input = input.clone();
        input.port.clear();
        let capture = self.start_bound_capture(duts, &key, &input, &settings)?;
        self.draft_preview = Some(DraftPreview {
            capture,
            title,
            opened: false,
            started: Instant::now(),
        });
        Ok(())
    }

    fn update_draft_preview(&mut self) -> bool {
        let Some(preview) = self.draft_preview.as_mut() else {
            return false;
        };
        if preview.capture.has_ended()
            || (!preview.opened && preview.started.elapsed() > Duration::from_secs(10))
        {
            self.output = preview.capture.status();
        } else if preview.opened {
            if preview.capture.preview_is_open() {
                return false;
            }
        } else if !preview.capture.ready() {
            return false;
        } else {
            match preview.capture.preview(&self.capture, &preview.title) {
                Ok(()) => {
                    preview.opened = true;
                    return false;
                }
                Err(error) => self.output = error,
            }
        }
        self.draft_preview = None;
        true
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
            let preview = capture.inline_preview()?;
            // Reuse and create both hold the same idempotent Monitor lease
            // (§4.4); a repeat preview never stacks a second one.
            self.acquire_monitor_lease(&key);
            return Ok((preview, false));
        }
        self.draft_preview = None;
        self.stop_capture(&key);
        let capture = self.start_bound_capture(duts, &key, input, settings)?;
        let preview = capture.inline_preview()?;
        self.sessions.insert(key.clone(), capture);
        self.acquire_monitor_lease(&key);
        Ok((preview, true))
    }

    /// Release the side-panel `Monitor` lease, then stop only when the ledger
    /// says nothing else holds the capture (§4.4). A recording or a `Manual`
    /// pin keeps it alive; an Agent-opened session starts its idle window.
    pub fn close_inline_capture(&mut self, dut_id: &str, source: Source) {
        let key = (dut_id.to_owned(), source);
        if let Some(lease) = self.monitor_leases.remove(&key)
            && let Some(entry) = self.demand.get_mut(&key)
        {
            entry.release(lease);
        }
        let now = Instant::now();
        let idle = effective_idle(&self.capture);
        let decision = self.demand.get(&key).map(|entry| entry.decide(now, idle));
        let stop = match decision {
            Some(ReclaimDecision::StopNow) => true,
            Some(_) => false,
            // Untracked session (legacy/test insert): keep today's rule.
            None => self
                .sessions
                .get(&key)
                .is_some_and(|capture| !capture.is_recording()),
        };
        if stop {
            self.stop_capture(&key);
        }
    }

    pub fn new(dir: PathBuf) -> Self {
        let (sender, messages) = mpsc::channel();
        let (devices_tx, devices_rx) = mpsc::channel();
        Self {
            urls: Default::default(),
            dir,
            capture: CaptureSettings::default(),
            quota: None,
            ops: MediaOpPort::new(),
            demand: BTreeMap::new(),
            next_lease: 1,
            manual_leases: BTreeMap::new(),
            monitor_leases: BTreeMap::new(),
            hold_leases: BTreeMap::new(),
            recordings: BTreeMap::new(),
            aborted: BTreeMap::new(),
            orphan_results: BTreeSet::new(),
            next_recording: 1,
            recording_seconds: 0,
            sessions: BTreeMap::new(),
            draft_preview: None,
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
            collapsed_sources: BTreeSet::new(),
        }
    }

    /// Replace the resolved capture settings (`docs/design/CAPTURE_STREAM_BACKENDS.md`
    /// §3.7). The caller resolves `ffmpeg` through `tool_resolver` first. A
    /// running `Capture` keeps its process, so the new path applies only to
    /// captures started afterwards; `idle_secs` is read per tick.
    pub fn set_capture_settings(&mut self, capture: &CaptureSettings) {
        self.capture = capture.clone();
    }

    /// The resolved settings every capture start must use.
    pub fn capture_settings(&self) -> &CaptureSettings {
        &self.capture
    }

    /// Share the app-wide artifact quota (§3.7). Agent ops refuse to run until
    /// it is injected rather than writing outside quota accounting.
    pub fn set_artifact_quota(&mut self, quota: Arc<DutQuota>) {
        self.quota = Some(quota);
    }

    /// Clone-able submit end for `RuntimeMcpState` (§5.5).
    pub fn media_op_submitter(&self) -> MediaOpSubmitter {
        self.ops.submitter()
    }

    /// Active Agent recording for one binding: `(recordingId, elapsed secs)`.
    /// The manual record/stop-recording buttons stay hidden while this is
    /// `Some` because both sources share the single `Capture::recording` slot.
    pub fn agent_recording(&self, dut_id: &str, source: Source) -> Option<(RecordingId, u64)> {
        self.recordings
            .iter()
            .find(|(_, rec)| rec.key.0 == dut_id && rec.key.1 == source)
            .map(|(id, rec)| (*id, rec.started_at.elapsed().as_secs()))
    }

    /// The single finalization path (§4.5): panel stop, binding reconcile, DUT
    /// deletion and idle reclaim all end up here, so in-flight ops are rejected
    /// **before** the `Capture` drops (its `Drop` joins the recording thread but
    /// discards the last segment path).
    fn stop_capture(&mut self, key: &(String, Source)) {
        // 1. In-flight snapshot/recordStart: MEDIA_CAPTURE_ENDED. A recordStop
        //    already waiting for its last segment gets MEDIA_RECORDING_ENDED,
        //    because that segment can no longer be handed back.
        let pending = self.ops.ids_for_key(key);
        for id in pending {
            let kind = self.ops.in_flight.get(&id).map(|entry| entry.kind);
            let code = if matches!(kind, Some(InFlightKind::RecordStop(_))) {
                MEDIA_RECORDING_ENDED
            } else {
                MEDIA_CAPTURE_ENDED
            };
            if let Some(entry) = self.ops.fail(id, code, "capture stopped")
                && let Some(lease) = entry.lease
            {
                self.release_lease(lease);
            }
        }
        // 2. Active Agent recordings on this binding are interrupted: the id is
        //    tombstoned so a later recordStop answers MEDIA_RECORDING_ENDED.
        let recording_ids: Vec<RecordingId> = self
            .recordings
            .iter()
            .filter(|(_, rec)| &rec.key == key)
            .map(|(id, _)| *id)
            .collect();
        for id in recording_ids {
            if let Some(rec) = self.recordings.remove(&id) {
                self.aborted.insert(id, Instant::now());
                self.release_lease(rec.lease);
            }
            if let Some(entry) = self.demand.get_mut(key) {
                entry.agent_recording = None;
            }
        }
        // 3. Stop the recording thread without waiting; `Drop` joins it and
        //    discards the final path, so an interrupted recording is always a
        //    failure and never a completed artifact.
        if let Some(capture) = self.sessions.get(key) {
            capture.stop_recording();
        }
        // 4. Revoke the URL and drop the process machinery.
        {
            let mut urls = self.urls.lock().unwrap();
            if let Some(sources) = urls.get_mut(&key.0) {
                sources.remove(key.1.key().as_ref());
                if sources.is_empty() {
                    urls.remove(&key.0);
                }
            }
        }
        self.sessions.remove(key);
        self.demand.remove(key);
        // Lease ids die with the entry; drop the per-source registries so a
        // later preview/start acquires fresh ids (§4.5).
        self.manual_leases.remove(key);
        self.monitor_leases.remove(key);
        self.hold_leases.retain(|(held, _), _| held != key);
        self.orphan_results.remove(key);
    }

    pub fn reconcile_bindings(&mut self, duts: &DutsState) {
        let stale: Vec<_> = self
            .sessions
            .iter()
            .filter(|(key, capture)| {
                !saved_capture(duts, &key.0, key.1)
                    .is_ok_and(|input| input.same_capture(&capture.input))
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.stop_capture(&key);
        }
    }

    pub fn tick(&mut self, duts: &DutsState) -> bool {
        self.reconcile_bindings(duts);
        // MCP op bridge (§5.5): take requests, advance pending starts/jobs,
        // expire post-stop replies and mirror the recording state. There is no
        // recording-duration timer here: a recording ends only through
        // `recordStop` or `stop_capture`.
        let mut changed = self.update_draft_preview();
        changed |= self.drain_media_ops(duts);
        changed |= self.advance_media_ops(duts);
        changed |= self.sweep_recording_stops();
        self.mirror_recording_state();
        // Ledger (§4.2/§4.5): expire reader holds, then reclaim only what the
        // decision table says is finished. A freshly stamped idle clock yields
        // Keep, so default settings never auto-stop a Manual or Monitor
        // session — only Agent-opened (ephemeral) sessions are reclaimed.
        let now = Instant::now();
        for entry in self.demand.values_mut() {
            entry.sweep_expired(now);
        }
        let idle = effective_idle(&self.capture);
        let reclaim: Vec<(String, Source)> = self
            .demand
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry.decide(now, idle),
                    ReclaimDecision::Reclaim | ReclaimDecision::StopNow
                )
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in reclaim {
            self.stop_capture(&key);
            changed = true;
        }
        let statuses: BTreeMap<_, _> = self
            .sessions
            .iter()
            .map(|(key, capture)| (key.clone(), capture.status()))
            .collect();
        if statuses != self.statuses {
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
            changed = true;
        }
        // Collect first, then route: an Agent recording's final segment belongs
        // to its `recordStop` reply, never to the panel message (§5.6).
        let mut finished = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            if let Some(result) = session.poll_recording() {
                finished.push((key.clone(), result));
            }
        }
        for (key, result) in finished {
            if !self.finish_recording(&key, &result) {
                self.output = match result {
                    Ok(path) => crate::i18n::message(
                        "mediaRecordingSaved",
                        &[("path", &path.display().to_string())],
                    ),
                    Err(err) => err,
                };
            }
            changed = true;
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
        // Repaint the "Agent recording · Ns" status once per elapsed second.
        let seconds = self
            .recordings
            .values()
            .map(|rec| rec.started_at.elapsed().as_secs())
            .max()
            .unwrap_or(0);
        if seconds != self.recording_seconds {
            self.recording_seconds = seconds;
            changed = true;
        }
        changed
    }

    // ------------------------------------------------------------------
    // MCP op bridge (§5.5/§5.6)
    // ------------------------------------------------------------------

    /// Take queued requests without blocking; one op per `(dut, source)`,
    /// `MAX_IN_FLIGHT` overall. `pick_source` reads only saved bindings.
    fn drain_media_ops(&mut self, duts: &DutsState) -> bool {
        let mut changed = false;
        while let Some(request) = self.ops.try_recv_request() {
            changed = true;
            if self.ops.in_flight.len() >= MAX_IN_FLIGHT {
                let _ = request.reply.send(MediaOpReply::Err(MediaOpError {
                    code: MEDIA_BUSY,
                    message: "capture operation queue is full".into(),
                }));
                continue;
            }
            let MediaOpRequest {
                id,
                dut_id,
                source,
                op,
                reply,
            } = request;
            match op {
                MediaOp::RecordStop { recording_id } => {
                    self.begin_record_stop(id, dut_id, recording_id, reply);
                }
                MediaOp::Snapshot { note } => {
                    self.begin_capture_op(
                        duts,
                        id,
                        dut_id,
                        source,
                        note,
                        InFlightKind::Snapshot,
                        reply,
                    );
                }
                MediaOp::RecordStart { note } => {
                    self.begin_capture_op(
                        duts,
                        id,
                        dut_id,
                        source,
                        note,
                        InFlightKind::RecordStart,
                        reply,
                    );
                }
            }
        }
        changed
    }

    /// Validate one snapshot/recordStart request and park it until the capture
    /// is ready (or the start bound expires).
    #[allow(clippy::too_many_arguments)]
    fn begin_capture_op(
        &mut self,
        duts: &DutsState,
        id: OpId,
        dut_id: String,
        source: Option<String>,
        note: Option<String>,
        kind: InFlightKind,
        reply: tokio::sync::oneshot::Sender<MediaOpReply>,
    ) {
        let source = match Self::pick_source(duts, &dut_id, source.as_deref()) {
            Ok(source) => source,
            Err(failure) => {
                reply_error(reply, failure.code, failure.message);
                return;
            }
        };
        let key = (dut_id, source);
        if self.ops.op_for_key(&key).is_some() {
            reply_error(
                reply,
                MEDIA_BUSY,
                "capture source already has an active operation",
            );
            return;
        }
        if matches!(kind, InFlightKind::RecordStart)
            && self.recordings.values().any(|rec| rec.key == key)
        {
            reply_error(
                reply,
                MEDIA_RECORDING_IN_PROGRESS,
                "capture source is already recording",
            );
            return;
        }
        let created = match self.ensure_session(duts, &key) {
            Ok(created) => created,
            Err(failure) => {
                reply_error(reply, failure.code, failure.message);
                return;
            }
        };
        let lease = self.acquire_agent_lease(&key, created);
        let now = Instant::now();
        self.ops.insert(
            id,
            InFlight {
                key,
                kind,
                queued_at: now,
                started_at: None,
                lease: Some(lease),
                job_deadline: None,
                note,
                reply: Some(reply),
            },
        );
    }

    /// `pick_source` (bin-side, §5.2): requested source if saved-bound, else
    /// `hdmi`, else `camera`; never reads the selected settings item or drafts.
    fn pick_source(
        duts: &DutsState,
        dut_id: &str,
        requested: Option<&str>,
    ) -> Result<Source, OpFailure> {
        let missing = || OpFailure::new(MEDIA_NOT_BOUND, crate::i18n::label("captureNeedsVideo"));
        let media = duts.saved_media(dut_id).ok_or_else(missing)?;
        let mut bound = Source::sources(media).filter(|source| source.input(media).has_video());
        if let Some(requested) = requested.map(str::trim).filter(|name| !name.is_empty())
            && let Some(source) = Source::sources(media)
                .filter(|source| source.input(media).has_video())
                .find(|source| source.key().eq_ignore_ascii_case(requested))
        {
            return Ok(source);
        }
        bound.next().ok_or_else(missing)
    }

    /// Reuse the live session or start an ephemeral one from saved bindings.
    /// Returns whether this call created the session (only a created session is
    /// marked ephemeral).
    fn ensure_session(
        &mut self,
        duts: &DutsState,
        key: &(String, Source),
    ) -> Result<bool, OpFailure> {
        if self
            .sessions
            .get(key)
            .is_some_and(|capture| !capture.has_ended())
        {
            return Ok(false);
        }
        self.stop_capture(key);
        let input = saved_capture(duts, &key.0, key.1)
            .map_err(|error| OpFailure::new(MEDIA_NOT_BOUND, error))?;
        let settings = self.capture.clone();
        let capture = self
            .start_bound_capture(duts, key, input, &settings)
            .map_err(|error| OpFailure::new(MEDIA_START_FAILED, error))?;
        self.sessions.insert(key.clone(), capture);
        Ok(true)
    }

    /// Take one ledger lease. `ephemeral` is used only when the entry does not
    /// exist yet: an Agent-created session is ephemeral, everything else is
    /// interactive (`Manual`/`Monitor` keep today's stop behavior).
    fn acquire_lease(
        &mut self,
        key: &(String, Source),
        kind: DemandKind,
        ephemeral: bool,
    ) -> LeaseId {
        let id = self.next_lease;
        self.next_lease += 1;
        let entry = self
            .demand
            .entry(key.clone())
            .or_insert_with(|| DemandEntry::new(ephemeral));
        entry.acquire(id, kind, Instant::now());
        id
    }

    /// Panel "start capture" pin: one Manual lease per binding (idempotent).
    #[cfg(test)]
    fn acquire_manual_lease(&mut self, key: &(String, Source)) {
        if self.manual_leases.contains_key(key) {
            return;
        }
        let lease = self.acquire_lease(key, DemandKind::Manual, false);
        self.manual_leases.insert(key.clone(), lease);
    }

    /// Side-panel monitor lease: one per binding, repeated previews reuse it.
    fn acquire_monitor_lease(&mut self, key: &(String, Source)) {
        if self.monitor_leases.contains_key(key) {
            return;
        }
        let lease = self.acquire_lease(key, DemandKind::Monitor, false);
        self.monitor_leases.insert(key.clone(), lease);
    }

    /// Reader hold (§4.6): refreshed on every preview/copy so a lost consumer
    /// can never pin the capture forever. `HOLD_SECS` is the only expiry.
    fn acquire_hold_lease(&mut self, key: &(String, Source), hold: HoldKind) {
        let now = Instant::now();
        let kind = hold.demand();
        if let Some(lease) = self.hold_leases.get(&(key.clone(), hold)).copied() {
            if let Some(entry) = self.demand.get_mut(key) {
                entry.acquire_hold(lease, kind, now, HOLD_SECS);
            }
            return;
        }
        let lease = self.next_lease;
        self.next_lease += 1;
        let entry = self
            .demand
            .entry(key.clone())
            .or_insert_with(|| DemandEntry::new(false));
        entry.acquire_hold(lease, kind, now, HOLD_SECS);
        self.hold_leases.insert((key.clone(), hold), lease);
    }

    fn acquire_agent_lease(&mut self, key: &(String, Source), ephemeral: bool) -> LeaseId {
        self.acquire_lease(key, DemandKind::AgentOp, ephemeral)
    }

    fn release_lease(&mut self, lease: LeaseId) {
        for entry in self.demand.values_mut() {
            entry.release(lease);
        }
    }

    /// Mirror "this capture is recording" into the ledger; a recording is a
    /// state of the `Capture`, not a lease (§4.2).
    fn mirror_recording_state(&mut self) {
        for (key, entry) in self.demand.iter_mut() {
            entry.recording = self.sessions.get(key).is_some_and(Capture::is_recording);
        }
    }

    /// Worker results first, then readiness/start-bound, then job watchdogs.
    /// Every failure ends the op only; the capture keeps running.
    fn advance_media_ops(&mut self, duts: &DutsState) -> bool {
        let now = Instant::now();
        let mut changed = false;
        while let Ok(finished) = self.ops.done_rx.try_recv() {
            changed = true;
            if let Some(entry) = self.ops.finish(finished.id, finished.reply)
                && let Some(lease) = entry.lease
            {
                self.release_lease(lease);
            }
        }
        let pending: Vec<OpId> = self
            .ops
            .in_flight
            .iter()
            .filter(|(_, entry)| {
                entry.started_at.is_none()
                    && matches!(
                        entry.kind,
                        InFlightKind::Snapshot | InFlightKind::RecordStart
                    )
            })
            .map(|(id, _)| *id)
            .collect();
        for id in pending {
            let Some(entry) = self.ops.in_flight.get(&id) else {
                continue;
            };
            let key = entry.key.clone();
            let kind = entry.kind;
            let queued_at = entry.queued_at;
            let ended = self.sessions.get(&key).is_none_or(Capture::has_ended);
            if self.sessions.get(&key).is_some_and(Capture::ready) {
                if matches!(kind, InFlightKind::Snapshot) {
                    changed |= self.start_snapshot_job(id, duts, now);
                } else {
                    changed |= self.start_recording_op(id, duts, now);
                }
            } else if ended {
                self.fail_op(id, MEDIA_CAPTURE_ENDED, "capture ended");
                changed = true;
            } else if now.duration_since(queued_at) >= START_TIMEOUT {
                // Re-check readiness once: a capture that just became ready
                // still gets its op; otherwise fail this op only and leave the
                // session to idle reclaim (§5.5).
                if self.sessions.get(&key).is_some_and(Capture::ready) {
                    continue;
                }
                self.fail_op(id, MEDIA_START_FAILED, "capture startup timed out");
                changed = true;
            }
        }
        let timed_out: Vec<OpId> = self
            .ops
            .in_flight
            .iter()
            .filter(|(_, entry)| entry.job_deadline.is_some_and(|deadline| now >= deadline))
            .map(|(id, _)| *id)
            .collect();
        for id in timed_out {
            self.fail_op(id, MEDIA_OP_TIMEOUT, "capture timed out");
            changed = true;
        }
        changed
    }

    /// Fail one in-flight op and release the AgentOp lease it still owns.
    fn fail_op(&mut self, id: OpId, code: &'static str, message: impl Into<String>) {
        if let Some(entry) = self.ops.fail(id, code, message)
            && let Some(lease) = entry.lease
        {
            self.release_lease(lease);
        }
    }

    /// Spawn the snapshot worker. The worker only sees the std `done` channel;
    /// the oneshot sender stays on the GUI thread.
    fn start_snapshot_job(&mut self, id: OpId, duts: &DutsState, now: Instant) -> bool {
        let Some(entry) = self.ops.in_flight.get(&id) else {
            return false;
        };
        let key = entry.key.clone();
        let note = entry.note.clone();
        let Some(quota) = self.quota.clone() else {
            self.fail_op(
                id,
                MEDIA_CAPTURE_UNAVAILABLE,
                "capture capability unavailable",
            );
            return true;
        };
        let Some(name) = duts.saved_name(&key.0).map(str::to_owned) else {
            self.fail_op(id, MEDIA_NOT_BOUND, "DUT artifact directory missing");
            return true;
        };
        let Some(capture) = self.sessions.get(&key) else {
            self.fail_op(id, MEDIA_CAPTURE_ENDED, "capture ended");
            return true;
        };
        let screenshot = capture.screenshot(&self.dir, &key.0, &name, quota);
        let source_key = key.1.key().into_owned();
        let done = self.ops.done_tx.clone();
        if let Some(entry) = self.ops.in_flight.get_mut(&id) {
            entry.started_at = Some(now);
            entry.job_deadline = Some(now + JOB_TIMEOUT);
            entry.note = None;
        }
        std::thread::spawn(move || {
            let reply = match screenshot() {
                Ok(path) => {
                    MediaOpReply::Artifact(snapshot_artifact(&path, note, Some(source_key)))
                }
                Err(message) => MediaOpReply::Err(MediaOpError {
                    code: MEDIA_CAPTURE_FAILED,
                    message,
                }),
            };
            let _ = done.send(OpFinished { id, reply });
        });
        true
    }

    /// Start the Agent recording: `record()` returns the first segment
    /// immediately, so the reply and the registry entry happen in this tick.
    /// The AgentOp lease moves from the op to the recording.
    fn start_recording_op(&mut self, id: OpId, duts: &DutsState, now: Instant) -> bool {
        let Some(entry) = self.ops.in_flight.get(&id) else {
            return false;
        };
        let key = entry.key.clone();
        let note = entry.note.clone();
        let Some(quota) = self.quota.clone() else {
            self.fail_op(
                id,
                MEDIA_CAPTURE_UNAVAILABLE,
                "capture capability unavailable",
            );
            return true;
        };
        let Some(name) = duts.saved_name(&key.0).map(str::to_owned) else {
            self.fail_op(id, MEDIA_NOT_BOUND, "DUT artifact directory missing");
            return true;
        };
        if self.recordings.len() >= MAX_ACTIVE_RECORDINGS
            || self.recordings.values().any(|rec| rec.key == key)
        {
            self.fail_op(
                id,
                MEDIA_RECORDING_IN_PROGRESS,
                "capture source is already recording",
            );
            return true;
        }
        if self.sessions.get(&key).is_some_and(Capture::is_recording) {
            // Manual and Agent recording share one slot; `record()` would
            // silently overwrite the running one.
            self.fail_op(
                id,
                MEDIA_RECORDING_IN_PROGRESS,
                "capture source is already recording",
            );
            return true;
        }
        let Some(capture) = self.sessions.get_mut(&key) else {
            self.fail_op(id, MEDIA_CAPTURE_ENDED, "capture ended");
            return true;
        };
        match capture.record(&self.dir, &key.0, &name, quota) {
            Ok(first_path) => {
                let recording_id = self.next_recording;
                self.next_recording += 1;
                let lease = self
                    .ops
                    .in_flight
                    .get_mut(&id)
                    .and_then(|entry| {
                        entry.started_at = Some(now);
                        entry.note = None;
                        entry.lease.take()
                    })
                    .unwrap_or(0);
                let handle = RecordingHandle {
                    recording_id,
                    source: key.1.key().into(),
                    first_path: first_path.display().to_string(),
                    started_at_ms: unix_millis(),
                    note: note.clone(),
                };
                self.recordings.insert(
                    recording_id,
                    ActiveRecording {
                        key: key.clone(),
                        note,
                        first_path,
                        started_at: Instant::now(),
                        lease,
                        stop_requested_at: None,
                    },
                );
                if let Some(entry) = self.demand.get_mut(&key) {
                    entry.agent_recording = Some(recording_id);
                }
                // Answer once; the lease now belongs to the recording and is
                // released by recordStop or stop_capture.
                self.ops.finish(id, MediaOpReply::Recording(handle));
            }
            Err(message) => self.fail_op(id, MEDIA_CAPTURE_FAILED, message),
        }
        true
    }

    /// Validate a `recordStop` (DUT ownership, one-in-flight, not already
    /// stopping) and ask the capture to flush. The last segment is delivered by
    /// the existing `poll_recording` in `tick`.
    fn begin_record_stop(
        &mut self,
        id: OpId,
        dut_id: String,
        recording_id: RecordingId,
        reply: tokio::sync::oneshot::Sender<MediaOpReply>,
    ) {
        let Some(key) = self
            .recordings
            .get(&recording_id)
            .map(|rec| rec.key.clone())
        else {
            let code = if self.aborted.contains_key(&recording_id) {
                MEDIA_RECORDING_ENDED
            } else {
                MEDIA_RECORDING_UNKNOWN
            };
            // Unknown ids, cross-DUT probes and tombstoned ids share one code
            // so a client cannot probe other DUTs (§5.2/§5.3).
            reply_error(reply, code, "recording does not exist or has ended");
            return;
        };
        if key.0 != dut_id {
            reply_error(
                reply,
                MEDIA_RECORDING_UNKNOWN,
                "recording does not exist or has ended",
            );
            return;
        }
        if self.recordings[&recording_id].stop_requested_at.is_some()
            || self.ops.op_for_key(&key).is_some()
        {
            reply_error(
                reply,
                MEDIA_BUSY,
                "only one stop request may be active per recording",
            );
            return;
        }
        if !self.sessions.get(&key).is_some_and(Capture::is_recording) {
            // The capture ended outside `stop_capture`: the registry entry can
            // never hand back a complete artifact.
            if let Some(rec) = self.recordings.remove(&recording_id) {
                self.release_lease(rec.lease);
            }
            if let Some(entry) = self.demand.get_mut(&key) {
                entry.agent_recording = None;
            }
            reply_error(reply, MEDIA_RECORDING_ENDED, "recording has ended");
            return;
        }
        if let Some(capture) = self.sessions.get_mut(&key) {
            capture.stop_recording();
        }
        let now = Instant::now();
        if let Some(rec) = self.recordings.get_mut(&recording_id) {
            rec.stop_requested_at = Some(now);
        }
        self.ops.insert(
            id,
            InFlight {
                key,
                kind: InFlightKind::RecordStop(recording_id),
                queued_at: now,
                started_at: Some(now),
                lease: None,
                job_deadline: None,
                note: None,
                reply: Some(reply),
            },
        );
    }

    /// Route one `poll_recording` result: an Agent recording's outcome becomes
    /// the single `recordStop` reply; anything else stays a panel message.
    /// Returns `true` when the result belonged to the Agent bridge.
    fn finish_recording(
        &mut self,
        key: &(String, Source),
        result: &Result<PathBuf, String>,
    ) -> bool {
        // RECORD_STOP_GRACE already ended the op: discard the late result so it
        // cannot leak into the panel message (§5.5).
        if self.orphan_results.remove(key) {
            return true;
        }
        let Some(id) = self
            .recordings
            .iter()
            .find(|(_, rec)| &rec.key == key)
            .map(|(id, _)| *id)
        else {
            return false;
        };
        let Some(rec) = self.recordings.remove(&id) else {
            return false;
        };
        let stop_requested = rec.stop_requested_at.is_some();
        let reply = match result {
            // Only "recordStop arrived first, then the last segment" is success.
            Ok(last) if stop_requested => MediaOpReply::Artifact(MediaArtifact {
                path: last.display().to_string(),
                first_path: Some(rec.first_path.display().to_string()),
                rotated: &rec.first_path != last,
                bytes: artifact_bytes(last).unwrap_or(0),
                width: None,
                height: None,
                duration_secs: Some(rec.started_at.elapsed().as_secs()),
                note: rec.note.clone(),
                source: Some(key.1.key().into()),
            }),
            Ok(_) => MediaOpReply::Err(MediaOpError {
                code: MEDIA_RECORDING_ENDED,
                message: "capture ended".into(),
            }),
            Err(message) => MediaOpReply::Err(MediaOpError {
                code: MEDIA_CAPTURE_FAILED,
                message: message.clone(),
            }),
        };
        if let Some(entry) = self.demand.get_mut(key) {
            entry.agent_recording = None;
        }
        if let Some(op) = self.ops.op_by_kind(InFlightKind::RecordStop(id)) {
            self.ops.finish(op, reply);
        }
        self.release_lease(rec.lease);
        true
    }

    /// Post-`recordStop` reply bound (5s): a stuck recording thread must not
    /// leave the MCP call hanging. This ends the op and the registry entry only;
    /// it never stops the capture and is **not** a recording length limit.
    fn sweep_recording_stops(&mut self) -> bool {
        self.aborted.retain(|_, at| at.elapsed() < ABORTED_TTL);
        let now = Instant::now();
        let stranded: Vec<RecordingId> = self
            .recordings
            .iter()
            .filter(|(_, rec)| {
                rec.stop_requested_at
                    .is_some_and(|started| now.duration_since(started) >= RECORD_STOP_GRACE)
            })
            .map(|(id, _)| *id)
            .collect();
        let changed = !stranded.is_empty();
        for id in stranded {
            let Some(rec) = self.recordings.remove(&id) else {
                continue;
            };
            if let Some(entry) = self.demand.get_mut(&rec.key) {
                entry.agent_recording = None;
            }
            if let Some(op) = self.ops.op_by_kind(InFlightKind::RecordStop(id)) {
                self.fail_op(op, MEDIA_OP_TIMEOUT, "capture timed out");
            }
            self.release_lease(rec.lease);
            // The thread may still flush a last segment: discard that result.
            self.orphan_results.insert(rec.key);
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

/// Send one error reply to a request that never entered the in-flight table.
fn reply_error(
    reply: tokio::sync::oneshot::Sender<MediaOpReply>,
    code: &'static str,
    message: impl Into<String>,
) {
    let _ = reply.send(MediaOpReply::Err(MediaOpError {
        code,
        message: message.into(),
    }));
}

/// Wall-clock milliseconds since the UNIX epoch (`startedAtMs`).
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(0))
}

/// Build a snapshot artifact from the PNG on disk; unreadable dimensions stay
/// `None` instead of being invented (§5.4).
fn snapshot_artifact(path: &Path, note: Option<String>, source: Option<String>) -> MediaArtifact {
    let (width, height) = png_size(path);
    MediaArtifact {
        path: path.display().to_string(),
        first_path: None,
        rotated: false,
        bytes: artifact_bytes(path).unwrap_or(0),
        width,
        height,
        duration_secs: None,
        note,
        source,
    }
}

/// PNG IHDR dimensions from the first 24 bytes; `(None, None)` otherwise.
fn png_size(path: &Path) -> (Option<u32>, Option<u32>) {
    use std::io::Read;
    let mut header = [0u8; 24];
    let Ok(mut file) = std::fs::File::open(path) else {
        return (None, None);
    };
    if file.read_exact(&mut header).is_err() {
        return (None, None);
    }
    png_dimensions(&header).map_or((None, None), |(width, height)| (Some(width), Some(height)))
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
                .child(idle_field(w, p, window, cx))
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
                                // The panel starts captures from `MediaView`'s
                                // resolved copy, so an edited ffmpeg path only
                                // takes effect after it is re-injected.
                                this.refresh_capture_settings();
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

/// Idle-reclaim seconds (`tools.capture.idleSecs`, §3.5): a text draft that can
/// be cleared (empty = use the default), committed by its own save button so a
/// typo never blocks the ffmpeg/listen settings from saving.
fn idle_field(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    let draft = w.input_value(In::CaptureIdle);
    field(
        crate::i18n::label("captureIdle"),
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().w(px(120.)).child(w.text_field(
                In::CaptureIdle,
                &draft,
                "60",
                true,
                p,
                window,
                cx,
            )))
            .child(
                btn(
                    "save-capture-idle",
                    crate::i18n::label("save"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.save_capture_idle();
                    cx.notify();
                })),
            ),
        Some(crate::i18n::label("captureIdleHint")),
        p,
    )
}

pub(crate) fn render_bindings(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let enabled = dut.media.is_enabled();
    let dut_id = dut.id.clone();
    let enable = toggle("capture-devices-enabled", enabled, p).on_click(cx.listener(
        move |w, checked: &bool, _, cx| {
            if let Some(dut) = w.duts.connection_mut(&dut_id) {
                dut.media.enabled = Some(*checked);
                if *checked && Source::configured_sources(&dut.media).next().is_none() {
                    add_capture_device(&mut dut.media);
                }
                w.duts.mark_connection_dirty();
                w.open_dd = None;
                cx.notify();
            }
        },
    ));
    let dut_id = dut.id.clone();
    let controls = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .when(enabled, |controls| {
            controls.child(
                btn(
                    "add-capture-device",
                    crate::i18n::label("addCaptureDevice"),
                    &BtnVariant::Tiny,
                    p,
                )
                .icon(crate::icons::kit_icon("plus").size(px(14.)))
                .on_click(cx.listener(move |w, _, _, cx| {
                    if let Some(dut) = w.duts.connection_mut(&dut_id)
                        && dut.media.is_enabled()
                    {
                        add_capture_device(&mut dut.media);
                        w.duts.mark_connection_dirty();
                        cx.notify();
                    }
                })),
            )
        })
        .child(enable);
    let header = panel_head("eye", crate::i18n::label("captureDevicesTitle"), p).child(controls);
    if !enabled {
        return crate::pages::collapsible_panel(header, None, p);
    }
    let mut body = div().flex().flex_col().gap(px(12.));
    let sources: Vec<_> = Source::configured_sources(&dut.media).collect();
    for &source in &sources {
        body = body.child(div().id(format!("capture-device-{}", source.key())).child(
            source_panel(w, &dut, source, sources.len() > 1, p, window, cx),
        ));
    }
    if !w.media.output.is_empty() {
        body = body.child(
            div()
                .text_size(px(12.))
                .text_color(p.muted)
                .child(w.media.output.clone()),
        );
    }
    crate::pages::collapsible_panel(header, Some(body), p)
}

fn add_capture_device(media: &mut duthub_core::model::DutMediaConfig) {
    let id = loop {
        let id = uuid::Uuid::new_v4().as_u128() as u32;
        if id != 0 && !media.devices.iter().any(|device| device.id == id) {
            break id;
        }
    };
    media.devices.push(duthub_core::model::CaptureDevice {
        id,
        input: CaptureInput::default(),
    });
}

#[derive(Debug)]
pub(crate) struct VideoDevice {
    pub name: String,
    pub endpoint: Option<String>,
}

pub(crate) fn directshow_video_devices(output: &str) -> Vec<VideoDevice> {
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

pub(crate) fn detected_devices(output: &str, audio: bool) -> Vec<String> {
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

fn directshow_video_choices(output: &str) -> Vec<(String, String)> {
    let videos = directshow_video_devices(output);
    videos
        .iter()
        .enumerate()
        .filter_map(|(index, video)| {
            let same_name = videos
                .iter()
                .filter(|other| other.name.eq_ignore_ascii_case(&video.name))
                .count();
            if same_name > 1 {
                let endpoint = video.endpoint.as_ref()?;
                Some((format!("{} · {}", video.name, index + 1), endpoint.clone()))
            } else {
                Some((video.name.clone(), video.name.clone()))
            }
        })
        .collect()
}

fn directshow_video_display_name(output: &str, configured: &str) -> Option<String> {
    let configured = configured.trim();
    if configured.is_empty() {
        return None;
    }
    let videos = directshow_video_devices(output);
    videos.iter().enumerate().find_map(|(index, video)| {
        let matches_endpoint = video
            .endpoint
            .as_deref()
            .is_some_and(|id| id.eq_ignore_ascii_case(configured));
        if !matches_endpoint {
            return None;
        }
        let same_name = videos
            .iter()
            .filter(|other| other.name.eq_ignore_ascii_case(&video.name))
            .count();
        if same_name > 1 {
            Some(format!("{} · {}", video.name, index + 1))
        } else {
            Some(video.name.clone())
        }
    })
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
    let raw_value = id
        .value(&w.settings.tools.capture, w.duts.selected())
        .to_owned();
    let is_dshow_endpoint = cfg!(windows) && !audio && raw_value.trim().starts_with("@device_");
    if is_dshow_endpoint && w.media.devices.is_none() && !w.media.scanning {
        w.media.scan_devices(w.settings.tools.capture.clone());
    }
    let resolved_display = if is_dshow_endpoint {
        w.media
            .devices
            .as_ref()
            .and_then(|res| res.as_ref().ok())
            .and_then(|raw| directshow_video_display_name(raw, &raw_value))
    } else {
        None
    };
    let editable = if let Some(display) = resolved_display {
        field(
            label,
            div()
                .w_full()
                .min_w_0()
                .px(px(10.))
                .py(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(p.line)
                .bg(p.bg)
                .text_size(px(13.))
                .text_color(p.text)
                .overflow_hidden()
                .whitespace_nowrap()
                .child(display),
            None,
            p,
        )
    } else {
        text(
            w,
            id,
            label,
            crate::i18n::label("captureDeviceHint"),
            p,
            window,
            cx,
        )
    };
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
                                        directshow_video_choices(raw)
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
    if w.open_dd.as_deref() == Some(dd.key().as_ref()) {
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
    Stop,
    Screenshot,
    Record,
    StopRecording,
    Copy,
}

fn source_panel(
    w: &mut Workbench,
    dut: &DutConfig,
    source: Source,
    removable: bool,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    let key = (dut.id.clone(), source);
    let input = binding(dut, source);
    let collapsed = w.media.collapsed_sources.contains(&key);
    let toggle_key = key.clone();
    let delete_id = dut.id.clone();
    let delete_key = key.clone();
    let chevron = if collapsed {
        "chevron-right"
    } else {
        "chevron-down"
    };
    let collapsed_summary = (collapsed && !input.video.trim().is_empty()).then(|| {
        let raw_value = input.video.trim();
        if cfg!(windows) && raw_value.starts_with("@device_") {
            w.media
                .devices
                .as_ref()
                .and_then(|res| res.as_ref().ok())
                .and_then(|raw| directshow_video_display_name(raw, raw_value))
                .unwrap_or_else(|| raw_value.to_owned())
        } else {
            raw_value.to_owned()
        }
    });
    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(10.))
        .px(px(18.))
        .pt(px(15.))
        .pb(px(14.))
        .border_b_1()
        .border_color(p.line)
        .child(
            div()
                .id(SharedString::from(format!(
                    "toggle-capture-{}",
                    source.key()
                )))
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap(px(9.))
                .cursor_pointer()
                .on_click(cx.listener(move |w, _, _, cx| {
                    if !w.media.collapsed_sources.remove(&toggle_key) {
                        w.media.collapsed_sources.insert(toggle_key.clone());
                    }
                    w.open_dd = None;
                    cx.notify();
                }))
                .child(
                    crate::icons::icon(chevron)
                        .size(px(15.))
                        .text_color(p.muted),
                )
                .child(crate::icons::icon("eye").size(px(17.)).text_color(p.accent))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(13.))
                        .font_weight(gpui_kit::FontWeight(670.0))
                        .text_color(p.text)
                        .child(SharedString::from(
                            source.named_label(&dut.media).to_owned(),
                        )),
                )
                .children(collapsed_summary.map(|summary| {
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(p.muted)
                        .child(SharedString::from(summary))
                })),
        )
        .when(removable, |head| {
            head.child(
                crate::components::icon_btn(format!("remove-capture-{}", source.key()), "trash", p)
                    .tooltip(crate::i18n::label("removeCaptureDevice"))
                    .on_click(cx.listener(move |w, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(dut) = w.duts.connection_mut(&delete_id) {
                            match source {
                                Source::Device(id) => {
                                    dut.media.devices.retain(|device| device.id != id)
                                }
                                Source::Hdmi | Source::Camera => {
                                    if let Some(input) = source.input_mut(&mut dut.media) {
                                        *input = CaptureInput::default();
                                    }
                                }
                            }
                            w.media.collapsed_sources.remove(&delete_key);
                            w.duts.mark_connection_dirty();
                            w.open_dd = None;
                            cx.notify();
                        }
                    })),
            )
        });
    if collapsed {
        return crate::pages::collapsible_panel(header, None, p);
    }
    let saved = w.duts.saved_media(&dut.id).map(|media| source.input(media));
    let pending = saved.is_none_or(|saved| !saved.same_capture(input));
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
    // An Agent recording owns the single `Capture::recording` slot, so the
    // manual record/stop-recording button is neither rendered nor dispatched
    // (§5.6); "Stop capture" remains available and ends the recording.
    let agent_recording = w.media.agent_recording(&dut.id, source);
    let actions = if pending
        || readiness.is_err()
        || (capture.is_none_or(Capture::has_ended) && !recording)
    {
        Vec::new()
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
        let mut actions = vec![(
            "screenshot",
            crate::i18n::label("screenshot"),
            Action::Screenshot,
        )];
        if agent_recording.is_none() {
            actions.push(record);
        }
        actions.push(("copy", crate::i18n::label("copyStreamUrl"), Action::Copy));
        actions.push(("stop", crate::i18n::label("stopCapture"), Action::Stop));
        actions
    };
    let preview_dut = dut.id.clone();
    let mut bar = div().flex().flex_wrap().gap(px(8.)).child(
        btn(
            format!("capture-{}-preview", source.key()),
            crate::i18n::label("openCapturePreview"),
            &BtnVariant::Tiny,
            p,
        )
        .disabled(!input.has_video())
        .on_click(cx.listener(move |w, _, _, cx| {
            let Some(dut) = w.duts.selected().filter(|dut| dut.id == preview_dut) else {
                return;
            };
            if let Err(error) = w.media.open_connection_preview(&w.duts, dut, source) {
                w.media.output = error;
            }
            cx.notify();
        })),
    );
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
    let body = div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(text(
            w,
            MediaField::Name(source),
            crate::i18n::label("captureDeviceName"),
            source.label(),
            p,
            window,
            cx,
        ))
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
        .children(agent_recording.map(|(_, secs)| {
            div()
                .text_size(px(12.))
                .text_color(p.accent)
                .child(SharedString::from(crate::i18n::message(
                    "agentRecording",
                    &[("secs", &secs.to_string())],
                )))
        }))
        .children(url.map(|url| {
            div()
                .text_size(px(12.))
                .text_color(p.accent)
                .child(SharedString::from(url))
        }))
        .child(bar);
    crate::pages::collapsible_panel(header, Some(body), p)
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
    // Hard guard behind the hidden button: manual recording and an Agent
    // recording share `Capture::recording`, and `record()` would silently
    // overwrite the running one (§5.6).
    if matches!(action, Action::Record | Action::StopRecording)
        && w.media.agent_recording(&key.0, key.1).is_some()
    {
        return Err(crate::i18n::label("agentRecordingBusy").to_owned());
    }
    w.media.reconcile_bindings(&w.duts);
    let _ = saved_capture(&w.duts, &key.0, key.1)?;
    let dut = w
        .duts
        .file
        .duts
        .iter()
        .find(|dut| dut.id == key.0)
        .ok_or(crate::i18n::label("dutNotFound"))?;
    let saved_name = w
        .duts
        .saved_name(&dut.id)
        .ok_or(crate::i18n::label("dutNotFound"))?;
    let mut hold = None;
    let capture = w
        .media
        .sessions
        .get_mut(key)
        .ok_or(crate::i18n::label("startCaptureFirst"))?;
    match action {
        Action::Screenshot => {
            let screenshot =
                capture.screenshot(&w.media.dir, &dut.id, saved_name, w.artifact_quota.clone());
            let sender = w.media.sender.clone();
            std::thread::spawn(move || {
                let status = screenshot()
                    .map(|path| {
                        crate::i18n::message(
                            "mediaScreenshotSaved",
                            &[("path", &path.display().to_string())],
                        )
                    })
                    .unwrap_or_else(|e| e);
                let _ = sender.send(status);
            });
        }
        Action::Record => {
            if !capture.is_recording() {
                let path =
                    capture.record(&w.media.dir, &dut.id, saved_name, w.artifact_quota.clone())?;
                w.media.output = crate::i18n::message(
                    "mediaRecordingInProgress",
                    &[("path", &path.display().to_string())],
                );
            }
        }
        Action::StopRecording => capture.stop_recording(),
        Action::Copy => {
            copy_text(cx, &capture.url);
            // Copied URL: a self-expiring hold, refreshed per copy.
            hold = Some(HoldKind::CopyUrl);
        }
        Action::Stop => unreachable!(),
    }
    if let Some(hold) = hold {
        w.media.acquire_hold_lease(key, hold);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_settings_are_injected_and_read_back_verbatim() {
        let mut view = MediaView::new(std::env::temp_dir());
        assert_eq!(view.capture_settings(), &CaptureSettings::default());
        let injected = CaptureSettings {
            ffmpeg: "C:/tools/ffmpeg/bin/ffmpeg.exe".into(),
            idle_secs: Some(90),
            ..CaptureSettings::default()
        };
        view.set_capture_settings(&injected);
        assert_eq!(view.capture_settings(), &injected);
    }

    #[test]
    fn pick_source_uses_saved_bindings_and_falls_back_in_order() {
        let file = serde_json::from_value(serde_json::json!({
            "version": 1,
            "duts": [
                {"id": "a", "name": "A", "serial": {"host": "local", "port": "DEMO"},
                 "media": {"camera": {"video": "cam"}}},
                {"id": "b", "name": "B", "serial": {"host": "local", "port": "DEMO"},
                 "media": {"hdmi": {"video": "hdmi"}}},
                {"id": "c", "name": "C", "serial": {"host": "local", "port": "DEMO"}}
            ]
        }))
        .unwrap();
        let duts = DutsState::from_file(file);
        // Requested and bound: honored.
        assert_eq!(
            MediaView::pick_source(&duts, "a", Some("camera")).unwrap(),
            Source::Camera
        );
        // Requested but unbound: falls back to the other bound source.
        assert_eq!(
            MediaView::pick_source(&duts, "a", Some("hdmi")).unwrap(),
            Source::Camera
        );
        // No request: hdmi first when bound.
        assert_eq!(
            MediaView::pick_source(&duts, "b", None).unwrap(),
            Source::Hdmi
        );
        // Unknown source names never invent a binding.
        assert_eq!(
            MediaView::pick_source(&duts, "b", Some("display")).unwrap(),
            Source::Hdmi
        );
        // No saved video: MEDIA_NOT_BOUND.
        let failure = MediaView::pick_source(&duts, "c", None).unwrap_err();
        assert_eq!(failure.code, MEDIA_NOT_BOUND);
    }

    #[test]
    fn stop_capture_rejects_in_flight_ops_with_capture_ended() {
        let mut view = MediaView::new(std::env::temp_dir());
        let key = ("dut".to_string(), Source::Hdmi);
        let (reply, mut receiver) = tokio::sync::oneshot::channel();
        view.ops.insert(
            1,
            InFlight {
                key: key.clone(),
                kind: InFlightKind::Snapshot,
                queued_at: Instant::now(),
                started_at: None,
                lease: Some(7),
                job_deadline: None,
                note: None,
                reply: Some(reply),
            },
        );
        view.stop_capture(&key);
        match receiver.try_recv() {
            Ok(MediaOpReply::Err(error)) => assert_eq!(error.code, MEDIA_CAPTURE_ENDED),
            other => panic!("expected MEDIA_CAPTURE_ENDED, got {other:?}"),
        }
        assert!(view.ops.in_flight.is_empty());
        assert!(view.demand.is_empty());
    }

    #[test]
    fn record_stop_for_an_unknown_id_answers_recording_unknown() {
        let mut view = MediaView::new(std::env::temp_dir());
        let submitter = view.media_op_submitter();
        let mut receiver = submitter.submit(
            "dut".to_string(),
            None,
            MediaOp::RecordStop { recording_id: 9 },
        );
        assert!(view.tick(&DutsState::empty()));
        match receiver.try_recv() {
            Ok(MediaOpReply::Err(error)) => assert_eq!(error.code, MEDIA_RECORDING_UNKNOWN),
            other => panic!("expected MEDIA_RECORDING_UNKNOWN, got {other:?}"),
        }
        assert!(view.recordings.is_empty());
    }

    #[test]
    fn monitor_lease_is_idempotent_and_close_releases_it() {
        let mut view = MediaView::new(std::env::temp_dir());
        let key = ("dut".to_string(), Source::Hdmi);
        view.acquire_monitor_lease(&key);
        view.acquire_monitor_lease(&key);
        assert_eq!(view.monitor_leases.len(), 1, "repeat previews never stack");
        view.close_inline_capture("dut", Source::Hdmi);
        // Monitor-created interaction: last lease released -> StopNow.
        assert!(view.monitor_leases.is_empty());
        assert!(!view.demand.contains_key(&key));
    }

    #[test]
    fn manual_pin_survives_monitor_close_and_idle() {
        let mut view = MediaView::new(std::env::temp_dir());
        view.set_capture_settings(&CaptureSettings {
            idle_secs: Some(1),
            ..CaptureSettings::default()
        });
        let key = ("dut".to_string(), Source::Hdmi);
        view.acquire_manual_lease(&key);
        view.acquire_manual_lease(&key);
        assert_eq!(view.manual_leases.len(), 1);
        view.acquire_monitor_lease(&key);
        view.close_inline_capture("dut", Source::Hdmi);
        assert!(view.demand.contains_key(&key), "Manual pin keeps it alive");
        std::thread::sleep(Duration::from_millis(1100));
        view.tick(&DutsState::empty());
        assert!(
            view.demand.contains_key(&key),
            "idle reclaim never touches a pinned session"
        );
    }

    #[test]
    fn ephemeral_session_is_reclaimed_after_the_idle_window() {
        let mut view = MediaView::new(std::env::temp_dir());
        view.set_capture_settings(&CaptureSettings {
            idle_secs: Some(1),
            ..CaptureSettings::default()
        });
        let key = ("dut".to_string(), Source::Hdmi);
        let lease = view.acquire_agent_lease(&key, true);
        view.release_lease(lease);
        view.tick(&DutsState::empty());
        assert!(
            view.demand.contains_key(&key),
            "idle clock starts on release"
        );
        std::thread::sleep(Duration::from_millis(1100));
        assert!(view.tick(&DutsState::empty()));
        assert!(
            !view.demand.contains_key(&key),
            "ephemeral capture is reclaimed after idleSecs"
        );
    }

    #[test]
    fn copy_hold_is_refreshed_and_its_expiry_finishes_the_session() {
        let mut view = MediaView::new(std::env::temp_dir());
        let key = ("dut".to_string(), Source::Hdmi);
        view.acquire_hold_lease(&key, HoldKind::CopyUrl);
        view.acquire_hold_lease(&key, HoldKind::CopyUrl);
        assert_eq!(view.hold_leases.len(), 1, "a repeat copy re-arms one hold");
        // Simulate the 300s hold expiring: with no other lease the interactive
        // session is finished on the next tick.
        let lease = view.hold_leases[&(key.clone(), HoldKind::CopyUrl)];
        view.release_lease(lease);
        assert!(view.tick(&DutsState::empty()));
        assert!(!view.demand.contains_key(&key));
    }

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
    fn named_capture_devices_are_addressable_by_stable_key() {
        let file = serde_json::from_value(serde_json::json!({
            "version": 1,
            "duts": [{"id": "a", "name": "A", "serial": {"host": "local", "port": "DEMO"},
             "media": {
                 "hdmi": {"video": "legacy"},
                 "devices": [{"id": 7, "name": "Front", "video": "front"},
                             {"id": 11, "name": "Rear", "video": "rear"}]}}
            ]
        }))
        .unwrap();
        let mut state = DutsState::from_file(file);
        let media = state.saved_media("a").expect("saved bindings");
        let picked = MediaView::pick_source(&state, "a", Some("capture-11")).ok();
        assert_eq!(picked, Some(Source::Device(11)));
        assert_eq!(Source::Device(7).named_label(media), "Front");
        assert!(validate_bindings(&state.file.duts).is_ok());
        assert_eq!(Source::sources(media).count(), 3);
        state.file.duts[0].media.enabled = Some(false);
        state.sync_saved_media();
        assert!(saved_capture(&state, "a", Source::Device(7)).is_err());
        let media = state.saved_media("a").expect("saved bindings");
        assert_eq!(Source::configured_sources(media).count(), 3);
        assert_eq!(Source::sources(media).count(), 0);
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
    fn directshow_video_choices_prefer_friendly_name_and_resolve_saved_endpoints() {
        let raw = r#"
[in#0] "USB Capture" (video)
[in#0] Alternative name "@device_pnp_capture_one"
[in#0] "USB Capture" (video)
[in#0] Alternative name "@device_pnp_capture_two"
[in#0] "UGREEN 35866" (video)
[in#0] Alternative name "@device_pnp_ugreen"
"#;
        assert_eq!(
            directshow_video_choices(raw),
            vec![
                ("USB Capture · 1".into(), "@device_pnp_capture_one".into()),
                ("USB Capture · 2".into(), "@device_pnp_capture_two".into()),
                ("UGREEN 35866".into(), "UGREEN 35866".into()),
            ]
        );
        assert_eq!(
            directshow_video_display_name(raw, "@device_pnp_ugreen").as_deref(),
            Some("UGREEN 35866")
        );
        assert_eq!(
            directshow_video_display_name(raw, "@device_pnp_capture_two").as_deref(),
            Some("USB Capture · 2")
        );
        assert_eq!(directshow_video_display_name(raw, "UGREEN 35866"), None);
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
