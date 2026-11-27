//! Salvo hoop around the tus router: `guard → tus handlers`.
//!
//! The guard is an around-middleware. On the way in it authenticates and
//! authorizes every non-OPTIONS request (fail closed), validates the publish
//! name, enforces the disk budget and binds server-owned metadata. On the way
//! out of an acknowledged write it applies the completion effects the tus layer
//! has no notion of: whole-file verification when the client declared a digest,
//! and discarding the upload when that digest does not match.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use salvo::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use salvo::tus::utils::is_safe_upload_id;
use salvo::tus::{DataStore, Metadata, TUS_VERSION, TusError};
use salvo::writing::Json;
use salvo::{Depot, FlowCtrl, handler};
use serde_json::json;

use crate::authorize::{AuthContext, AuthError, UploadAuthorizer, UploadOp, UploadScope};
use crate::checksum::{self, Checksum, ChecksumError};
use crate::config::TransferConfig;
use crate::names;
use crate::store::{BUDGET_SLACK_BYTES, ExpiringDiskStore, FreeSpace, budget_required};

/// Metadata key bound to the authenticated subject. A client-supplied value is
/// always overwritten: the server decides the identity.
pub const KEY_SUBJECT: &str = "subject";
/// Metadata key naming the board the image belongs to (authorization scope).
const KEY_BOARD: &str = "board";
/// Metadata key naming the completed file. The tus disk store finalizes a
/// completed upload to this name, so it is the publish name.
const KEY_FILENAME: &str = "filename";
/// Metadata key binding the upload to the DUT served by this instance. Written
/// by the server, never taken from the client.
const KEY_DUT: &str = "dutId";

/// Response header carrying the on-disk path of a completed upload.
pub const HEADER_ARTIFACT: &str = "x-duthub-artifact";
/// Response header carrying `<algorithm>:<digest>` once verification passed.
pub const HEADER_CHECKSUM: &str = "x-duthub-checksum";

/// State shared by the guard, the tus handlers and the sweep.
pub struct TransferState {
    pub authorizer: Option<Arc<dyn UploadAuthorizer>>,
    pub store: Arc<ExpiringDiskStore>,
    pub free_space: Arc<dyn FreeSpace>,
    pub config: TransferConfig,
    /// Mount path normalized to a leading slash and no trailing slash.
    pub base: String,
    /// DUT this listener serves; `None` for a standalone (single-tenant) root.
    /// When set, every upload is bound to it at creation and checked on later
    /// requests, so two DUT servers cannot touch each other's uploads.
    pub dut_id: Option<String>,
    /// Serializes creation: the budget is a snapshot, and upload records only
    /// exist after the tus layer runs, so two concurrent creates would each see
    /// the other's declared length as zero and oversell the disk.
    pub create_gate: tokio::sync::Mutex<()>,
    /// Bounds whole-file hashing: one 30 GiB verification must not serialize
    /// every other completion.
    pub verify_gate: std::sync::Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for TransferState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferState")
            .field("authorizer_configured", &self.authorizer.is_some())
            .field("store", &self.store)
            .field("config", &self.config)
            .field("base", &self.base)
            .field("dut_id", &self.dut_id)
            .finish_non_exhaustive()
    }
}

/// The guard itself; Salvo hoops must be `Clone + Send + Sync`.
#[derive(Clone)]
pub(crate) struct TransferGuard(pub Arc<TransferState>);

/// Everything the guard answers on its own, without reaching the tus layer.
#[derive(Debug)]
enum Rejection {
    Unauthorized,
    Forbidden(String),
    InvalidMetadata(String),
    InvalidChecksum,
    UnsupportedChecksum(String),
    /// Not below the mount in a spelling the router would route verbatim.
    InvalidPath,
    /// The request does not carry a supported `Tus-Resumable` version.
    UnsupportedVersion,
    /// Below the mount, but not `{base}/{id}` with a server-minted id.
    InvalidUploadId,
    /// The upload record exists but cannot be read or decoded.
    CorruptUpload(String),
    InsufficientStorage {
        required: u64,
        available: u64,
    },
    StorageProbe(String),
    MethodNotAllowed,
    ChecksumMismatch {
        algorithm: &'static str,
        expected: String,
        actual: String,
    },
    VerificationFailed(String),
}

