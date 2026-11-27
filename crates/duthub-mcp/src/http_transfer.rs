//! Streaming HTTP endpoints mounted alongside `/mcp` on the same listener:
//!
//! - `PUT /dut/{dut_id}/image/{filename}`: single-request streaming image upload for `curl -T`
//!   (supports optional `X-Content-SHA256` verification and `?overwrite=true`).
//! - `OPTIONS|POST /dut/{dut_id}/transfer` and `HEAD|PATCH|DELETE /dut/{dut_id}/transfer/{upload_id}`:
//!   resumable tus 1.0.0 upload endpoint supporting both `ztus` (HTTP/2 cleartext `h2c` and HTTP/1.1)
//!   and standard `curl` tus clients.
//! - `GET /dut/{dut_id}/logs/export`: snapshot-bounded bulk export of archived serial log segments
//!   with optional ANSI stripping and gzip compression.
//!
//! All mutating upload routes require Bearer authentication, `Host`/`Origin` validation, DUT scope
//! verification, and explicit `DutAction::Flash` authorization (fail-closed; read-only tokens are
//! rejected with actionable guidance). Log export requires `DutAction::Read`.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, options, put};
use base64::Engine as _;
use http_body_util::BodyExt as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

use duthub_core::Principal;
use duthub_core::agent_access::DutAction;
use duthub_core::audit::AuditOutcome;
use duthub_core::lease::LeasePhase;

use crate::auth_http::{
    AccessContext, AuthFailureDecision, AuthFailureLimiter, bearer_token, rate_limited_response,
    unauthorized_response,
};
use crate::state::McpState;

/// Maximum firmware upload size accepted on HTTP transfer endpoints (30 GiB).
pub const MAX_IMAGE_UPLOAD_BYTES: u64 = 30 * 1024 * 1024 * 1024;
/// tus protocol version advertised and enforced on `/dut/{dut_id}/transfer`.
pub const TUS_VERSION: &str = "1.0.0";
/// Maximum simultaneous in-flight tus sessions kept in memory per listener.
const MAX_TUS_SESSIONS: usize = 64;

#[derive(Clone)]
pub(crate) struct HttpTransferState {
    pub(crate) state: Arc<dyn McpState>,
    pub(crate) access: Option<Arc<AccessContext>>,
    pub(crate) listener_id: Option<Arc<str>>,
    pub(crate) listener_dut: Option<Arc<str>>,
    pub(crate) allowed_hosts: Arc<[String]>,
    failures: Arc<AuthFailureLimiter>,
    tus_sessions: Arc<Mutex<HashMap<String, TusUploadSession>>>,
}

impl std::fmt::Debug for HttpTransferState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransferState")
            .field("listener_id", &self.listener_id)
            .field("listener_dut", &self.listener_dut)
            .field("allowed_hosts_count", &self.allowed_hosts.len())
            .finish_non_exhaustive()
    }
}

struct TusUploadSession {
    dut_id: String,
    dut_name: String,
    root: PathBuf,
    temp_name: String,
    temp_path: PathBuf,
    final_name: String,
    overwrite: bool,
    subject: String,
    total_length: u64,
    offset: u64,
    expected_sha256: Option<String>,
    hasher: Sha256,
    busy: bool,
}

