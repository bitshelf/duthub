//! Failure paths: offset conflicts, digest mismatches, budget exhaustion,
//! lock contention, expiry, termination, name attacks and ownership of
//! server-bound metadata.
//!
//! Status codes come from the upstream tus layer (salvo's `tus` module) except
//! where the guard answers itself: 400 for names and checksum declarations,
//! 403 for scope, 460 for a digest mismatch, 507 for the disk budget.

mod common;

use common::*;
use duthub_transfer::authorize::UploadScope;
use duthub_transfer::store::FreeSpace;
use salvo::http::StatusCode;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Fake free-space probe: makes the 507 budget path deterministic.
struct FixedFreeSpace(AtomicU64);

impl FreeSpace for FixedFreeSpace {
    fn available_bytes(&self, _dir: &Path) -> std::io::Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[tokio::test]
async fn offset_regression_and_lead_return_409_and_keep_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = payload(50_000, 239);
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(&http, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let url = format!("{base}/{id}");

    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, payload[..20_000].to_vec()).await;
    assert_eq!(response.status().as_u16(), 204);

    // Regression: offset 0 when 20_000 are acknowledged → 409.
    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, payload[..5_000].to_vec()).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    // Lead: offset beyond the acknowledged value → 409.
    let response = patch_bytes(
        &http,
        &url,
        Some(TOKEN_A),
        30_000,
        payload[30_000..40_000].to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // Bytes untouched by the rejected chunks.
    let staged = std::fs::read(tmp.path().join("data").join(format!("{id}.bin"))).unwrap();
    assert_eq!(
        staged.len(),
        20_000,
        "rejected chunks must not change the file"
    );
    let (status, offset) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offset, Some(20_000));
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn percent_encoded_ids_cannot_bypass_the_subject_check() {
    // Regression: the router percent-decodes path segments, so `{id}` reached
    // as `/%63…` names the same upload as `/c…`. The guard must authorize the
    // id the tus layer will act on — or refuse the spelling outright — never
    // look up the raw text, miss, and wave the request through.
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let payload = b"owned by agent-a".to_vec();
    let (status, id) = create_upload(&http, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let first = id.chars().next().unwrap() as u8;
    let encoded = format!("{base}/%{first:02x}{}", &id[1..]);

    for (token, label) in [(TOKEN_B, "foreign subject"), (TOKEN_A, "owner")] {
        let (status, _) = head_offset(&http, &encoded, Some(token)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "HEAD {label}");
        let response = patch_bytes(&http, &encoded, Some(token), 0, payload.clone()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "PATCH {label}");
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"],
            "invalid_upload_id"
        );
        let response = delete_upload(&http, &encoded, Some(token)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "DELETE {label}");
    }
    // The upload is untouched; its canonical spelling still works for the owner.
    let (status, offset) = head_offset(&http, &format!("{base}/{id}"), Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offset, Some(0));
    // Deeper paths below the mount never reach the tus handlers: Salvo answers
    // 404 for a route that does not match, and the guard's 400 is the other
    // safe outcome. What matters is that neither is a success.
    let response = patch_bytes(
        &http,
        &format!("{base}/{id}/extra"),
        Some(TOKEN_A),
        0,
        payload,
    )
    .await;
    assert!(
        matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
        ),
        "unexpected {} for a non-canonical path",
        response.status()
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn checksum_mismatch_is_a_terminal_460_without_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let payload = b"definitely not the declared digest".to_vec();
    // Declared BLAKE3 intentionally wrong.
    let (status, id) = create_upload(
        &http,
        &base,
        TOKEN_A,
        payload.len() as u64,
        Some(&format!("blake3 {}", "a".repeat(64))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let url = format!("{base}/{id}");

    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, payload).await;
    assert_eq!(
        response.status().as_u16(),
        460,
        "digest mismatch is terminal"
    );
    // The acked offset must not survive a discarded upload.
    assert!(response.headers().get("upload-offset").is_none());
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["error"], "checksum_mismatch");
    assert_eq!(body["detail"], "checksum mismatch");
    assert_eq!(body["algorithm"], "blake3");
    assert_eq!(body["expected"], "a".repeat(64));
    assert_eq!(
        body["actual"],
        blake3_hex(b"definitely not the declared digest")
    );

    // Staging discarded, HEAD → 404, no record, no marker: do NOT retry.
    assert!(!tmp.path().join("data").join(PUBLISH_NAME).exists());
    assert!(!tmp.path().join("data").join(format!("{id}.bin")).exists());
    assert!(!server.store().verified_marker(&id).unwrap().exists());
    assert!(
        !server.store().failed_marker(&id).unwrap().exists(),
        "discarded cleanly, no flag left"
    );
    assert_eq!(server.store().ids(), Vec::<String>::new());
    let (status, _) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn sha256_mismatch_is_terminal_too() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(
        &http,
        &base,
        TOKEN_A,
        9,
        Some(&format!("sha256 {}", "b".repeat(64))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let response = patch_bytes(
        &http,
        &format!("{base}/{}", id.unwrap()),
        Some(TOKEN_A),
        0,
        b"123456789".to_vec(),
    )
    .await;
    assert_eq!(response.status().as_u16(), 460);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["algorithm"],
        "sha256"
    );
    assert_eq!(server.store().ids(), Vec::<String>::new());
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn missing_filename_metadata_is_rejected_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let metadata = metadata_header(&[("board", "board1")]);
    let response = post_upload(&http, &base, Some(TOKEN_A), 1024, &metadata).await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "missing filename → 400"
    );
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"],
        "invalid_metadata"
    );
    // No metadata at all is just as incomplete.
    let response = post_upload(&http, &base, Some(TOKEN_A), 1024, "").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn checksum_declarations_are_validated_up_front() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let cases: [(&str, &str); 4] = [
        (
            &format!("sha1 {}", "a".repeat(40)),
            "unsupported_checksum_algorithm",
        ),
        ("blake3 nothex", "invalid_checksum"),
        (&format!("blake3 {}", "a".repeat(63)), "invalid_checksum"),
        ("sha256", "invalid_checksum"),
    ];
    for (declaration, expected) in cases {
        let response = post_upload_with(
            &http,
            &base,
            Some(TOKEN_A),
            1024,
            &default_metadata(),
            &[("Upload-Checksum", declaration)],
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{declaration:?}"
        );
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["error"], expected, "{declaration:?}");
        if expected == "unsupported_checksum_algorithm" {
            // ztus falls back from sha1 to sha256 only when the server names
            // the problem in the body.
            assert!(
                body["detail"]
                    .as_str()
                    .unwrap()
                    .contains("unsupported checksum algorithm"),
                "{body}"
            );
        }
    }
    // None of the rejected creations left a record behind.
    assert_eq!(server.store().ids(), Vec::<String>::new());
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn oversized_declared_length_is_413() {
    let tmp = tempfile::tempdir().unwrap();
    // Pin the layering: the scope ceiling answers 403, the server
    // Tus-Max-Size answers 413. Declare 32 KiB — inside the 1 MiB scope but
    // above a shrunken 16 KiB server cap — so the 413 comes from the tus layer.
    let config = TransferConfig {
        max_upload_bytes: 16 * 1024,
        ..test_config(tmp.path())
    };
    let (_server, handle, base) = spawn_server(tmp.path(), Some(config)).await;
    let (status, _) = create_upload(&client(), &base, TOKEN_A, 32 * 1024, None).await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "Upload-Length above Tus-Max-Size → 413"
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn exhausted_disk_budget_returns_507_with_required_and_available() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(tmp.path());
    let server = duthub_transfer::TransferServer::with_free_space_in(
        tmp.path(),
        config,
        Some(authorizer()),
        Arc::new(FixedFreeSpace(AtomicU64::new(0))),
    )
    .unwrap();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());

    let response = post_upload(&client(), &base, Some(TOKEN_A), 4096, &default_metadata()).await;
    assert_eq!(
        response.status(),
        StatusCode::INSUFFICIENT_STORAGE,
        "budget guard answers 507 directly"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"], "insufficient_storage");
    assert!(
        body["required"].as_u64().unwrap() > 0,
        "required must be present"
    );
    assert_eq!(body["available"], 0, "available must be present");
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn concurrent_patch_waits_for_the_lock_then_503_and_bytes_unchanged() {
    use bytes::Bytes;
    use futures_util::{StreamExt, stream};
    use std::convert::Infallible;

    let tmp = tempfile::tempdir().unwrap();
    let payload = payload(40_000, 233);
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(&http, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let url = format!("{base}/{}", id.unwrap());

    // A writer whose body stalls after its first chunk holds the upload lock
    // exactly like a slow network would.
    let head = Bytes::copy_from_slice(&payload[..10_000]);
    let body = reqwest::Body::wrap_stream(
        stream::once(async move { Ok::<_, Infallible>(head) }).chain(stream::pending()),
    );
    let stalled = http
        .patch(&url)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Upload-Offset", 0)
        .header("Content-Type", CHUNK_CONTENT_TYPE)
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .body(body)
        .send();
    let writer = tokio::spawn(stalled);

    // Wait until the stalled writer really owns the upload lock. HEAD takes
    // the same lock in read mode, which makes it a side-effect-free probe: it
    // can only time out (503) while a writer holds the lock.
    let started = Instant::now();
    while head_offset(&http, &url, Some(TOKEN_A)).await.0 != StatusCode::SERVICE_UNAVAILABLE {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the lock was never contended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 1 s lock wait (test_config): the contended PATCH fails after ~1 s with
    // 503 (the upstream LockTimeout mapping).
    let started = Instant::now();
    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, payload[..5_000].to_vec()).await;
    let elapsed = started.elapsed();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "blocked PATCH surfaces as LockTimeout"
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "waited for the lock: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "did not hang: {elapsed:?}"
    );
    // Cutting the stalled writer releases the lock.
    writer.abort();
    let _ = writer.await;

    // Nothing was committed by either PATCH.
    let (status, offset) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offset, Some(0));
    // A retry at the acknowledged offset overwrites the uncommitted tail.
    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, payload.clone()).await;
    assert_eq!(response.status().as_u16(), 204);
    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        payload
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn expired_upload_is_410_until_swept_then_404() {
    let tmp = tempfile::tempdir().unwrap();
    // Single-source TTL: 1 s, fed to the tus expiration extension and the sweep.
    let config = TransferConfig {
        upload_expires_secs: 1,
        ..test_config(tmp.path())
    };
    let (server, handle, base) = spawn_server(tmp.path(), Some(config)).await;
    let http = client();
    let payload = b"short-lived".to_vec();
    // Upload A pins the 410 → sweep → 404 flow; upload B (created in the same
    // window) pins the DELETE contract on an expired-but-unswept upload.
    let response = post_upload(
        &http,
        &base,
        Some(TOKEN_A),
        payload.len() as u64,
        &default_metadata(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    // The deadline is advertised when it is known at creation time.
    assert!(response.headers().get("upload-expires").is_some());
    let id = location_id(&response).unwrap();
    let url = format!("{base}/{id}");
    let (status, id_b) = create_upload(&http, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let url_b = format!("{base}/{}", id_b.unwrap());

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (status, _) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::GONE, "expired but not yet swept → Gone");

    let response = delete_upload(&http, &url_b, Some(TOKEN_A)).await;
    assert_eq!(
        response.status().as_u16(),
        204,
        "DELETE on expired upload is accepted"
    );
    let (status, _) = head_offset(&http, &url_b, Some(TOKEN_A)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "DELETE removed the expired staging"
    );

    assert_eq!(
        server.gc_now().await,
        1,
        "gc_now sweeps the expired staging; staging_exists={}",
        tmp.path().join("data").join(format!("{id}.bin")).exists()
    );
    let (status, _) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "swept → NotFound");
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn delete_is_logical_and_head_then_404() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = b"to be terminated".to_vec();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(&http, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let url = format!("{base}/{id}");

    let response = delete_upload(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(response.status().as_u16(), 204);
    let (status, _) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!tmp.path().join("data").join(format!("{id}.bin")).exists());
    // A PATCH on the deleted upload cannot resurrect it.
    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, payload).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn a_completed_upload_cannot_be_taken_back() {
    // A finished upload is a published artifact: the session that created it
    // may neither delete it nor append to it.
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let payload = b"published artifact".to_vec();
    let (status, id) = create_upload(&http, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let url = format!("{base}/{}", id.unwrap());
    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, payload.clone()).await;
    assert_eq!(response.status().as_u16(), 204);

    let response = delete_upload(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Appending past the declared length is refused and changes nothing.
    let response = patch_bytes(
        &http,
        &url,
        Some(TOKEN_A),
        payload.len() as u64,
        b"more".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        payload
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn cross_subject_writes_are_403() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = b"subject bound".to_vec();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(&http, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let url = format!("{base}/{}", id.unwrap());

    // agent-b cannot touch agent-a's upload (subject bound at creation).
    let response = patch_bytes(&http, &url, Some(TOKEN_B), 0, payload).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let (status, _) = head_offset(&http, &url, Some(TOKEN_B)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let response = delete_upload(&http, &url, Some(TOKEN_B)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // The owner still can.
    let (status, _) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn name_attacks_are_400_and_foreign_boards_403() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();

    let attack_cases: [(&str, &str, StatusCode); 4] = [
        ("../../etc/passwd", "board1", StatusCode::BAD_REQUEST),
        ("CON.img", "board1", StatusCode::BAD_REQUEST),
        ("evil.exe", "board1", StatusCode::BAD_REQUEST),
        // Valid name, board outside the scope.
        ("flash.img", "boardX", StatusCode::FORBIDDEN),
    ];
    for (publish_name, board, expected) in attack_cases {
        let metadata = metadata_header(&[("filename", publish_name), ("board", board)]);
        let response = post_upload(&http, &base, Some(TOKEN_A), 1024, &metadata).await;
        assert_eq!(response.status(), expected, "{publish_name:?}/{board:?}");
    }
    // A malformed board name is 400 even when the scope would be 403.
    let metadata = metadata_header(&[("filename", PUBLISH_NAME), ("board", "a/b")]);
    let response = post_upload(&http, &base, Some(TOKEN_A), 1024, &metadata).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn client_cannot_forge_the_bound_subject_or_a_digest() {
    // `subject` and `checksum` are server-owned metadata keys: whatever the
    // client sends is dropped before the upload is recorded.
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let forged = metadata_header(&[
        ("filename", PUBLISH_NAME),
        ("board", "board1"),
        ("subject", "agent-b"),
        ("checksum", &format!("blake3:{}", "a".repeat(64))),
    ]);
    let response = post_upload(&http, &base, Some(TOKEN_A), 11, &forged).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let url = format!("{base}/{}", location_id(&response).unwrap());

    let head = http
        .head(&url)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .send()
        .await
        .unwrap();
    let echoed = header(&head, "upload-metadata");
    assert!(
        echoed.contains(&metadata_header(&[("subject", "agent-a")])),
        "{echoed}"
    );
    assert!(
        !echoed.contains(&metadata_header(&[("subject", "agent-b")])),
        "{echoed}"
    );
    assert!(
        !echoed.contains("checksum"),
        "no header, no digest recorded: {echoed}"
    );

    // The forged subject does not hand agent-b any rights.
    let response = patch_bytes(&http, &url, Some(TOKEN_B), 0, b"0123456789a".to_vec()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // And the upload completes as a plain (unverified) transfer.
    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, b"0123456789a".to_vec()).await;
    assert_eq!(response.status().as_u16(), 204);
    assert!(response.headers().get("x-duthub-checksum").is_none());
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn second_upload_of_the_same_name_keeps_the_first() {
    // Upstream finalize never overwrites: the later upload lands next to the
    // first under an id-suffixed name.
    let tmp = tempfile::tempdir().unwrap();
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    for round in 0..2 {
        let payload = format!("artifact {round}").into_bytes();
        let response = post_upload(
            &http,
            &base,
            Some(TOKEN_A),
            payload.len() as u64,
            &default_metadata(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let id = location_id(&response).unwrap();
        let response = patch_bytes(
            &http,
            &format!("{base}/{id}"),
            Some(TOKEN_A),
            0,
            payload.clone(),
        )
        .await;
        assert_eq!(response.status().as_u16(), 204);
        let artifact =
            std::fs::canonicalize(tmp.path().join(header(&response, "x-duthub-artifact"))).unwrap();
        assert_eq!(std::fs::read(&artifact).unwrap(), payload);
    }
    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        b"artifact 0"
    );
    let files: Vec<String> = std::fs::read_dir(tmp.path().join("data"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(
        files.iter().any(|name| {
            name.starts_with("flash-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("img"))
        }),
        "{files:?}"
    );
    assert_eq!(server.store().ids().len(), 2);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn version_mismatch_is_412() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let response = client()
        .post(&base)
        .header("Tus-Resumable", "0.2.2")
        .header("Upload-Length", "10")
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn scope_ceiling_is_403_even_below_tus_max() {
    let tmp = tempfile::tempdir().unwrap();
    // Scope max 64 KiB, server max 2 MiB: the stricter one wins with 403.
    let config = test_config(tmp.path());
    let server = duthub_transfer::TransferServer::new_in(
        tmp.path(),
        config,
        Some(Arc::new(
            duthub_transfer::authorize::StaticTokenAuthorizer::new(HashMap::from([(
                TOKEN_A.to_string(),
                UploadScope {
                    subject: "agent-a".to_string(),
                    boards: vec!["board1".to_string()],
                    max_bytes: 64 * 1024,
                    expires_at_epoch: None,
                },
            )])),
        )),
    )
    .unwrap();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    let (status, _) = create_upload(&client(), &base, TOKEN_A, 128 * 1024, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "declared length above the scope ceiling → 403"
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn only_the_tus_upload_surface_is_served() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let bearer = format!("Bearer {TOKEN_A}");
    let resource = format!("{base}/00000000-0000-4000-8000-0000000000e1");
    // GET downloads are not part of the receiving endpoint.
    let response = http
        .get(&resource)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["error"], "method_not_allowed");
    let response = http
        .put(&resource)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn deferred_length_and_creation_with_upload_are_not_advertised() {
    // Both would let a request skip the declared-length checks the scope
    // ceiling and the disk budget are built on.
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let response = post_upload_with(
        &http,
        &base,
        Some(TOKEN_A),
        10,
        &default_metadata(),
        &[("Upload-Defer-Length", "1")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let response = http
        .post(&base)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Upload-Length", "10")
        .header("Content-Type", CHUNK_CONTENT_TYPE)
        .header("Upload-Metadata", default_metadata())
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .body("0123456789")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    handle.shutdown_and_join().await;
}
