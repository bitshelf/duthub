//! Shared helpers for the duthub-transfer integration tests.
//!
//! Every test runs against its own `tempfile::TempDir` and an ephemeral
//! loopback port — never against the repository's `duthub-data/`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duthub_transfer::authorize::{StaticTokenAuthorizer, UploadScope};
pub use duthub_transfer::config::TransferConfig;
use duthub_transfer::{TransferListenerHandle, TransferServer};
use salvo::tus::Metadata;

pub const TOKEN_A: &str = "tok-agent-a";
pub const TOKEN_B: &str = "tok-agent-b";

pub const TUS_VERSION: &str = "1.0.0";
pub const CHUNK_CONTENT_TYPE: &str = "application/offset+octet-stream";

/// The publish name every default creation uses.
pub const PUBLISH_NAME: &str = "flash.img";

pub fn scope_for(subject: &str) -> UploadScope {
    UploadScope {
        subject: subject.to_string(),
        boards: vec!["board1".to_string(), "board2".to_string()],
        max_bytes: 1 << 20,
        expires_at_epoch: None,
    }
}

/// Two subjects on the same boards — the cross-subject 403 matrix needs both.
pub fn authorizer() -> Arc<StaticTokenAuthorizer> {
    Arc::new(StaticTokenAuthorizer::new(HashMap::from([
        (TOKEN_A.to_string(), scope_for("agent-a")),
        (TOKEN_B.to_string(), scope_for("agent-b")),
    ])))
}

/// Small test limits: 2 MiB upload cap, no reserved margin, 1 s lock wait.
pub fn test_config(dir: &Path) -> TransferConfig {
    TransferConfig {
        transfer_dir: Some(dir.to_string_lossy().to_string()),
        max_upload_bytes: 2 * (1 << 20),
        reserved_disk_bytes: 0,
        lock_timeout_secs: 1,
        ..TransferConfig::default()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hex_digest(&hasher.finalize())
}

pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Wire-format `Upload-Metadata` value, built with the same encoder the
/// receiver uses (values are base64-encoded).
pub fn metadata_header(pairs: &[(&str, &str)]) -> String {
    let mut metadata = Metadata::default();
    for (key, value) in pairs {
        metadata.insert((*key).to_owned(), Some(value.as_bytes().to_vec()));
    }
    Metadata::stringify(metadata)
}

/// Metadata of a default creation: a publish name on an in-scope board.
pub fn default_metadata() -> String {
    metadata_header(&[
        ("filename", PUBLISH_NAME),
        ("board", "board1"),
        ("imageType", "img"),
    ])
}

/// Spawn a server with two test tokens on an ephemeral loopback port.
/// Returns the server (kept alive), the lifecycle handle and the base URL.
pub async fn spawn_server(
    dir: &Path,
    config_override: Option<TransferConfig>,
) -> (TransferServer, TransferListenerHandle, String) {
    let config = config_override.unwrap_or_else(|| test_config(dir));
    let server = TransferServer::new_in(dir, config, Some(authorizer())).unwrap();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    (server, handle, base)
}

/// A plain HTTP/1.1 client (what `curl` speaks).
pub fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

/// A prior-knowledge HTTP/2 client over cleartext TCP: the transport the
/// `ztus` CLI is built on (`reqwest::Client::http2_prior_knowledge`).
pub fn h2c_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap()
}

fn authorize(request: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(token) => request.header("Authorization", format!("Bearer {token}")),
        None => request,
    }
}

/// POST a new upload (creation), with optional extra headers.
pub async fn post_upload_with(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    length: u64,
    metadata: &str,
    extra: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = client
        .post(base)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Upload-Length", length);
    if !metadata.is_empty() {
        request = request.header("Upload-Metadata", metadata);
    }
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    authorize(request, token).send().await.unwrap()
}

/// POST a new upload (creation).
pub async fn post_upload(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    length: u64,
    metadata: &str,
) -> reqwest::Response {
    post_upload_with(client, base, token, length, metadata, &[]).await
}

/// PATCH a chunk at `offset`.
pub async fn patch_bytes(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    offset: u64,
    bytes: Vec<u8>,
) -> reqwest::Response {
    let request = client
        .patch(url)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Upload-Offset", offset)
        .header("Content-Type", CHUNK_CONTENT_TYPE)
        .body(bytes);
    authorize(request, token).send().await.unwrap()
}

/// HEAD the upload; returns (status, Upload-Offset).
pub async fn head_offset(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> (reqwest::StatusCode, Option<u64>) {
    let request = client.head(url).header("Tus-Resumable", TUS_VERSION);
    let response = authorize(request, token).send().await.unwrap();
    let offset = response
        .headers()
        .get("upload-offset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    (response.status(), offset)
}

/// DELETE the upload.
pub async fn delete_upload(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> reqwest::Response {
    let request = client.delete(url).header("Tus-Resumable", TUS_VERSION);
    authorize(request, token).send().await.unwrap()
}

/// Convenience creation with the default metadata and an optional
/// `Upload-Checksum` value: returns (status, upload id parsed from Location).
pub async fn create_upload(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    length: u64,
    checksum: Option<&str>,
) -> (reqwest::StatusCode, Option<String>) {
    let extra: Vec<(&str, &str)> = checksum
        .map(|value| vec![("Upload-Checksum", value)])
        .unwrap_or_default();
    let response = post_upload_with(
        client,
        base,
        Some(token),
        length,
        &default_metadata(),
        &extra,
    )
    .await;
    (response.status(), location_id(&response))
}

/// Upload id of a creation response.
pub fn location_id(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit('/').next())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Header value as text, for assertions.
pub fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_owned()
}

/// Resolve a (possibly relative) `Location` against the mount, the way tus
/// clients do.
pub fn upload_url(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_owned();
    }
    let origin = base.rsplit_once("/").map_or(base, |(origin, _)| origin);
    format!("{origin}{location}")
}

/// On-disk path of a finalized upload.
pub fn published_path(dir: &Path, name: &str) -> PathBuf {
    dir.join("data").join(name)
}

/// Deterministic pseudo-random payload (no RNG dependency).
pub fn payload(len: usize, modulus: u32) -> Vec<u8> {
    (0..len as u32).map(|i| (i % modulus) as u8).collect()
}