impl HttpTransferState {
    pub(crate) fn new(
        state: Arc<dyn McpState>,
        access: Option<Arc<AccessContext>>,
        listener_id: Option<Arc<str>>,
        listener_dut: Option<Arc<str>>,
        allowed_hosts: Vec<String>,
    ) -> Self {
        Self {
            state,
            access,
            listener_id,
            listener_dut,
            allowed_hosts: Arc::from(allowed_hosts),
            failures: Arc::new(AuthFailureLimiter::new()),
            tus_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn validate_host_and_origin(
        &self,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<(), Box<Response>> {
        if headers.get(axum::http::header::ORIGIN).is_some() {
            return Err(Box::new(json_error(
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "origin_not_allowed",
                    "hint": "Omit the browser Origin header when calling duthub from CLI or Agent clients."
                }),
            )));
        }
        let Some(host_str) = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| uri.authority().map(http::uri::Authority::as_str))
        else {
            return Err(Box::new(json_error(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "error": "missing_host_header" }),
            )));
        };
        let Ok(authority) = http::uri::Authority::try_from(host_str) else {
            return Err(Box::new(json_error(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "error": "invalid_host_header" }),
            )));
        };
        let req_host = normalize_host(authority.host());
        let req_port = authority.port_u16();
        let allowed = self.allowed_hosts.iter().any(|entry| {
            let (entry_host, entry_port) = parse_allowed_entry(entry);
            entry_host == req_host && entry_port.is_none_or(|p| req_port == Some(p))
        });
        if !allowed {
            return Err(Box::new(json_error(
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "host_not_allowed",
                    "host": host_str,
                    "hint": "Enable agentAccess.lanAccess for LAN IPs or add this authority to agentAccess.allowedHosts in .duthub/settings.json."
                }),
            )));
        }
        Ok(())
    }

    fn authenticate_request(
        &self,
        headers: &HeaderMap,
        connect: Option<&ConnectInfo<SocketAddr>>,
    ) -> Result<Principal, Box<Response>> {
        let Some(ctx) = self.access.as_ref() else {
            return Err(Box::new(unauthorized_response()));
        };
        let source_key = connect.map_or_else(
            || "unknown".to_owned(),
            |ConnectInfo(addr)| addr.ip().to_string(),
        );
        if self.failures.is_limited_for(&source_key) {
            return Err(Box::new(rate_limited_response()));
        }
        let bearer = bearer_token(headers);
        let internal = self
            .listener_id
            .as_deref()
            .is_some_and(|id| ctx.auth.is_runtime_listener(id));
        let identity = match (internal, self.listener_id.as_deref()) {
            (true, Some(id)) => ctx.auth.authenticate_internal(id, bearer.unwrap_or("")),
            _ => ctx
                .auth
                .authenticate_http(self.listener_id.as_deref(), bearer),
        };
        if let Ok(principal) = identity {
            return Ok(principal);
        }
        let decision = self.failures.record_failure_for(&source_key);
        match decision {
            AuthFailureDecision::Audit => {
                ctx.audit.record(
                    "auth.fail",
                    None,
                    None,
                    None,
                    AuditOutcome::Denied,
                    Some("HTTP transfer/export Bearer authentication failed"),
                );
            }
            AuthFailureDecision::Aggregated { total } => {
                ctx.audit.record(
                    "auth.fail",
                    None,
                    None,
                    None,
                    AuditOutcome::Denied,
                    Some(&format!(
                        "Source {source_key} failed auth {total} times/window (rate_limited)"
                    )),
                );
            }
            AuthFailureDecision::Suppressed => {}
        }
        if matches!(decision, AuthFailureDecision::Aggregated { .. })
            || self.failures.is_limited_for(&source_key)
        {
            Err(Box::new(rate_limited_response()))
        } else {
            Err(Box::new(unauthorized_response()))
        }
    }

    fn authorize_dut_action(
        &self,
        principal: &Principal,
        dut_id: &str,
        action: DutAction,
    ) -> Result<(), Box<Response>> {
        if let Some(bound) = self.listener_dut.as_deref()
            && bound != dut_id
        {
            return Err(Box::new(json_error(
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "unauthorized",
                    "code": "UNAUTHORIZED",
                    "dutId": dut_id,
                    "hint": format!("This listener is strictly bound to DUT '{bound}'.")
                }),
            )));
        }
        let Some(ctx) = self.access.as_ref() else {
            return Err(Box::new(unauthorized_response()));
        };
        if ctx.auth.authorize(principal, dut_id, action).is_err() {
            let required = action.as_str();
            return Err(Box::new(json_error(
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "unauthorized",
                    "code": "UNAUTHORIZED",
                    "dutId": dut_id,
                    "requiredAction": required,
                    "hint": format!(
                        "Credential lacks '{required}' permission on DUT '{dut_id}'. Ask the duthub administrator to grant '{required}' for this DUT."
                    )
                }),
            )));
        }
        Ok(())
    }

    fn ensure_upload_slot_open(&self, dut_id: &str) -> Result<(), Box<Response>> {
        let Some(ctx) = self.access.as_ref() else {
            return Ok(());
        };
        if ctx.lease.manual_control() {
            return Err(Box::new(json_error(
                StatusCode::CONFLICT,
                serde_json::json!({
                    "error": "manual_takeover",
                    "code": "MANUAL_CONTROL",
                    "dutId": dut_id,
                    "hint": "Manual Takeover is active in duthub; wait for the operator to disable Manual Takeover before uploading firmware images."
                }),
            )));
        }
        let status = ctx.lease.status(dut_id);
        match status.phase {
            LeasePhase::Protected => Err(Box::new(json_error(
                StatusCode::CONFLICT,
                serde_json::json!({
                    "error": "dut_protected",
                    "code": "DUT_PROTECTED",
                    "dutId": dut_id,
                    "hint": "DUT is currently executing a protected flash operation; wait until dutState returns to ready."
                }),
            ))),
            LeasePhase::NeedsInspection => Err(Box::new(json_error(
                StatusCode::CONFLICT,
                serde_json::json!({
                    "error": "needs_inspection",
                    "code": "NEEDS_INSPECTION",
                    "dutId": dut_id,
                    "hint": "DUT is locked for human inspection after a hardware fault; call notify_user and wait for operator clearance."
                }),
            ))),
            LeasePhase::Free | LeasePhase::Held => Ok(()),
        }
    }

    fn resolve_dut_target(&self, dut_id: &str) -> Result<(PathBuf, String), Box<Response>> {
        let root = self.state.dut_artifact_root().ok_or_else(|| {
            Box::new(json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({ "error": "artifact_root_unavailable" }),
            ))
        })?;
        let dut = self.state.dut_info(dut_id).ok_or_else(|| {
            Box::new(json_error(
                StatusCode::NOT_FOUND,
                serde_json::json!({
                    "error": "dut_not_found",
                    "dutId": dut_id,
                    "hint": "Call MCP tool dut_list to discover valid dutId values."
                }),
            ))
        })?;
        Ok((root, dut.name))
    }

    fn audit_upload(
        &self,
        principal: &Principal,
        dut_id: &str,
        outcome: AuditOutcome,
        detail: &str,
    ) {
        if let Some(ctx) = self.access.as_ref() {
            let (agent, cred) = principal
                .audit_identity()
                .map_or((None, None), |(a, c)| (Some(a), Some(c)));
            ctx.audit.record(
                "image.upload",
                agent,
                cred,
                Some(dut_id),
                outcome,
                Some(detail),
            );
        }
    }
}

