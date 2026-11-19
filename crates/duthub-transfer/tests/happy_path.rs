//! Happy path: OPTIONS → POST → HEAD → PATCH chunks → finalized artifact.
//!
//! Two flows share the same surface: a plain LAN transfer (no digest declared,
//! nothing hashed) and a verified one (`Upload-Checksum` on creation).

mod common;

use common::*;
use salvo::http::StatusCode;

const STATUS_NO_CONTENT: u16 = 204;

#[tokio::test]
async fn options_is_anonymous_and_advertises_the_upload_surface() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;

    let options = client()
        .request(reqwest::Method::OPTIONS, &base)
        .send()
        .await
        .unwrap();
    assert_eq!(options.status().as_u16(), STATUS_NO_CONTENT);
    let headers = options.headers().clone();
    assert_eq!(headers.get("tus-version").unwrap(), TUS_VERSION);
    let extensions = headers.get("tus-extension").unwrap().to_str().unwrap();
    for expected in ["creation", "termination", "expiration"] {
        assert!(extensions.contains(expected), "{extensions}");
    }
    for absent in [
        "creation-with-upload",
        "creation-defer-length",
        "checksum",
        "concatenation",
    ] {
        assert!(!extensions.contains(absent), "{extensions}");
    }
    assert_eq!(headers.get("tus-max-size").unwrap(), "2097152");

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn full_upload_publishes_a_named_image_without_hashing() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let client = client();

    // No `Upload-Checksum`: the client asked for a plain transfer.
    let payload = payload(300_000, 251);
    let (status, id) = create_upload(&client, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED, "POST must create the upload");
    let id = id.expect("Location header with the artifact id");
    let url = format!("{base}/{id}");

    // HEAD right after creation reports offset 0 and echoes the metadata.
    let (status, offset) = head_offset(&client, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offset, Some(0));
    let head = client
        .head(&url)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .send()
        .await
        .unwrap();
    let echoed = head
        .headers()
        .get("upload-metadata")
        .unwrap()
        .to_str()
        .unwrap();
    // Metadata is a HashMap — key order is arbitrary; only presence matters.
    assert!(echoed.contains("filename "), "metadata echoed: {echoed}");
    assert!(echoed.contains("board "), "metadata echoed: {echoed}");
    assert!(echoed.contains("subject "), "subject is bound: {echoed}");
    assert_eq!(head.headers().get("upload-length").unwrap(), "300000");
    assert_eq!(head.headers().get("cache-control").unwrap(), "no-store");

    // PATCH the first half, then the rest.
    let (first, second) = payload.split_at(150_000);
    let response = patch_bytes(&client, &url, Some(TOKEN_A), 0, first.to_vec()).await;
    assert_eq!(response.status().as_u16(), STATUS_NO_CONTENT);
    assert_eq!(response.headers().get("upload-offset").unwrap(), "150000");
    // Nothing is verified yet, so no digest header on an intermediate chunk.
    assert!(response.headers().get("x-duthub-checksum").is_none());
    let (_, offset) = head_offset(&client, &url, Some(TOKEN_A)).await;
    assert_eq!(offset, Some(150_000), "HEAD reports the committed offset");

    let response = patch_bytes(&client, &url, Some(TOKEN_A), 150_000, second.to_vec()).await;
    assert_eq!(response.status().as_u16(), STATUS_NO_CONTENT);

    // The upstream disk store finalized the upload to its upload name.
    let published = published_path(tmp.path(), PUBLISH_NAME);
    assert_eq!(std::fs::read(&published).unwrap(), payload);
    // The receipt names the artifact relative to the transfer root; the host's
    // absolute path is never exposed.
    let artifact = header(&response, "x-duthub-artifact");
    assert!(!artifact.contains(':'), "no drive letter: {artifact}");
    assert_eq!(
        std::fs::canonicalize(tmp.path().join(&artifact)).unwrap(),
        std::fs::canonicalize(&published).unwrap()
    );
    // A plain transfer is never hashed and leaves no verification marker.
    assert!(response.headers().get("x-duthub-checksum").is_none());
    assert!(
        !tmp.path()
            .join("metadata")
            .join(format!("{id}.verified"))
            .exists()
    );

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn full_upload_with_blake3_checksum_verifies_and_publishes() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let client = client();

    let payload = payload(300_000, 251);
    let expected = blake3_hex(&payload);
    let (status, id) = create_upload(
        &client,
        &base,
        TOKEN_A,
        payload.len() as u64,
        Some(&format!("blake3 {expected}")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let url = format!("{base}/{id}");

    let (first, second) = payload.split_at(150_000);
    let response = patch_bytes(&client, &url, Some(TOKEN_A), 0, first.to_vec()).await;
    assert_eq!(response.status().as_u16(), STATUS_NO_CONTENT);
    assert!(response.headers().get("x-duthub-checksum").is_none());

    // The completing chunk is verified before it is acknowledged.
    let response = patch_bytes(&client, &url, Some(TOKEN_A), 150_000, second.to_vec()).await;
    assert_eq!(response.status().as_u16(), STATUS_NO_CONTENT);
    assert_eq!(
        header(&response, "x-duthub-checksum"),
        format!("blake3:{expected}")
    );

    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        payload
    );
    // The verified digest is durably recorded next to the upload.
    let marker = server.store().verified_marker(&id).unwrap();
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("blake3:{expected}")
    );

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn full_upload_with_sha256_checksum_verifies_and_publishes() {
    // `sha256` is what the ztus CLI declares; uppercase hex is accepted too.
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let client = client();

    let payload = payload(90_000, 241);
    let expected = sha256_hex(&payload);
    let (status, id) = create_upload(
        &client,
        &base,
        TOKEN_A,
        payload.len() as u64,
        Some(&format!("sha256 {}", expected.to_uppercase())),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let url = format!("{base}/{}", id.unwrap());

    let response = patch_bytes(&client, &url, Some(TOKEN_A), 0, payload.clone()).await;
    assert_eq!(response.status().as_u16(), STATUS_NO_CONTENT);
    assert_eq!(
        header(&response, "x-duthub-checksum"),
        format!("sha256:{expected}")
    );
    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        payload
    );

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn retried_final_chunk_reports_the_recorded_verification() {
    // A client that lost the completing response retries the last chunk. The
    // tus layer answers the empty PATCH without re-running anything, so the
    // recorded digest is what tells the client the upload really verified.
    let tmp = tempfile::tempdir().unwrap();
    let (server, handle, base) = spawn_server(tmp.path(), None).await;
    let client = client();

    let payload = payload(40_000, 239);
    let expected = blake3_hex(&payload);
    let (status, id) = create_upload(
        &client,
        &base,
        TOKEN_A,
        payload.len() as u64,
        Some(&format!("blake3 {expected}")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let url = format!("{base}/{id}");
    patch_bytes(&client, &url, Some(TOKEN_A), 0, payload.clone()).await;

    let retry = patch_bytes(
        &client,
        &url,
        Some(TOKEN_A),
        payload.len() as u64,
        Vec::new(),
    )
    .await;
    assert_eq!(retry.status().as_u16(), STATUS_NO_CONTENT);
    assert_eq!(
        header(&retry, "x-duthub-checksum"),
        format!("blake3:{expected}")
    );

    // Losing the marker (a crash between commit and verification) makes the
    // next touch re-verify instead of trusting the file.
    let marker = server.store().verified_marker(&id).unwrap();
    std::fs::remove_file(&marker).unwrap();
    let retry = patch_bytes(
        &client,
        &url,
        Some(TOKEN_A),
        payload.len() as u64,
        Vec::new(),
    )
    .await;
    assert_eq!(retry.status().as_u16(), STATUS_NO_CONTENT);
    assert_eq!(
        header(&retry, "x-duthub-checksum"),
        format!("blake3:{expected}")
    );
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("blake3:{expected}")
    );

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn zero_length_upload_completes_when_explicitly_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    // Zero-length artifacts are refused by default; a host must opt in.
    let config = TransferConfig {
        allow_zero_length_uploads: true,
        ..test_config(tmp.path())
    };
    let (_server, handle, base) = spawn_server(tmp.path(), Some(config)).await;
    let client = client();

    let expected = blake3_hex(b"");
    let response = post_upload_with(
        &client,
        &base,
        Some(TOKEN_A),
        0,
        &default_metadata(),
        &[("Upload-Checksum", &format!("blake3 {expected}"))],
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        header(&response, "x-duthub-checksum"),
        format!("blake3:{expected}")
    );
    assert_eq!(
        std::fs::read(published_path(tmp.path(), PUBLISH_NAME)).unwrap(),
        b""
    );

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn zero_length_upload_is_refused_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let response =
        post_upload_with(&client(), &base, Some(TOKEN_A), 0, &default_metadata(), &[]).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"],
        "invalid_metadata"
    );
    assert!(!published_path(tmp.path(), PUBLISH_NAME).exists());
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn options_works_without_any_token() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let response = client()
        .request(reqwest::Method::OPTIONS, &base)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), STATUS_NO_CONTENT);
    handle.shutdown_and_join().await;
}