impl From<AuthError> for Rejection {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Unauthorized => Self::Unauthorized,
            AuthError::Forbidden(reason) => Self::Forbidden(reason),
        }
    }
}

impl From<ChecksumError> for Rejection {
    fn from(error: ChecksumError) -> Self {
        match error {
            ChecksumError::Malformed => Self::InvalidChecksum,
            ChecksumError::Unsupported(algorithm) => Self::UnsupportedChecksum(algorithm),
        }
    }
}

#[handler]
impl TransferGuard {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let state = Arc::clone(&self.0);
        let method = req.method().clone();
        let path = req.uri().path();
        // The router normalizes `//`, `.` and `..` segments before matching, so
        // a non-canonical spelling would reach the tus handlers while this
        // guard classified it as outside the mount and skipped every check.
        // Reject such spellings outright; only the wire form the router routes
        // verbatim may be served.
        if method != Method::OPTIONS
            && let Some(rejection) = non_canonical_path(path, &state.base)
        {
            return render_rejection(res, rejection);
        }
        // OPTIONS stays anonymous per the tus spec, and paths outside the
        // mount are not ours to guard (the router answers 404).
        let target = target(path, &state.base);
        if method == Method::OPTIONS || target == Target::Outside {
            ctrl.call_next(req, depot, res).await;
            return;
        }

        // Fail closed on the protocol version: answering 412 here is what the
        // tus layer would do, but only after this guard has skipped every
        // check. Never let a version variant reach the handlers unauthenticated.
        if !has_supported_version(req.headers()) {
            res.headers_mut()
                .insert("tus-version", HeaderValue::from_static(TUS_VERSION));
            return render_rejection(res, Rejection::UnsupportedVersion);
        }

        // No authorizer configured → DenyAll semantics (fail closed).
        let Some(authorizer) = state.authorizer.clone() else {
            return render_rejection(res, Rejection::Unauthorized);
        };
        let scope = match authorizer.authenticate(bearer_token(req.headers())) {
            Ok(scope) => scope,
            Err(error) => return render_rejection(res, Rejection::from(error)),
        };

        // Only the tus upload surface is served: create at the mount, operate
        // on `{base}/{id}`. Anything else (GET downloads included) is a 405.
        let create = method == Method::POST && target == Target::Mount;
        let upload = match &target {
            Target::Upload(id) => Some(id.as_str()),
            _ => None,
        };
        // Budget is a snapshot taken before the tus layer persists the record,
        // so creations are serialized across the check and the write.
        let _create_gate = if create {
            Some(state.create_gate.lock().await)
        } else {
            None
        };
        let rejection = if create {
            authorize_create(&state, authorizer.as_ref(), &scope, req)
                .await
                .err()
        } else if let Some(id) = upload
            && [Method::PATCH, Method::HEAD, Method::DELETE].contains(&method)
        {
            authorize_resource(&state, authorizer.as_ref(), &scope, &method, id)
                .await
                .err()
        } else if target == Target::Malformed {
            Some(Rejection::InvalidUploadId)
        } else {
            Some(Rejection::MethodNotAllowed)
        };
        if let Some(rejection) = rejection {
            return render_rejection(res, rejection);
        }

        ctrl.call_next(req, depot, res).await;

        // An acknowledged write may have completed an upload.
        if let Some(id) = upload {
            complete(&state, res, id).await;
        } else if create && let Some(id) = created_id(res) {
            complete(&state, res, &id).await;
        }
    }
}

/// Where a request path points, relative to the mount.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// Not below the mount: the router answers 404.
    Outside,
    /// The mount itself (with or without a trailing slash): creation lives here.
    Mount,
    /// `{base}/{id}` with a canonical, server-minted id.
    Upload(String),
    /// Below the mount, but not a canonical upload path.
    Malformed,
}