pub(crate) fn mount_transfer_routes(
    router: axum::Router,
    transfer: HttpTransferState,
) -> axum::Router {
    let sub = axum::Router::new()
        .route("/dut/{dut_id}/image/{filename}", put(handle_put_image))
        .route(
            "/dut/{dut_id}/transfer",
            options(handle_tus_options).post(handle_tus_create),
        )
        .route(
            "/dut/{dut_id}/transfer/{upload_id}",
            options(handle_tus_options)
                .head(handle_tus_head)
                .patch(handle_tus_patch)
                .delete(handle_tus_delete),
        )
        .route("/dut/{dut_id}/logs/export", get(handle_logs_export))
        .with_state(transfer);
    router.merge(sub)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutImageQuery {
    #[serde(default)]
    overwrite: bool,
}

async fn handle_put_image(
    State(ctx): State<HttpTransferState>,
    Path((dut_id, filename)): Path<(String, String)>,
    Query(query): Query<PutImageQuery>,
    uri: Uri,
    headers: HeaderMap,
    extensions: Extensions,
    body: Body,
) -> Response {
    if let Err(resp) = ctx.validate_host_and_origin(&uri, &headers) {
        return *resp;
    }
    let principal =
        match ctx.authenticate_request(&headers, extensions.get::<ConnectInfo<SocketAddr>>()) {
            Ok(p) => p,
            Err(resp) => return *resp,
        };
    if let Err(resp) = ctx.authorize_dut_action(&principal, &dut_id, DutAction::Flash) {
        return *resp;
    }
    if let Err(resp) = ctx.ensure_upload_slot_open(&dut_id) {
        return *resp;
    }
    let clean_name = match duthub_core::dut_artifacts::validate_image_filename(&filename) {
        Ok(name) => name.to_owned(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": "invalid_filename",
                    "code": "INVALID_FILE_ID",
                    "detail": err.to_string(),
                    "hint": "Use a plain .img, .bin, or .iso filename without path separators (e.g. PUT /dut/<dutId>/image/update.img)."
                }),
            );
        }
    };
    let expected_sha256 = match parse_optional_sha256_header(&headers, "x-content-sha256") {
        Ok(val) => val,
        Err(msg) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "error": "invalid_checksum_header", "detail": msg }),
            );
        }
    };
    if let Some(declared) = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        && declared > MAX_IMAGE_UPLOAD_BYTES
    {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            serde_json::json!({
                "error": "payload_too_large",
                "maxBytes": MAX_IMAGE_UPLOAD_BYTES
            }),
        );
    }
    let (root, dut_name) = match ctx.resolve_dut_target(&dut_id) {
        Ok(pair) => pair,
        Err(resp) => return *resp,
    };
    let temp_name = format!(".upload-{}.part", uuid::Uuid::new_v4().simple());
    let (std_file, _) = match duthub_core::dut_artifacts::create_image_upload_temp(
        &root, &dut_id, &dut_name, &temp_name,
    ) {
        Ok(pair) => pair,
        Err(err) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "upload_init_failed", "detail": err.to_string() }),
            );
        }
    };
    let mut async_file = tokio::fs::File::from_std(std_file);
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut stream = body;
    while let Some(frame_result) = stream.frame().await {
        let frame = match frame_result {
            Ok(f) => f,
            Err(err) => {
                drop(async_file);
                duthub_core::dut_artifacts::remove_image_upload_temp(
                    &root, &dut_id, &dut_name, &temp_name,
                );
                return json_error(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({ "error": "upload_stream_error", "detail": err.to_string() }),
                );
            }
        };
        if let Ok(data) = frame.into_data() {
            let chunk_len = u64::try_from(data.len()).unwrap_or(u64::MAX);
            if received.saturating_add(chunk_len) > MAX_IMAGE_UPLOAD_BYTES {
                drop(async_file);
                duthub_core::dut_artifacts::remove_image_upload_temp(
                    &root, &dut_id, &dut_name, &temp_name,
                );
                return json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    serde_json::json!({
                        "error": "payload_too_large",
                        "maxBytes": MAX_IMAGE_UPLOAD_BYTES
                    }),
                );
            }
            if let Err(err) = async_file.write_all(&data).await {
                drop(async_file);
                duthub_core::dut_artifacts::remove_image_upload_temp(
                    &root, &dut_id, &dut_name, &temp_name,
                );
                return json_error(
                    StatusCode::INSUFFICIENT_STORAGE,
                    serde_json::json!({ "error": "upload_write_failed", "detail": err.to_string() }),
                );
            }
            hasher.update(&data);
            received = received.saturating_add(chunk_len);
        }
    }
    if received == 0 {
        drop(async_file);
        duthub_core::dut_artifacts::remove_image_upload_temp(&root, &dut_id, &dut_name, &temp_name);
        return json_error(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "empty_upload",
                "hint": "Upload body must not be empty; pass --upload-file <path> or --data-binary @<path>."
            }),
        );
    }
    if let Err(err) = async_file.flush().await {
        drop(async_file);
        duthub_core::dut_artifacts::remove_image_upload_temp(&root, &dut_id, &dut_name, &temp_name);
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": "upload_flush_failed", "detail": err.to_string() }),
        );
    }
    drop(async_file);
    let actual_sha256 = duthub_core::util::hex_lower(&hasher.finalize());
    if let Some(expected) = expected_sha256.as_deref()
        && expected != actual_sha256
    {
        duthub_core::dut_artifacts::remove_image_upload_temp(&root, &dut_id, &dut_name, &temp_name);
        ctx.audit_upload(
            &principal,
            &dut_id,
            AuditOutcome::Denied,
            &format!("PUT {clean_name} checksum mismatch"),
        );
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({
                "error": "checksum_mismatch",
                "expectedSha256": expected,
                "actualSha256": actual_sha256
            }),
        );
    }
    match duthub_core::dut_artifacts::commit_image_upload_temp(
        &root,
        &dut_id,
        &dut_name,
        &temp_name,
        &clean_name,
        query.overwrite,
    ) {
        Ok(file_id) => {
            ctx.audit_upload(
                &principal,
                &dut_id,
                AuditOutcome::Ok,
                &format!("PUT {file_id} bytes={received} sha256={actual_sha256}"),
            );
            json_response(
                StatusCode::CREATED,
                serde_json::json!({
                    "ok": true,
                    "dutId": dut_id,
                    "fileId": file_id,
                    "bytes": received,
                    "sha256": actual_sha256,
                    "nextActionHint": format!(
                        "Verify with dut_files(dutId=\"{dut_id}\", category=\"image\") and flash via dut_control(action=\"flash_update\", dutId=\"{dut_id}\", fileId=\"{file_id}\", leaseId=\"...\")."
                    )
                }),
            )
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => json_error(
            StatusCode::CONFLICT,
            serde_json::json!({
                "error": "image_already_exists",
                "detail": err.to_string(),
                "hint": "Choose a new filename or append ?overwrite=true to replace the existing image."
            }),
        ),
        Err(err) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": "upload_commit_failed", "detail": err.to_string() }),
        ),
    }
}

