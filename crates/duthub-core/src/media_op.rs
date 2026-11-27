//! MCP → GUI media-op bridge DTOs (`docs/design/CAPTURE_STREAM_BACKENDS.md` §5.5).
//!
//! Why this lives in `duthub-core`: the dependency direction is
//! `duthub-desktop → duthub-mcp → duthub-core`, so a request/reply type owned
//! by the desktop crate would force `duthub-mcp` into a cyclic dependency.
//! Everything here is a plain DTO plus the submission seam; the GUI-side
//! queue, the recording registry and `MediaView` stay in `apps/desktop`.
//!
//! Reply-channel ownership: the MCP handler creates the pair, keeps the
//! [`OpReceiver`], and moves the request — with its single `Sender` — into the
//! GUI queue. From then on the GUI is the only sender and answers exactly
//! once with a [`MediaOpReply`].

/// Correlates one request with its in-flight entry on the GUI side.
pub type OpId = u64;

/// Identifies an active recording; returned by `recordStart`, consumed by a
/// paired `recordStop`.
pub type RecordingId = u64;

/// What the MCP layer awaits (under `tokio::time::timeout`).
pub type OpReceiver = tokio::sync::oneshot::Receiver<MediaOpReply>;

/// The operation carried by a request.
#[derive(Debug)]
pub enum MediaOp {
    /// Grab one PNG frame and report it as an artifact.
    Snapshot { note: Option<String> },
    /// Start recording. No duration field by design: a recording runs until
    /// the paired `RecordStop` arrives (or the capture is stopped).
    RecordStart { note: Option<String> },
    /// Stop the recording started earlier and hand back its artifact.
    RecordStop { recording_id: RecordingId },
}

/// One queued operation.
///
/// `reply` is moved into the GUI's in-flight table on receipt; no worker ever
/// sees it, so a reply cannot be sent twice or from the wrong thread.
#[derive(Debug)]
pub struct MediaOpRequest {
    pub id: OpId,
    pub dut_id: String,
    /// A capture key reported by the desktop, or `None` for its first enabled source.
    pub source: Option<String>,
    pub op: MediaOp,
    pub reply: tokio::sync::oneshot::Sender<MediaOpReply>,
}

impl MediaOpRequest {
    /// Build a request together with the receiver the caller awaits.
    #[must_use]
    pub fn channel(
        id: OpId,
        dut_id: String,
        source: Option<String>,
        op: MediaOp,
    ) -> (Self, OpReceiver) {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        (
            Self {
                id,
                dut_id,
                source,
                op,
                reply,
            },
            receiver,
        )
    }
}

/// Successful outcomes and failures share one channel: the GUI never returns
/// a partial artifact, so "no reply" always means "dropped or timed out".
#[derive(Debug)]
pub enum MediaOpReply {
    /// Result of `snapshot` / `recordStop`.
    Artifact(MediaArtifact),
    /// Result of `recordStart`.
    Recording(RecordingHandle),
    Err(MediaOpError),
}

/// A failure the MCP layer maps onto a `MEDIA_*` `data.code`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct MediaOpError {
    pub code: &'static str,
    pub message: String,
}

/// Returned by `recordStart` as soon as `Capture::record` hands over the first
/// segment.
#[derive(Debug)]
pub struct RecordingHandle {
    pub recording_id: RecordingId,
    pub source: String,
    /// First segment reported by `Capture::record` (it returns immediately).
    pub first_path: String,
    /// UNIX epoch millis when `record()` returned.
    pub started_at_ms: u64,
    pub note: Option<String>,
}

/// A finished media file on disk.
#[derive(Debug)]
pub struct MediaArtifact {
    /// Snapshot: the PNG. Recording: the last segment.
    pub path: String,
    /// Recording only: the first segment (`== path` when nothing rotated).
    pub first_path: Option<String>,
    /// Recording only: more than one segment exists.
    pub rotated: bool,
    /// Size of the file at `path`.
    pub bytes: u64,
    /// Snapshot only; omitted when the PNG header could not be parsed.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Recording only: measured wall-clock seconds from `record()` to the last
    /// segment. There is no cap, so this can exceed any fixed window.
    pub duration_secs: Option<u64>,
    pub note: Option<String>,
    pub source: Option<String>,
}

/// Queue end of the bridge: the one seam the MCP layer needs.
///
/// The desktop implements this over the queue handle it shares with
/// `RuntimeMcpState`. A full or closed queue hands the request back (instead
/// of dropping it silently) so the caller can answer `MEDIA_OP_DROPPED` while
/// keeping ownership of the reply sender.
pub trait MediaOpSubmitter: Send + Sync {
    fn submit(&self, request: MediaOpRequest) -> Result<(), MediaOpRequest>;
}