/// Classify a request path.
///
/// Upload ids are only ever accepted in their canonical spelling. The router
/// percent-decodes path segments, so an id written as `/%63…` would reach the
/// tus handlers as `c…`; authorizing the raw text instead would look up the
/// wrong record and let a foreign subject through. Anything but the exact
/// server-minted form is therefore refused, never waved through.
fn target(path: &str, base: &str) -> Target {
    if path == base {
        return Target::Mount;
    }
    let Some(rest) = path
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return Target::Outside;
    };
    if rest.is_empty() {
        Target::Mount
    } else if !rest.contains('/') && is_safe_upload_id(rest) {
        Target::Upload(rest.to_owned())
    } else {
        Target::Malformed
    }
}

/// `true` when the shell forwarded the path exactly as the router will route
/// it. A single trailing slash is allowed because the mount is addressed that
/// way too; everything else that the router would rewrite (`//` runs, `.`/`..`
/// segments, percent-encoding, backslashes) is not.
fn is_canonical_path(path: &str) -> bool {
    if !path.starts_with('/') || path.contains('\\') || path.contains('%') {
        return false;
    }
    let mut segments = path.strip_suffix('/').unwrap_or(path).split('/');
    if segments.next() != Some("") {
        return false;
    }
    segments.all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Why a path may not be served at all. A bad spelling below the mount is an
/// upload-id problem; anywhere else it is a mount problem.
fn non_canonical_path(path: &str, base: &str) -> Option<Rejection> {
    if is_canonical_path(path) {
        return None;
    }
    if path.starts_with(base) && path[base.len()..].starts_with('/') {
        Some(Rejection::InvalidUploadId)
    } else {
        Some(Rejection::InvalidPath)
    }
}

/// POST at the mount: validate names and the declared digest (400), then the
/// scope (403) and the disk budget (507), then bind the server-owned metadata.
///
/// Name validation runs before the scope check so a malformed request is 400
/// rather than 403.
async fn authorize_create(
    state: &TransferState,
    authorizer: &dyn UploadAuthorizer,
    scope: &UploadScope,
    req: &mut Request,
) -> Result<(), Rejection> {
    let invalid_metadata = |detail: &str| Rejection::InvalidMetadata(detail.to_owned());
    let mut metadata = match req.headers().get("upload-metadata") {
        Some(value) => value
            .to_str()
            .map_err(|_| invalid_metadata("Upload-Metadata is not text"))
            .and_then(|raw| {
                // An empty header carries no metadata (some clients send it
                // unconditionally). Only a non-empty malformed value is a 400.
                if raw.trim().is_empty() {
                    return Ok(Metadata::default());
                }
                Metadata::parse_metadata(raw)
                    .map_err(|_| invalid_metadata("Upload-Metadata is not valid tus metadata"))
            })?,
        None => Metadata::default(),
    };

    let filename = metadata_text(&metadata, KEY_FILENAME)?
        .ok_or_else(|| invalid_metadata("missing filename metadata"))?;
    names::validate_publish_name(&filename, &state.config.allowed_extensions)
        .map_err(|_| invalid_metadata("filename rejected"))?;
    let board = metadata_text(&metadata, KEY_BOARD)?;
    if let Some(board) = &board {
        names::validate_board_name(board).map_err(|_| invalid_metadata("board rejected"))?;
    }

    let declared_len = upload_length(req.headers());
    if declared_len == Some(0) && !state.config.allow_zero_length_uploads {
        return Err(invalid_metadata(
            "zero-length uploads are disabled by this receiver",
        ));
    }
    authorizer.authorize(
        scope,
        &AuthContext {
            op: UploadOp::Create,
            board: board.as_deref(),
            declared_len,
            upload_subject: None,
        },
    )?;

    // Verification is the client's call: no header, no hashing.
    let checksum = match req.headers().get("upload-checksum") {
        Some(value) => {
            let raw = value.to_str().map_err(|_| Rejection::InvalidChecksum)?;
            Some(Checksum::parse_header(raw)?)
        }
        None => None,
    };

    if let Some(declared) = declared_len {
        // Per-DUT ceiling first: completed artifacts count toward it, and when
        // it is short the oldest completed artifacts are evicted (in-flight
        // uploads never are). The global free-space budget still applies.
        let inflight = state.store.inflight_remaining_bytes().await;
        if let Some(quota) = state.config.dut_storage_bytes {
            let mut used = state.store.used_bytes_async().await;
            let required = used.saturating_add(inflight).saturating_add(declared);
            if required > quota {
                let need = required - quota;
                state.store.evict_completed(need).await;
                used = state.store.used_bytes_async().await;
                let required = used.saturating_add(inflight).saturating_add(declared);
                if required > quota {
                    return Err(Rejection::InsufficientStorage {
                        required,
                        available: quota.saturating_sub(used.saturating_add(inflight)),
                    });
                }
            }
        }
        let required = budget_required(
            declared,
            state.config.reserved_disk_bytes,
            BUDGET_SLACK_BYTES,
            inflight,
        );
        match state.free_space.available_bytes(state.store.root()) {
            Ok(available) if required <= available => {}
            Ok(available) => {
                return Err(Rejection::InsufficientStorage {
                    required,
                    available,
                });
            }
            Err(error) => return Err(Rejection::StorageProbe(error.to_string())),
        }
    }

    // Server-owned keys: drop whatever the client sent, then bind ours.
    metadata.remove(KEY_SUBJECT);
    metadata.remove(checksum::METADATA_KEY);
    metadata.remove(KEY_DUT);
    metadata.insert(
        KEY_SUBJECT.to_owned(),
        Some(scope.subject.as_bytes().to_vec()),
    );
    if let Some(dut_id) = &state.dut_id {
        metadata.insert(KEY_DUT.to_owned(), Some(dut_id.as_bytes().to_vec()));
    }
    if let Some(checksum) = &checksum {
        metadata.insert(
            checksum::METADATA_KEY.to_owned(),
            Some(checksum.to_metadata().into_bytes()),
        );
    }
    let header = HeaderValue::from_str(&Metadata::stringify(metadata))
        .map_err(|_| invalid_metadata("metadata is not representable as a header"))?;
    req.headers_mut().insert("upload-metadata", header);
    Ok(())
}

/// PATCH / HEAD / DELETE on `{base}/{id}`: check the scope against the subject
/// and board recorded for the upload. Unknown ids pass through so the tus layer
/// answers 404 without leaking scope information.
async fn authorize_resource(
    state: &TransferState,
    authorizer: &dyn UploadAuthorizer,
    scope: &UploadScope,
    method: &Method,
    id: &str,
) -> Result<(), Rejection> {
    let op = if method == Method::PATCH {
        UploadOp::Write
    } else if method == Method::HEAD {
        UploadOp::Query
    } else if method == Method::DELETE {
        UploadOp::Terminate
    } else {
        return Ok(());
    };
    // Only a missing record is the tus layer's 404 to answer. A record that
    // exists but cannot be read or decoded must not become "unknown upload":
    // skipping the scope check there would be fail-open.
    let upload = match state.store.get_upload_file_info(id).await {
        Ok(upload) => upload,
        Err(TusError::NotFound) => return Ok(()),
        Err(error) => return Err(Rejection::CorruptUpload(error.to_string())),
    };
    let corrupt = |error: std::str::Utf8Error| Rejection::CorruptUpload(error.to_string());
    let (subject, board, dut) = match upload.metadata.as_ref() {
        None => (None, None, None),
        Some(metadata) => (
            metadata
                .get_text(KEY_SUBJECT)
                .map_err(corrupt)?
                .map(str::to_owned),
            metadata
                .get_text(KEY_BOARD)
                .map_err(corrupt)?
                .map(str::to_owned),
            metadata
                .get_text(KEY_DUT)
                .map_err(corrupt)?
                .map(str::to_owned),
        ),
    };
    // A per-DUT listener only serves uploads it created; an upload without the
    // binding (created by a standalone root) is not this listener's.
    if let Some(expected) = &state.dut_id
        && dut.as_deref() != Some(expected.as_str())
    {
        return Err(Rejection::Forbidden(
            "upload belongs to another DUT".to_string(),
        ));
    }
    authorizer.authorize(
        scope,
        &AuthContext {
            op,
            board: board.as_deref(),
            declared_len: None,
            upload_subject: subject.as_deref(),
        },
    )?;
    Ok(())
}

/// Completion effects for a write the tus layer just acknowledged. Only a
/// finished upload gets here: verify it when the client declared a digest, and
/// discard it when the bytes on disk are not the file that digest describes.
async fn complete(state: &TransferState, res: &mut Response, id: &str) {
    if !matches!(
        res.status_code,
        Some(StatusCode::CREATED | StatusCode::NO_CONTENT)
    ) {
        return;
    }
    let Ok(upload) = state.store.get_upload_file_info(id).await else {
        return;
    };
    // Still receiving bytes: nothing to verify yet.
    if !matches!((upload.size, upload.offset), (Some(size), Some(offset)) if size == offset) {
        return;
    }
    let Some(storage) = upload.storage.as_ref() else {
        return;
    };

    let declared = match upload
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(checksum::METADATA_KEY))
    {
        // No digest declared: the client asked for a plain transfer.
        None => {
            insert_artifact(res, state, &storage.path);
            return;
        }
        // The record is ours. If it cannot be read back, the upload cannot be
        // trusted, so fail closed instead of publishing it unverified.
        Some(record) => if let Some(declared) = record
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(Checksum::from_metadata) { declared } else {
            discard(state, id, "stored checksum record is unreadable".to_owned()).await;
            render_rejection(
                res,
                Rejection::VerificationFailed(
                    "stored checksum record is unreadable".to_owned(),
                ),
            );
            return;
        },
    };
    let Ok(marker) = state.store.verified_marker(id) else {
        return;
    };
    if let Ok(verified) = tokio::fs::read_to_string(&marker).await {
        insert_artifact(res, state, &storage.path);
        record_checksum(res, verified.trim());
        return;
    }

    // Bound the number of concurrent whole-file hashes. A global mutex would
    // let one 30 GiB verification stall every other completion.
    let Ok(_permit) = state.verify_gate.acquire().await else {
        return;
    };
    if let Ok(verified) = tokio::fs::read_to_string(&marker).await {
        insert_artifact(res, state, &storage.path);
        record_checksum(res, verified.trim());
        return;
    }

    let algorithm = declared.algorithm;
    let expected = declared.digest.clone();
    let path = PathBuf::from(&storage.path);
    let verify = tokio::task::spawn_blocking(move || {
        let actual = declared.digest_file(&path)?;
        if actual == declared.digest {
            std::fs::write(&marker, format!("{}:{actual}", declared.algorithm.as_str()))?;
        }
        Ok::<_, std::io::Error>(actual)
    })
    .await;
    match verify {
        Ok(Ok(actual)) if actual == expected => {
            // Only a verified artifact may advertise its location; the 460
            // path below must not carry a path to bytes that were discarded.
            insert_artifact(res, state, &storage.path);
            record_checksum(
                res,
                &Checksum {
                    algorithm,
                    digest: actual,
                }
                .to_metadata(),
            );
        }
        Ok(Ok(actual)) => {
            // Terminal: the bytes on disk are not the file the client declared.
            // Resending the same bytes can only fail again.
            discard(
                state,
                id,
                format!("checksum mismatch: expected {expected}, actual {actual}"),
            )
            .await;
            render_rejection(
                res,
                Rejection::ChecksumMismatch {
                    algorithm: algorithm.as_str(),
                    expected,
                    actual,
                },
            );
        }
        Ok(Err(error)) => {
            render_rejection(res, Rejection::VerificationFailed(error.to_string()));
        }
        Err(error) => {
            render_rejection(res, Rejection::VerificationFailed(error.to_string()));
        }
    }
}