async fn handle_tus_options(
    State(ctx): State<HttpTransferState>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = ctx.validate_host_and_origin(&uri, &headers) {
        return *resp;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let hdrs = response.headers_mut();
    hdrs.insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    hdrs.insert("tus-version", HeaderValue::from_static(TUS_VERSION));
    hdrs.insert(
        "tus-extension",
        HeaderValue::from_static("creation,termination"),
    );
    if let Ok(val) = HeaderValue::from_str(&MAX_IMAGE_UPLOAD_BYTES.to_string()) {
        hdrs.insert("tus-max-size", val);
    }
    hdrs.insert("tus-checksum-algorithm", HeaderValue::from_static("sha256"));
    response
}

fn require_tus_resumable(headers: &HeaderMap) -> Result<(), Box<Response>> {
    let version = headers
        .get("tus-resumable")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    if version == Some(TUS_VERSION) {
        return Ok(());
    }
    let mut resp = json_error(
        StatusCode::PRECONDITION_FAILED,
        serde_json::json!({
            "error": "unsupported_tus_version",
            "hint": "Send header 'Tus-Resumable: 1.0.0'."
        }),
    );
    resp.headers_mut()
        .insert("tus-version", HeaderValue::from_static(TUS_VERSION));
    Err(Box::new(resp))
}

async fn handle_tus_create(
    State(ctx): State<HttpTransferState>,
    Path(dut_id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(resp) = ctx.validate_host_and_origin(&uri, &headers) {
        return *resp;
    }
    if let Err(resp) = require_tus_resumable(&headers) {
        return *resp;
    }
    let principal =
        match ctx.authenticate_request(&headers, extensions.get::<ConnectInfo<SocketAddr>>()) {
            Ok(p) => p,
            Err(resp) => return *resp,
        };
    if let Err(resp) = ctx.authorize_dut_action(&principal, &dut_id, DutAction::Flash) {
        return *resp;
    }
    if let Err(resp) = ctx.ensure_upload_slot_open(&dut_id) {
        return *resp;
    }
    let Some(total_length) = headers
        .get("upload-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
    else {
        return json_error(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "missing_upload_length",
                "hint": "Send header 'Upload-Length: <total_bytes>'."
            }),
        );
    };
    if total_length == 0 {
        return json_error(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "error": "empty_upload" }),
        );
    }
    if total_length > MAX_IMAGE_UPLOAD_BYTES {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            serde_json::json!({
                "error": "payload_too_large",
                "maxBytes": MAX_IMAGE_UPLOAD_BYTES
            }),
        );
    }
    let metadata = parse_tus_metadata(&headers);
    let Some(raw_filename) = metadata.get("filename") else {
        return json_error(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "missing_filename_metadata",
                "hint": "Send header 'Upload-Metadata: filename <base64(image.img)>'."
            }),
        );
    };
    let clean_name = match duthub_core::dut_artifacts::validate_image_filename(raw_filename) {
        Ok(name) => name.to_owned(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": "invalid_filename",
                    "code": "INVALID_FILE_ID",
                    "detail": err.to_string()
                }),
            );
        }
    };
    let overwrite = metadata
        .get("overwrite")
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1");
    let expected_sha256 = match parse_tus_upload_checksum(&headers) {
        Ok(val) => val,
        Err(msg) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "error": "invalid_upload_checksum", "detail": msg }),
            );
        }
    };
    let (root, dut_name) = match ctx.resolve_dut_target(&dut_id) {
        Ok(pair) => pair,
        Err(resp) => return *resp,
    };
    let upload_id = uuid::Uuid::new_v4().simple().to_string();
    let temp_name = format!(".upload-tus-{upload_id}.part");
    let (_, temp_path) = match duthub_core::dut_artifacts::create_image_upload_temp(
        &root, &dut_id, &dut_name, &temp_name,
    ) {
        Ok(pair) => pair,
        Err(err) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "upload_init_failed", "detail": err.to_string() }),
            );
        }
    };
    let subject = principal_subject(&principal);
    {
        let mut sessions = duthub_core::util::lock_unpoisoned(&ctx.tus_sessions);
        if sessions.len() >= MAX_TUS_SESSIONS {
            duthub_core::dut_artifacts::remove_image_upload_temp(
                &root, &dut_id, &dut_name, &temp_name,
            );
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({ "error": "too_many_active_uploads" }),
            );
        }
        sessions.insert(
            upload_id.clone(),
            TusUploadSession {
                dut_id: dut_id.clone(),
                dut_name,
                root,
                temp_name,
                temp_path,
                final_name: clean_name,
                overwrite,
                subject,
                total_length,
                offset: 0,
                expected_sha256,
                hasher: Sha256::new(),
                busy: false,
            },
        );
    }
    let location = format!("/dut/{dut_id}/transfer/{upload_id}");
    let mut response = StatusCode::CREATED.into_response();
    let hdrs = response.headers_mut();
    hdrs.insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    if let Ok(loc) = HeaderValue::from_str(&location) {
        hdrs.insert(axum::http::header::LOCATION, loc);
    }
    response
}

