//! Resumption: restart the server mid-upload and verify that the persisted
//! offset, the uncommitted-tail handling and the whole-file digest all hold.
//!
//! The record the upstream disk store commits after every chunk is the only
//! truth for the acknowledged offset; nothing is cached in this crate.

mod common;

use common::*;
use salvo::http::StatusCode;
use std::path::{Path, PathBuf};

/// Payload file of an upload that is still in flight.
fn in_flight_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("data").join(format!("{id}.bin"))
}

#[tokio::test]
async fn resumed_upload_across_restart_publishes_the_correct_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = payload(200_000, 253);
    let expected = blake3_hex(&payload);
    let (first, second) = payload.split_at(120_000);

    // First server: create + write half, then shut down.
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(
        &http,
        &base,
        TOKEN_A,
        payload.len() as u64,
        Some(&format!("blake3 {expected}")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let response = patch_bytes(
        &http,
        &format!("{base}/{id}"),
        Some(TOKEN_A),
        0,
        first.to_vec(),
    )
    .await;
    assert_eq!(response.status().as_u16(), 204);
    handle.shutdown_and_join().await;
    drop(server);

    // Second server on the SAME directory resumes from the committed record.
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let url = format!("{base}/{id}");
    let (status, offset) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offset, Some(120_000), "HEAD reports the persisted offset");

    // The digest the first server recorded is still the one in force.
    let head = http
        .head(&url)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .send()
        .await
        .unwrap();
    let echoed = header(&head, "upload-metadata");
    let expected_entry = format!("checksum {}", base64(&format!("blake3:{expected}")));
    assert!(
        echoed.contains(&expected_entry),
        "declared digest survived the restart: {echoed:?} vs {expected_entry:?}"
    );

    let response = patch_bytes(&http, &url, Some(TOKEN_A), 120_000, second.to_vec()).await;
    assert_eq!(
        response.status().as_u16(),
        204,
        "resume completes and verifies"
    );
    assert_eq!(
        header(&response, "x-duthub-checksum"),
        format!("blake3:{expected}")
    );

    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        payload
    );
    assert!(
        !in_flight_path(tmp.path(), &id).exists(),
        "finalize moved the payload to its upload name"
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn dirty_tail_beyond_acknowledged_offset_is_discarded_on_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = payload(100_000, 249);
    let expected = blake3_hex(&payload);
    let (first, second) = payload.split_at(50_000);

    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(
        &http,
        &base,
        TOKEN_A,
        payload.len() as u64,
        Some(&format!("blake3 {expected}")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let response = patch_bytes(
        &http,
        &format!("{base}/{id}"),
        Some(TOKEN_A),
        0,
        first.to_vec(),
    )
    .await;
    assert_eq!(response.status().as_u16(), 204);
    handle.shutdown_and_join().await;

    // Simulate a crash mid-next-chunk: garbage bytes beyond the acknowledged
    // offset are on disk but were never committed.
    let part = in_flight_path(tmp.path(), &id);
    let confirmed = std::fs::read(&part).unwrap();
    assert_eq!(confirmed.len(), 50_000);
    std::fs::write(&part, [confirmed, b"UNCONFIRMED-GARBAGE".to_vec()].concat()).unwrap();

    // Restart keeps the acknowledged offset as the only truth.
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let url = format!("{base}/{id}");
    let (status, offset) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        offset,
        Some(50_000),
        "acknowledged offset is the only truth"
    );

    let response = patch_bytes(&http, &url, Some(TOKEN_A), 50_000, second.to_vec()).await;
    assert_eq!(response.status().as_u16(), 204);
    assert_eq!(
        header(&response, "x-duthub-checksum"),
        format!("blake3:{expected}")
    );

    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        payload
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn same_server_chunked_upload_also_matches_digest() {
    // In-process multi-chunk upload: the digest covers everything, whatever
    // the chunk boundaries.
    let tmp = tempfile::tempdir().unwrap();
    let payload = payload(64_000, 241);
    let expected = blake3_hex(&payload);
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(
        &http,
        &base,
        TOKEN_A,
        payload.len() as u64,
        Some(&format!("blake3 {expected}")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let url = format!("{base}/{}", id.unwrap());
    for (offset, chunk) in [
        (0, &payload[..20_000]),
        (20_000, &payload[20_000..45_000]),
        (45_000, &payload[45_000..]),
    ] {
        let response = patch_bytes(&http, &url, Some(TOKEN_A), offset, chunk.to_vec()).await;
        assert_eq!(response.status().as_u16(), 204);
    }
    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        payload
    );
    assert_eq!(
        server.store().ids().len(),
        1,
        "one upload, however many chunks"
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn verification_record_survives_a_restart() {
    // The marker is what keeps a finished upload from being hashed again, so
    // it must outlive the process that wrote it.
    let tmp = tempfile::tempdir().unwrap();
    let payload = payload(40_000, 237);
    let expected = blake3_hex(&payload);
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(
        &http,
        &base,
        TOKEN_A,
        payload.len() as u64,
        Some(&format!("blake3 {expected}")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let response = patch_bytes(
        &http,
        &format!("{base}/{id}"),
        Some(TOKEN_A),
        0,
        payload.clone(),
    )
    .await;
    assert_eq!(response.status().as_u16(), 204);
    assert!(server.store().verified_marker(&id).unwrap().exists());
    handle.shutdown_and_join().await;

    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let retry = patch_bytes(
        &http,
        &format!("{base}/{id}"),
        Some(TOKEN_A),
        payload.len() as u64,
        Vec::new(),
    )
    .await;
    assert_eq!(retry.status().as_u16(), 204);
    assert_eq!(
        header(&retry, "x-duthub-checksum"),
        format!("blake3:{expected}")
    );
    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        payload
    );
    handle.shutdown_and_join().await;
}

/// Base64 of a metadata value, as it appears in an `Upload-Metadata` header.
fn base64(value: &str) -> String {
    metadata_header(&[("x", value)])
        .strip_prefix("x ")
        .unwrap()
        .to_owned()
}
