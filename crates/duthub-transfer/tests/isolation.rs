//! Per-DUT isolation, quota, directory locking and corrupt-ledger behaviour.
//!
//! These pin the fixes from `docs/reviews/TRANSFER_SECURITY_REVIEW.md` (F-02, F-04,
//! F-05, F-08, F-14): one DUT per directory and endpoint, fail closed when a
//! record cannot be read, and one writer per transfer root.

mod common;

use common::*;
use duthub_transfer::TransferError;
use duthub_transfer::TransferServer;
use salvo::http::StatusCode;

fn config() -> TransferConfig {
    TransferConfig {
        transfer_dir: None,
        max_upload_bytes: 1 << 20,
        reserved_disk_bytes: 0,
        lock_timeout_secs: 1,
        ..TransferConfig::default()
    }
}

#[tokio::test]
async fn for_dut_writes_under_the_dut_directory_and_binds_uploads() {
    let root = tempfile::tempdir().unwrap();
    let dut = root.path().join("rk3576");
    let server = TransferServer::for_dut("dut-a", &dut, config(), Some(authorizer())).unwrap();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    let http = client();

    let payload = b"dut image".to_vec();
    let (status, id) = create_upload(&http, &base, TOKEN_A, payload.len() as u64, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let url = format!("{base}/{}", id.unwrap());
    let response = patch_bytes(&http, &url, Some(TOKEN_A), 0, payload.clone()).await;
    assert_eq!(response.status().as_u16(), 204);

    // The artifact lives under this DUT's own directory, not the work root and
    // not another DUT's tree.
    assert_eq!(
        std::fs::read(dut.join("images").join("data").join(PUBLISH_NAME)).unwrap(),
        payload
    );
    assert!(!root.path().join("data").join(PUBLISH_NAME).exists());
    assert!(dut.join(".duthub-dut-id").exists());

    // The binding is server-owned metadata: HEAD echoes the DUT id.
    let head = http
        .head(&url)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .send()
        .await
        .unwrap();
    let echoed = header(&head, "upload-metadata");
    assert!(
        echoed.contains(&metadata_header(&[("dutId", "dut-a")])),
        "{echoed}"
    );

    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn for_dut_refuses_a_directory_owned_by_another_dut() {
    let root = tempfile::tempdir().unwrap();
    let dut = root.path().join("shared");
    assert!(TransferServer::for_dut("dut-a", &dut, config(), Some(authorizer())).is_ok());
    let error = match TransferServer::for_dut("dut-b", &dut, config(), Some(authorizer())) {
        Ok(_) => panic!("a second DUT must not adopt an owned directory"),
        Err(error) => error,
    };
    assert!(matches!(error, TransferError::Config(_)), "{error:?}");
}

#[tokio::test]
async fn per_dut_quota_rejects_an_upload_beyond_the_ceiling() {
    let root = tempfile::tempdir().unwrap();
    let config = TransferConfig {
        dut_storage_bytes: Some(8),
        ..config()
    };
    let server = TransferServer::new_in(root.path(), config, Some(authorizer())).unwrap();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());

    let response = post_upload(&client(), &base, Some(TOKEN_A), 10, &default_metadata()).await;
    assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"], "insufficient_storage");
    assert_eq!(body["available"], 8);
    handle.shutdown_and_join().await;
}

/// Publish one file through the public API and return the final status.
async fn publish(http: &reqwest::Client, base: &str, name: &str, bytes: &[u8]) -> StatusCode {
    let metadata = metadata_header(&[("filename", name), ("board", "board1")]);
    let response = post_upload_with(
        http,
        base,
        Some(TOKEN_A),
        bytes.len() as u64,
        &metadata,
        &[],
    )
    .await;
    let status = response.status();
    if status != StatusCode::CREATED {
        return status;
    }
    let id = location_id(&response).expect("location");
    let url = format!("{base}/{id}");
    patch_bytes(http, &url, Some(TOKEN_A), 0, bytes.to_vec())
        .await
        .status()
}

/// Backdate a published artifact so "oldest first" is deterministic.
fn backdate(path: &std::path::Path, seconds: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(seconds))
        .unwrap();
}