async fn handle_tus_head(
    State(ctx): State<HttpTransferState>,
    Path((dut_id, upload_id)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(resp) = ctx.validate_host_and_origin(&uri, &headers) {
        return *resp;
    }
    if let Err(resp) = require_tus_resumable(&headers) {
        return *resp;
    }
    let principal =
        match ctx.authenticate_request(&headers, extensions.get::<ConnectInfo<SocketAddr>>()) {
            Ok(p) => p,
            Err(resp) => return *resp,
        };
    if let Err(resp) = ctx.authorize_dut_action(&principal, &dut_id, DutAction::Flash) {
        return *resp;
    }
    let subject = principal_subject(&principal);
    let (offset, total_length) = {
        let sessions = duthub_core::util::lock_unpoisoned(&ctx.tus_sessions);
        let Some(session) = sessions.get(&upload_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if session.dut_id != dut_id || session.subject != subject {
            return json_error(
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": "upload_owner_mismatch" }),
            );
        }
        (session.offset, session.total_length)
    };
    let mut response = StatusCode::OK.into_response();
    let hdrs = response.headers_mut();
    hdrs.insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    hdrs.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    if let Ok(val) = HeaderValue::from_str(&offset.to_string()) {
        hdrs.insert("upload-offset", val);
    }
    if let Ok(val) = HeaderValue::from_str(&total_length.to_string()) {
        hdrs.insert("upload-length", val);
    }
    response
}

type PatchWriteResult = Result<(u64, Vec<Vec<u8>>), (StatusCode, serde_json::Value)>;

async fn handle_tus_patch(
    State(ctx): State<HttpTransferState>,
    Path((dut_id, upload_id)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    extensions: Extensions,
    body: Body,
) -> Response {
    if let Err(resp) = ctx.validate_host_and_origin(&uri, &headers) {
        return *resp;
    }
    if let Err(resp) = require_tus_resumable(&headers) {
        return *resp;
    }
    let principal =
        match ctx.authenticate_request(&headers, extensions.get::<ConnectInfo<SocketAddr>>()) {
            Ok(p) => p,
            Err(resp) => return *resp,
        };
    if let Err(resp) = ctx.authorize_dut_action(&principal, &dut_id, DutAction::Flash) {
        return *resp;
    }
    if let Err(resp) = ctx.ensure_upload_slot_open(&dut_id) {
        return *resp;
    }
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    if content_type != Some("application/offset+octet-stream") {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            serde_json::json!({
                "error": "invalid_content_type",
                "hint": "PATCH requires Content-Type: application/offset+octet-stream."
            }),
        );
    }
    let Some(req_offset) = headers
        .get("upload-offset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
    else {
        return json_error(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "error": "missing_upload_offset" }),
        );
    };
    let subject = principal_subject(&principal);
    let (temp_path, start_offset, total_length) = {
        let mut sessions = duthub_core::util::lock_unpoisoned(&ctx.tus_sessions);
        let Some(session) = sessions.get_mut(&upload_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if session.dut_id != dut_id || session.subject != subject {
            return json_error(
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": "upload_owner_mismatch" }),
            );
        }
        if session.busy {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({ "error": "upload_busy" }),
            );
        }
        if req_offset != session.offset {
            return json_error(
                StatusCode::CONFLICT,
                serde_json::json!({
                    "error": "offset_mismatch",
                    "expectedOffset": session.offset,
                    "providedOffset": req_offset
                }),
            );
        }
        session.busy = true;
        (
            session.temp_path.clone(),
            session.offset,
            session.total_length,
        )
    };

    // Stream chunk bytes outside the session lock so no Mutex is ever held across `.await`.
    let write_outcome: PatchWriteResult = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&temp_path)
            .await
            .map_err(|err| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "upload_open_failed", "detail": err.to_string() }),
                )
            })?;
        file.seek(std::io::SeekFrom::Start(start_offset))
            .await
            .map_err(|err| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "upload_seek_failed", "detail": err.to_string() }),
                )
            })?;
        let mut added: u64 = 0;
        let mut hash_chunks: Vec<Vec<u8>> = Vec::new();
        let mut stream = body;
        while let Some(frame_result) = stream.frame().await {
            let frame = frame_result.map_err(|err| {
                (
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({ "error": "upload_stream_error", "detail": err.to_string() }),
                )
            })?;
            if let Ok(data) = frame.into_data() {
                let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
                if start_offset.saturating_add(added).saturating_add(len) > total_length {
                    return Err((
                        StatusCode::CONFLICT,
                        serde_json::json!({
                            "error": "upload_exceeds_declared_length",
                            "declaredLength": total_length
                        }),
                    ));
                }
                file.write_all(&data).await.map_err(|err| {
                    (
                        StatusCode::INSUFFICIENT_STORAGE,
                        serde_json::json!({ "error": "upload_write_failed", "detail": err.to_string() }),
                    )
                })?;
                added = added.saturating_add(len);
                hash_chunks.push(data.to_vec());
            }
        }
        file.flush().await.map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "upload_flush_failed", "detail": err.to_string() }),
            )
        })?;
        Ok((added, hash_chunks))
    }
    .await;

    let (added, hash_chunks) = match write_outcome {
        Ok(res) => res,
        Err((status, payload)) => {
            if let Some(session) =
                duthub_core::util::lock_unpoisoned(&ctx.tus_sessions).get_mut(&upload_id)
            {
                session.busy = false;
            }
            return json_error(status, payload);
        }
    };

    let completed = {
        let mut sessions = duthub_core::util::lock_unpoisoned(&ctx.tus_sessions);
        let Some(session) = sessions.get_mut(&upload_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        for chunk in &hash_chunks {
            session.hasher.update(chunk);
        }
        session.offset = session.offset.saturating_add(added);
        session.busy = false;
        let new_offset = session.offset;
        if new_offset < session.total_length {
            let mut response = StatusCode::NO_CONTENT.into_response();
            let hdrs = response.headers_mut();
            hdrs.insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
            if let Ok(val) = HeaderValue::from_str(&new_offset.to_string()) {
                hdrs.insert("upload-offset", val);
            }
            return response;
        }
        sessions.remove(&upload_id)
    };

    let Some(session) = completed else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let actual_sha256 = duthub_core::util::hex_lower(&session.hasher.finalize());
    if let Some(expected) = session.expected_sha256.as_deref()
        && expected != actual_sha256
    {
        duthub_core::dut_artifacts::remove_image_upload_temp(
            &session.root,
            &session.dut_id,
            &session.dut_name,
            &session.temp_name,
        );
        ctx.audit_upload(
            &principal,
            &dut_id,
            AuditOutcome::Denied,
            &format!("tus {} checksum mismatch", session.final_name),
        );
        let status = StatusCode::from_u16(460).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY);
        return json_error(
            status,
            serde_json::json!({
                "error": "checksum_mismatch",
                "expectedSha256": expected,
                "actualSha256": actual_sha256
            }),
        );
    }
    match duthub_core::dut_artifacts::commit_image_upload_temp(
        &session.root,
        &session.dut_id,
        &session.dut_name,
        &session.temp_name,
        &session.final_name,
        session.overwrite,
    ) {
        Ok(file_id) => {
            ctx.audit_upload(
                &principal,
                &dut_id,
                AuditOutcome::Ok,
                &format!(
                    "tus {file_id} bytes={} sha256={actual_sha256}",
                    session.total_length
                ),
            );
            let mut response = StatusCode::NO_CONTENT.into_response();
            let hdrs = response.headers_mut();
            hdrs.insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
            if let Ok(val) = HeaderValue::from_str(&session.offset.to_string()) {
                hdrs.insert("upload-offset", val);
            }
            if let Ok(val) = HeaderValue::from_str(&format!("sha256:{actual_sha256}")) {
                hdrs.insert("x-duthub-checksum", val);
            }
            if let Ok(val) = HeaderValue::from_str(&file_id) {
                hdrs.insert("x-duthub-file-id", val);
            }
            response
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => json_error(
            StatusCode::CONFLICT,
            serde_json::json!({
                "error": "image_already_exists",
                "detail": err.to_string(),
                "hint": "Choose a new filename or pass Upload-Metadata 'overwrite dHJ1ZQ==' to replace."
            }),
        ),
        Err(err) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": "upload_commit_failed", "detail": err.to_string() }),
        ),
    }
}