/// Discard an upload that must not be published. The failed flag is written
/// first, so a removal that fails while the files are still held is finished by
/// the sweep rather than leaving an unverified artifact that looks published.
async fn discard(state: &TransferState, id: &str, reason: String) {
    if let Err(error) = state.store.flag_failed(id, &reason).await {
        tracing::error!(
            upload_id = %id,
            error = %error,
            "rejected upload could not be flagged; it stays unverified"
        );
    }
    if let Err(error) = state.store.remove(id).await {
        tracing::error!(
            upload_id = %id,
            error = %error,
            "rejected upload could not be discarded now; the sweep will retry"
        );
    }
}

/// Advertise the artifact as a path relative to the transfer root. Never the
/// host's absolute path: that leaks the drive letter, user name and install
/// location to any authenticated client, and to the LAN when `lanAccess` is on.
fn insert_artifact(res: &mut Response, state: &TransferState, storage: &str) {
    let Ok(relative) = Path::new(storage).strip_prefix(state.store.root()) else {
        return;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    if let Ok(value) = HeaderValue::from_str(&relative) {
        res.headers_mut().insert(HEADER_ARTIFACT, value);
    }
}

/// Stamp `<algorithm>:<digest>` onto a verified completion response.
fn record_checksum(res: &mut Response, verified: &str) {
    if let Ok(value) = HeaderValue::from_str(verified) {
        res.headers_mut().insert(HEADER_CHECKSUM, value);
    }
}

/// Answer a request the guard rejected, as JSON with a stable `error` code.
fn render_rejection(res: &mut Response, rejection: Rejection) {
    let (status, body) = match rejection {
        Rejection::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            json!({ "error": "unauthorized", "detail": "missing or invalid token" }),
        ),
        Rejection::Forbidden(reason) => (
            StatusCode::FORBIDDEN,
            json!({ "error": "forbidden", "detail": reason }),
        ),
        Rejection::InvalidMetadata(detail) => (
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid_metadata", "detail": detail }),
        ),
        Rejection::InvalidChecksum => (
            StatusCode::BAD_REQUEST,
            json!({
                "error": "invalid_checksum",
                "detail": "Upload-Checksum must be `<algorithm> <hex digest>`",
            }),
        ),
        Rejection::InvalidPath => (
            StatusCode::BAD_REQUEST,
            json!({
                "error": "invalid_path",
                "detail": "address the endpoint with its canonical path (no //, . or .. segments)",
            }),
        ),
        Rejection::UnsupportedVersion => (
            StatusCode::PRECONDITION_FAILED,
            json!({
                "error": "unsupported_tus_version",
                "detail": "Tus-Resumable: 1.0.0 is required",
            }),
        ),
        Rejection::CorruptUpload(detail) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "corrupt_upload_record", "detail": detail }),
        ),
        Rejection::InvalidUploadId => (
            StatusCode::BAD_REQUEST,
            json!({
                "error": "invalid_upload_id",
                "detail": "address uploads exactly as returned in the Location header",
            }),
        ),
        Rejection::UnsupportedChecksum(algorithm) => (
            StatusCode::BAD_REQUEST,
            json!({
                "error": "unsupported_checksum_algorithm",
                "detail": format!(
                    "unsupported checksum algorithm: {algorithm} (supported: blake3, sha256)"
                ),
            }),
        ),
        Rejection::InsufficientStorage {
            required,
            available,
        } => (
            StatusCode::INSUFFICIENT_STORAGE,
            json!({
                "error": "insufficient_storage",
                "required": required,
                "available": available,
            }),
        ),
        Rejection::StorageProbe(detail) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "storage_probe_failed", "detail": detail }),
        ),
        Rejection::MethodNotAllowed => (
            StatusCode::METHOD_NOT_ALLOWED,
            json!({ "error": "method_not_allowed", "detail": "only tus upload requests are served" }),
        ),
        Rejection::ChecksumMismatch {
            algorithm,
            expected,
            actual,
        } => (
            // 460 is this endpoint's long-standing mismatch code (not a
            // tus-defined status).
            StatusCode::from_u16(460).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            json!({
                "error": "checksum_mismatch",
                "detail": "checksum mismatch",
                "algorithm": algorithm,
                "expected": expected,
                "actual": actual,
            }),
        ),
        Rejection::VerificationFailed(detail) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "verification_failed", "detail": detail }),
        ),
    };
    if status == StatusCode::UNAUTHORIZED {
        res.headers_mut()
            .insert("www-authenticate", HeaderValue::from_static("Bearer"));
    }
    // Tus success headers describe an upload the client can no longer use.
    res.headers.remove("upload-offset");
    res.headers.remove(HEADER_ARTIFACT);
    res.status_code(status);
    res.render(Json(body));
}