#[tokio::test]
async fn quota_evicts_the_oldest_completed_artifact() {
    let root = tempfile::tempdir().unwrap();
    let config = TransferConfig {
        dut_storage_bytes: Some(16),
        ..config()
    };
    let server = TransferServer::new_in(root.path(), config, Some(authorizer())).unwrap();
    let transfer_dir = server.transfer_dir().to_path_buf();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    let http = client();

    assert_eq!(
        publish(&http, &base, "a.img", &[1u8; 8]).await,
        StatusCode::NO_CONTENT
    );
    backdate(&transfer_dir.join("data").join("a.img"), 7_200);
    assert_eq!(
        publish(&http, &base, "b.img", &[2u8; 8]).await,
        StatusCode::NO_CONTENT
    );
    backdate(&transfer_dir.join("data").join("b.img"), 3_600);

    // a + b fill the 16-byte ceiling; c evicts the oldest (a) and lands.
    assert_eq!(
        publish(&http, &base, "c.img", &[3u8; 8]).await,
        StatusCode::NO_CONTENT
    );
    assert!(
        !transfer_dir.join("data").join("a.img").exists(),
        "oldest evicted"
    );
    assert!(transfer_dir.join("data").join("b.img").exists());
    assert!(transfer_dir.join("data").join("c.img").exists());
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn quota_never_evicts_an_inflight_upload() {
    let root = tempfile::tempdir().unwrap();
    let config = TransferConfig {
        dut_storage_bytes: Some(16),
        ..config()
    };
    let server = TransferServer::new_in(root.path(), config, Some(authorizer())).unwrap();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    let http = client();

    // Two declared-but-unwritten uploads take the whole ceiling; there is
    // nothing completed to evict, so a third create is refused with 507.
    let (status, first) = create_upload(&http, &base, TOKEN_A, 8, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _second) = create_upload(&http, &base, TOKEN_A, 8, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _third) = create_upload(&http, &base, TOKEN_A, 8, None).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);

    // The in-flight upload survived and can still be resumed by its owner.
    let url = format!("{base}/{}", first.unwrap());
    let (status, offset) = head_offset(&http, &url, Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offset, Some(0));
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn a_corrupt_upload_record_is_rejected_instead_of_served() {
    let root = tempfile::tempdir().unwrap();
    let server = TransferServer::new_in(root.path(), config(), Some(authorizer())).unwrap();
    let transfer_dir = server.transfer_dir().to_path_buf();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    let http = client();
    let (status, id) = create_upload(&http, &base, TOKEN_A, 4, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();

    // Make the server-owned subject unreadable as text (valid JSON, invalid
    // UTF-8). The guard must fail closed, not treat it as an unknown upload.
    let sidecar = transfer_dir.join("metadata").join(format!("{id}.json"));
    let mut ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    ledger["metadata"]["subject"] = serde_json::json!([255u8]);
    std::fs::write(&sidecar, serde_json::to_vec(&ledger).unwrap()).unwrap();

    let (status, _) = head_offset(&http, &format!("{base}/{id}"), Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn one_writer_per_transfer_root() {
    let root = tempfile::tempdir().unwrap();
    let server = TransferServer::new_in(root.path(), config(), Some(authorizer())).unwrap();
    let first = server.serve(0).await.unwrap();
    let error = server.serve(0).await.unwrap_err();
    assert!(matches!(error, TransferError::Locked(_)), "{error:?}");
    first.shutdown_and_join().await;
    // Closing the listener releases the directory.
    let second = server.serve(0).await.unwrap();
    second.shutdown_and_join().await;
}

#[tokio::test]
async fn transfer_root_is_refused_outside_the_work_root() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let config = TransferConfig {
        transfer_dir: Some(outside.path().to_string_lossy().to_string()),
        ..config()
    };
    let error = match TransferServer::new_in(root.path(), config, Some(authorizer())) {
        Ok(_) => panic!("an absolute root outside the work directory must be refused"),
        Err(error) => error,
    };
    assert!(matches!(error, TransferError::Config(_)), "{error:?}");
}