async fn handle_tus_delete(
    State(ctx): State<HttpTransferState>,
    Path((dut_id, upload_id)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(resp) = ctx.validate_host_and_origin(&uri, &headers) {
        return *resp;
    }
    if let Err(resp) = require_tus_resumable(&headers) {
        return *resp;
    }
    let principal =
        match ctx.authenticate_request(&headers, extensions.get::<ConnectInfo<SocketAddr>>()) {
            Ok(p) => p,
            Err(resp) => return *resp,
        };
    if let Err(resp) = ctx.authorize_dut_action(&principal, &dut_id, DutAction::Flash) {
        return *resp;
    }
    let subject = principal_subject(&principal);
    let removed = {
        let mut sessions = duthub_core::util::lock_unpoisoned(&ctx.tus_sessions);
        let Some(existing) = sessions.get(&upload_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if existing.dut_id != dut_id || existing.subject != subject {
            return json_error(
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": "upload_owner_mismatch" }),
            );
        }
        if existing.busy {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({ "error": "upload_busy" }),
            );
        }
        sessions.remove(&upload_id)
    };
    if let Some(session) = removed {
        duthub_core::dut_artifacts::remove_image_upload_temp(
            &session.root,
            &session.dut_id,
            &session.dut_name,
            &session.temp_name,
        );
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    response
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogsExportQuery {
    since_segment_id: Option<String>,
    tail_segments: Option<usize>,
    strip_ansi: Option<bool>,
    format: Option<String>,
}

async fn handle_logs_export(
    State(ctx): State<HttpTransferState>,
    Path(dut_id): Path<String>,
    Query(query): Query<LogsExportQuery>,
    uri: Uri,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(resp) = ctx.validate_host_and_origin(&uri, &headers) {
        return *resp;
    }
    let principal =
        match ctx.authenticate_request(&headers, extensions.get::<ConnectInfo<SocketAddr>>()) {
            Ok(p) => p,
            Err(resp) => return *resp,
        };
    if let Err(resp) = ctx.authorize_dut_action(&principal, &dut_id, DutAction::Read) {
        return *resp;
    }
    let Some(log_dir) = ctx.state.log_archive_dir() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "error": "log_archive_unavailable" }),
        );
    };
    let strip_ansi = query.strip_ansi.unwrap_or(true);
    let gzip = query
        .format
        .as_deref()
        .is_some_and(|fmt| fmt.eq_ignore_ascii_case("gzip") || fmt.eq_ignore_ascii_case("gz"));
    let since = query.since_segment_id;
    let tail = query.tail_segments;
    let dut_for_job = dut_id.clone();
    let exported = tokio::task::spawn_blocking(move || {
        build_snapshot_log_export(
            &log_dir,
            &dut_for_job,
            since.as_deref(),
            tail,
            strip_ansi,
            gzip,
        )
    })
    .await;
    let Ok(Ok(bundle)) = exported else {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": "log_export_failed" }),
        );
    };
    let content_type = if gzip {
        "application/gzip"
    } else {
        "text/plain; charset=utf-8"
    };
    let mut response = (StatusCode::OK, Body::from(bundle.payload)).into_response();
    let hdrs = response.headers_mut();
    hdrs.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    if let Ok(val) = HeaderValue::from_str(&bundle.segment_count.to_string()) {
        hdrs.insert("x-duthub-segments", val);
    }
    if let Ok(val) = HeaderValue::from_str(&bundle.total_raw_bytes.to_string()) {
        hdrs.insert("x-duthub-total-bytes", val);
    }
    if let Ok(val) = HeaderValue::from_str(&bundle.sha256) {
        hdrs.insert("x-duthub-sha256", val);
    }
    response
}