/// Read a metadata value as UTF-8 text; a key without a value reads as absent.
fn metadata_text(metadata: &Metadata, key: &str) -> Result<Option<String>, Rejection> {
    metadata
        .get_text(key)
        .map(|value| value.map(str::to_owned))
        .map_err(|_| Rejection::InvalidMetadata(format!("{key} metadata is not valid UTF-8")))
}

/// Extract a Bearer token from the `Authorization` header.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get("authorization")?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        let token = token.trim();
        (!token.is_empty()).then_some(token)
    } else {
        None
    }
}

/// An unsupported (or missing) tus version is the tus layer's call: it answers
/// 412. The guard steps aside so its own validation cannot turn that into a 400.
fn has_supported_version(headers: &HeaderMap) -> bool {
    headers
        .get("tus-resumable")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == TUS_VERSION)
}

/// Parse `Upload-Length`; `None` when absent or unparseable (the tus layer
/// re-validates and answers 400).
fn upload_length(headers: &HeaderMap) -> Option<u64> {
    headers.get("upload-length")?.to_str().ok()?.parse().ok()
}

/// Upload id of a creation response, read from its `Location` header.
fn created_id(res: &Response) -> Option<String> {
    res.headers()
        .get("location")?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_extraction() {
        let mut headers = HeaderMap::new();
        assert!(bearer_token(&headers).is_none());
        headers.insert("authorization", HeaderValue::from_static("Bearer tok"));
        assert_eq!(bearer_token(&headers), Some("tok"));
        headers.insert("authorization", HeaderValue::from_static("bearer tok"));
        assert_eq!(bearer_token(&headers), Some("tok"));
        headers.insert("authorization", HeaderValue::from_static("Basic tok"));
        assert!(bearer_token(&headers).is_none());
        headers.insert("authorization", HeaderValue::from_static("Bearer "));
        assert!(bearer_token(&headers).is_none());
    }

    #[test]
    fn target_splits_the_mount_from_its_uploads() {
        let base = "/transfer";
        let id = "00000000-0000-4000-8000-0000000000d1";
        assert_eq!(target("/transfer", base), Target::Mount);
        assert_eq!(target("/transfer/", base), Target::Mount);
        assert_eq!(
            target(&format!("/transfer/{id}"), base),
            Target::Upload(id.to_owned())
        );
        // A shared prefix is not the mount.
        assert_eq!(target("/transfers", base), Target::Outside);
        assert_eq!(target("/other/transfer", base), Target::Outside);
        assert_eq!(target("/", base), Target::Outside);
    }

    #[test]
    fn target_refuses_every_non_canonical_id_spelling() {
        // The router percent-decodes these to a real id; the guard must never
        // authorize the raw text and let the decoded one through.
        let base = "/transfer";
        for path in [
            "/transfer/%63bc",
            "/transfer/abc/extra",
            "/transfer/abc/",
            "/transfer//abc",
            "/transfer/a%2fb",
            "/transfer/a\\b",
            "/transfer/../etc",
            "/transfer/a b",
        ] {
            assert_eq!(target(path, base), Target::Malformed, "{path}");
        }
    }

    #[test]
    fn canonical_gate_rejects_what_the_router_would_normalize() {
        // Salvo collapses `//`, drops `.` and pops `..` before matching, so any
        // such spelling desynchronises the guard's raw path from the path the
        // tus handlers act on. Only the verbatim form may be served.
        let base = "/transfer";
        for ok in [
            "/transfer",
            "/transfer/",
            "/transfer/abc",
            "/transfer/a-b_c",
        ] {
            assert!(is_canonical_path(ok), "{ok}");
            assert!(non_canonical_path(ok, base).is_none(), "{ok}");
        }
        for bad in [
            "//transfer",
            "///transfer",
            "/./transfer",
            "/foo/../transfer",
            "/%2Ftransfer",
            "/transfer//abc",
            "/transfer/./abc",
            "/transfer/../abc",
            "/transfer/%63bc",
            "/transfer/a%2fb",
            "/transfer/a\\b",
            "transfer",
            "",
        ] {
            assert!(!is_canonical_path(bad), "{bad:?}");
            assert!(non_canonical_path(bad, base).is_some(), "{bad:?}");
        }
        // A bad spelling below the mount is reported as an upload-id problem,
        // anywhere else as a mount problem.
        assert!(matches!(
            non_canonical_path("//transfer", base),
            Some(Rejection::InvalidPath)
        ));
        assert!(matches!(
            non_canonical_path("/%2Ftransfer", base),
            Some(Rejection::InvalidPath)
        ));
        assert!(matches!(
            non_canonical_path("/transfer/%63bc", base),
            Some(Rejection::InvalidUploadId)
        ));
    }

    #[test]
    fn upload_length_parses_only_valid_values() {
        let mut headers = HeaderMap::new();
        assert!(upload_length(&headers).is_none());
        headers.insert("upload-length", HeaderValue::from_static("1024"));
        assert_eq!(upload_length(&headers), Some(1024));
        headers.insert("upload-length", HeaderValue::from_static("abc"));
        assert!(upload_length(&headers).is_none());
    }

    #[test]
    fn version_gate_leaves_unsupported_versions_to_the_tus_layer() {
        let mut headers = HeaderMap::new();
        assert!(!has_supported_version(&headers), "missing header");
        headers.insert("tus-resumable", HeaderValue::from_static("0.2.2"));
        assert!(!has_supported_version(&headers));
        headers.insert("tus-resumable", HeaderValue::from_static("1.0.0"));
        assert!(has_supported_version(&headers));
    }

    #[test]
    fn created_id_comes_from_the_location_header() {
        let mut res = Response::new();
        assert_eq!(created_id(&res), None);
        res.headers_mut()
            .insert("location", HeaderValue::from_static("/transfer/abc123"));
        assert_eq!(created_id(&res), Some("abc123".to_string()));
    }

    #[test]
    fn rejections_carry_a_json_error_code() {
        let cases: Vec<(Rejection, StatusCode, &str)> = vec![
            (
                Rejection::Unauthorized,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ),
            (
                Rejection::Forbidden("board \"b\" not allowed".to_owned()),
                StatusCode::FORBIDDEN,
                "forbidden",
            ),
            (
                Rejection::InvalidMetadata("filename rejected".to_owned()),
                StatusCode::BAD_REQUEST,
                "invalid_metadata",
            ),
            (
                Rejection::UnsupportedChecksum("sha1".to_owned()),
                StatusCode::BAD_REQUEST,
                "unsupported_checksum_algorithm",
            ),
            (
                Rejection::InsufficientStorage {
                    required: 10,
                    available: 0,
                },
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_storage",
            ),
            (
                Rejection::MethodNotAllowed,
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
            ),
            (
                Rejection::ChecksumMismatch {
                    algorithm: "blake3",
                    expected: "a".repeat(64),
                    actual: "b".repeat(64),
                },
                StatusCode::from_u16(460).unwrap(),
                "checksum_mismatch",
            ),
        ];
        for (rejection, status, code) in cases {
            let mut res = Response::new();
            res.headers_mut()
                .insert("upload-offset", HeaderValue::from_static("10"));
            render_rejection(&mut res, rejection);
            assert_eq!(res.status_code, Some(status), "{code}");
            // A rejected write must not keep the offset the tus layer acked.
            assert!(res.headers().get("upload-offset").is_none(), "{code}");
            let body = res.take_body();
            let salvo::http::body::ResBody::Once(bytes) = body else {
                panic!("{code}: a JSON rejection must render as one buffered body");
            };
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"], code);
            // Only 401 carries the challenge.
            assert_eq!(
                res.headers().get("www-authenticate").is_some(),
                status == StatusCode::UNAUTHORIZED,
                "{code}"
            );
        }
    }
}
