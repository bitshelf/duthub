//! The `ztus` CLI's wire behavior, replayed over its transport.
//!
//! ztus talks prior-knowledge HTTP/2 over cleartext TCP (`h2c`) and relies on
//! four things from the server: an anonymous OPTIONS, a relative `Location` it
//! resolves against the base URL, `200` — not `204` — from the HEAD it issues
//! before resuming, and an `Upload-Offset` header on every acked chunk. It
//! declares the whole-file digest once, on the creation request.
//!
//! These tests drive the real listener through the same HTTP stack ztus is
//! built on (`reqwest` with `http2_prior_knowledge`), so they also pin the
//! `http2-cleartext` Salvo feature the receiver depends on.

mod common;

use common::*;
use duthub_transfer::TransferListenerHandle;
use duthub_transfer::TransferServer;
use duthub_transfer::authorize::{StaticTokenAuthorizer, UploadScope};
use salvo::http::StatusCode;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// A scope wide enough for a multi-chunk upload (the default test scope caps
/// at 1 MiB).
async fn spawn_ztus_shaped_server(dir: &Path) -> (TransferServer, TransferListenerHandle, String) {
    let config = TransferConfig {
        max_upload_bytes: 8 << 20,
        ..test_config(dir)
    };
    let authorizer = Arc::new(StaticTokenAuthorizer::new(HashMap::from([(
        TOKEN_A.to_string(),
        UploadScope {
            subject: "agent-a".to_string(),
            // `ztus` talks to a single-DUT receiver and only sends `filename` metadata.
            boards: Vec::new(),
            max_bytes: 8 << 20,
            expires_at_epoch: None,
        },
    )])));
    let server = TransferServer::new_in(dir, config, Some(authorizer)).unwrap();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    (server, handle, base)
}

/// The request ztus sends to create an upload: the filename comes from the
/// file being uploaded, the digest is the whole-file SHA-256.
async fn ztus_create(
    ztus: &reqwest::Client,
    base: &str,
    filename: &str,
    length: u64,
    checksum: Option<&str>,
) -> reqwest::Response {
    let extra: Vec<(&str, &str)> =
        checksum.map_or_else(Vec::new, |value| vec![("Upload-Checksum", value)]);
    post_upload_with(
        ztus,
        base,
        Some(TOKEN_A),
        length,
        &metadata_header(&[("filename", filename)]),
        &extra,
    )
    .await
}

#[tokio::test]
async fn prior_knowledge_h2c_client_uploads_and_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_ztus_shaped_server(tmp.path()).await;
    let ztus = h2c_client();

    // discover_capabilities(): anonymous OPTIONS.
    let response = ztus
        .request(reqwest::Method::OPTIONS, &base)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        204,
        "h2c must reach the tus router"
    );

    // start_new_upload().
    let payload = payload(3 * 1024 * 1024 + 12_345, 251);
    let expected = sha256_hex(&payload);
    let response = ztus_create(
        &ztus,
        &base,
        "boot.img",
        payload.len() as u64,
        Some(&format!("sha256 {expected}")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = header(&response, "location");
    assert!(
        location.starts_with("/transfer/"),
        "ztus resolves a relative Location against the base URL: {location}"
    );
    let url = upload_url(&base, &location);

    // get_upload_offset() only understands 200.
    let (status, offset) = head_offset(&ztus, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offset, Some(0));

    // upload_chunk(): adaptive-sized chunks, each acked with Upload-Offset.
    let mut offset = 0u64;
    let mut last = None;
    for chunk in payload.chunks(700_000) {
        let response = patch_bytes(&ztus, &url, Some(TOKEN_A), offset, chunk.to_vec()).await;
        assert_eq!(response.status().as_u16(), 204);
        offset += chunk.len() as u64;
        assert_eq!(header(&response, "upload-offset"), offset.to_string());
        last = Some(response);
    }
    assert_eq!(offset, payload.len() as u64);
    assert_eq!(
        header(&last.unwrap(), "x-duthub-checksum"),
        format!("sha256:{expected}")
    );
    assert_eq!(
        std::fs::read(published_path(tmp.path(), "boot.img")).unwrap(),
        payload,
        "finalized under the name ztus sent"
    );

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn prior_knowledge_h2c_client_resumes_from_head() {
    // The resume path: a first run is cut after one chunk, a second run asks
    // HEAD where the server is and continues from there.
    let tmp = tempfile::tempdir().unwrap();
    let (server, handle, base) = spawn_ztus_shaped_server(tmp.path()).await;
    let ztus = h2c_client();

    let payload = payload(1_500_000, 247);
    let expected = sha256_hex(&payload);
    let response = ztus_create(
        &ztus,
        &base,
        "boot.img",
        payload.len() as u64,
        Some(&format!("sha256 {expected}")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let url = upload_url(&base, &header(&response, "location"));

    let response = patch_bytes(&ztus, &url, Some(TOKEN_A), 0, payload[..900_000].to_vec()).await;
    assert_eq!(response.status().as_u16(), 204);
    assert_eq!(server.store().inflight_remaining_bytes().await, 600_000);

    // Second run: HEAD first, PATCH the remainder.
    let (status, offset) = head_offset(&ztus, &url, Some(TOKEN_A)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a 204 here would abort a ztus resume"
    );
    let offset = offset.unwrap();
    let response = patch_bytes(
        &ztus,
        &url,
        Some(TOKEN_A),
        offset,
        payload[usize::try_from(offset).unwrap()..].to_vec(),
    )
    .await;
    assert_eq!(response.status().as_u16(), 204);
    assert_eq!(
        header(&response, "x-duthub-checksum"),
        format!("sha256:{expected}")
    );
    assert_eq!(
        std::fs::read(published_path(tmp.path(), "boot.img")).unwrap(),
        payload
    );

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn prior_knowledge_h2c_client_can_skip_verification() {
    // `ztus upload ... --no-checksum`: nothing is declared, nothing is hashed.
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_ztus_shaped_server(tmp.path()).await;
    let ztus = h2c_client();

    let payload = payload(500_000, 251);
    let response = ztus_create(&ztus, &base, "boot.img", payload.len() as u64, None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let url = upload_url(&base, &header(&response, "location"));
    let response = patch_bytes(&ztus, &url, Some(TOKEN_A), 0, payload.clone()).await;
    assert_eq!(response.status().as_u16(), 204);
    assert!(response.headers().get("x-duthub-checksum").is_none());
    assert_eq!(
        std::fs::read(published_path(tmp.path(), "boot.img")).unwrap(),
        payload
    );

    handle.shutdown_and_join().await;
}