pub(crate) struct ExportedLogBundle {
    pub(crate) segment_count: usize,
    pub(crate) total_raw_bytes: u64,
    pub(crate) sha256: String,
    pub(crate) payload: Vec<u8>,
}

pub(crate) fn build_snapshot_log_export(
    log_dir: &std::path::Path,
    dut_id: &str,
    since_segment_id: Option<&str>,
    tail_segments: Option<usize>,
    strip_ansi: bool,
    gzip: bool,
) -> std::io::Result<ExportedLogBundle> {
    let segments = duthub_core::log_archive::snapshot_segments(
        log_dir,
        dut_id,
        since_segment_id,
        tail_segments,
    );
    let mut combined = Vec::new();
    let mut total_raw_bytes: u64 = 0;
    for seg in &segments {
        let raw =
            duthub_core::log_archive::read_segment_bounded(log_dir, dut_id, &seg.id, seg.bytes)?;
        total_raw_bytes = total_raw_bytes.saturating_add(u64::try_from(raw.len()).unwrap_or(0));
        let header = format!("=== segment: {} ({} bytes) ===\n", seg.id, raw.len());
        combined.extend_from_slice(header.as_bytes());
        if strip_ansi {
            let text = String::from_utf8_lossy(&raw);
            let clean = crate::keys::strip_ansi_codes(&text);
            combined.extend_from_slice(clean.as_bytes());
        } else {
            combined.extend_from_slice(&raw);
        }
        if !combined.ends_with(b"\n") {
            combined.push(b'\n');
        }
    }
    let payload = if gzip {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&combined)?;
        encoder.finish()?
    } else {
        combined
    };
    let sha256 = duthub_core::util::hex_lower(&Sha256::digest(&payload));
    Ok(ExportedLogBundle {
        segment_count: segments.len(),
        total_raw_bytes,
        sha256,
        payload,
    })
}

fn principal_subject(principal: &Principal) -> String {
    match principal {
        Principal::LocalAdmin => "local-admin".into(),
        Principal::Agent { credential_id, .. } => credential_id.clone(),
    }
}

fn normalize_host(host: &str) -> String {
    host.trim_matches('[')
        .trim_matches(']')
        .to_ascii_lowercase()
}

fn parse_allowed_entry(allowed: &str) -> (String, Option<u16>) {
    let allowed = allowed.trim();
    if let Ok(authority) = http::uri::Authority::try_from(allowed) {
        return (normalize_host(authority.host()), authority.port_u16());
    }
    (normalize_host(allowed), None)
}

fn parse_optional_sha256_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<String>, String> {
    let Some(raw) = headers.get(name) else {
        return Ok(None);
    };
    let text = raw
        .to_str()
        .map_err(|_| format!("{name} must be valid ASCII"))?
        .trim()
        .to_ascii_lowercase();
    let hex = text.strip_prefix("sha256:").unwrap_or(&text);
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{name} must be a 64-character hex SHA-256 digest"));
    }
    Ok(Some(hex.to_owned()))
}

fn parse_tus_metadata(headers: &HeaderMap) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(raw) = headers.get("upload-metadata").and_then(|v| v.to_str().ok()) else {
        return map;
    };
    for pair in raw.split(',') {
        let trimmed = pair.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let value = match parts.next() {
            Some(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_default(),
            None => String::new(),
        };
        map.insert(key.to_ascii_lowercase(), value);
    }
    map
}

fn parse_tus_upload_checksum(headers: &HeaderMap) -> Result<Option<String>, String> {
    let Some(raw) = headers.get("upload-checksum") else {
        return Ok(None);
    };
    let text = raw
        .to_str()
        .map_err(|_| "Upload-Checksum must be valid ASCII".to_string())?
        .trim();
    let mut parts = text.split_whitespace();
    let Some(algo) = parts.next() else {
        return Err("Upload-Checksum must be '<algorithm> <digest>'".into());
    };
    let Some(digest) = parts.next() else {
        return Err("Upload-Checksum is missing digest value".into());
    };
    if !algo.eq_ignore_ascii_case("sha256") {
        return Err(format!(
            "unsupported Upload-Checksum algorithm '{algo}'; supported: sha256"
        ));
    }
    let lowered = digest.to_ascii_lowercase();
    if lowered.len() == 64 && lowered.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Some(lowered));
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(digest)
        .map_err(|_| "Upload-Checksum sha256 must be 64-char hex or base64".to_string())?;
    if decoded.len() != 32 {
        return Err("Upload-Checksum sha256 base64 must decode to 32 bytes".into());
    }
    Ok(Some(duthub_core::util::hex_lower(&decoded)))
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    (status, axum::Json(body)).into_response()
}

fn json_error(status: StatusCode, body: serde_json::Value) -> Response {
    json_response(status, body)
}
