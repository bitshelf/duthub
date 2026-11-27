//! Simulated server image transfer test: resumable upload + SHA-256 verification
//! + download readback + strict containment inside duthub's directory.

mod common;

use common::*;
use duthub_transfer::{TransferServer, config::TransferConfig};
use salvo::http::StatusCode;

#[tokio::test]
async fn simulated_server_upload_download_and_strict_path_containment() {
    let sandbox = tempfile::tempdir().unwrap();
    let duthub_root = sandbox.path().join("duthub-home");
    let outside_dir = sandbox.path().join("outside-system-dir");
    std::fs::create_dir_all(&duthub_root).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();

    // 1) Trying to point transfer_dir outside duthub's root must fail closed.
    let escape_cfg = TransferConfig {
        transfer_dir: Some(outside_dir.to_string_lossy().to_string()),
        ..TransferConfig::default()
    };
    assert!(
        TransferServer::new_in(&duthub_root, escape_cfg, Some(authorizer())).is_err(),
        "transfer server must refuse paths outside duthub root"
    );
    let rel_escape_cfg = TransferConfig {
        transfer_dir: Some("../outside-system-dir".into()),
        ..TransferConfig::default()
    };
    assert!(
        TransferServer::new_in(&duthub_root, rel_escape_cfg, Some(authorizer())).is_err(),
        "transfer server must refuse relative traversal outside duthub root"
    );

    // 2) Spin up a per-DUT transfer server strictly inside `<duthub_root>/RK3576-Board/images`.
    let dut_dir = duthub_root.join("RK3576-Board");
    std::fs::create_dir_all(&dut_dir).unwrap();
    let cfg = TransferConfig {
        max_upload_bytes: 8 * (1 << 20),
        reserved_disk_bytes: 0,
        lock_timeout_secs: 1,
        ..TransferConfig::default()
    };
    let server = TransferServer::for_dut("dut-rk3576", &dut_dir, cfg, Some(authorizer()))
        .expect("for_dut inside duthub_root");
    let handle = server.serve(0).await.expect("spawn transfer listener");
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    let http = client();

    // 3) Hostile publish filenames (traversal, absolute path, ADS, reserved names)
    //    must be rejected before creating any upload.
    for hostile in [
        "../outside-system-dir/evil.img",
        "..\\..\\Windows\\evil.img",
        "C:\\Windows\\evil.img",
        "boot.img:evil.exe",
        "CON.img",
        "NUL",
    ] {
        let meta = metadata_header(&[("filename", hostile), ("board", "board1")]);
        let resp = http
            .post(&base)
            .header("Tus-Resumable", TUS_VERSION)
            .header("Authorization", format!("Bearer {TOKEN_A}"))
            .header("Upload-Length", "1024")
            .header("Upload-Metadata", meta)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "hostile filename must be rejected: {hostile}"
        );
    }

    // 4) Simulate multi-chunk resumable firmware image upload (512 KiB) with SHA-256 checksum.
    let firmware = payload(512 * 1024, 131);
    let expected_sha = sha256_hex(&firmware);
    let meta = metadata_header(&[
        ("filename", "myir-image-lr3576-buildroot.img"),
        ("board", "board1"),
        ("imageType", "rockchip-update"),
    ]);
    let created = http
        .post(&base)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .header("Upload-Length", firmware.len().to_string())
        .header("Upload-Metadata", meta)
        .header("Upload-Checksum", format!("sha256 {expected_sha}"))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let upload_id = created
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let upload_url = format!("{base}/{upload_id}");

    // Upload in 4 resumable chunks (128 KiB each), checking HEAD offset between chunks.
    let mut offset = 0usize;
    for chunk in firmware.chunks(128 * 1024) {
        let (head_status, head_off) = head_offset(&http, &upload_url, Some(TOKEN_A)).await;
        assert_eq!(head_status, StatusCode::OK);
        assert_eq!(head_off, Some(offset as u64));

        let patch = patch_bytes(
            &http,
            &upload_url,
            Some(TOKEN_A),
            offset as u64,
            chunk.to_vec(),
        )
        .await;
        assert_eq!(patch.status(), StatusCode::NO_CONTENT);
        offset += chunk.len();
        if offset == firmware.len() {
            assert_eq!(
                header(&patch, "x-duthub-checksum"),
                format!("sha256:{expected_sha}")
            );
        }
    }

    // 5) Verify the image landed ONLY inside `<duthub_root>/RK3576-Board/images/data/`
    //    and download/read it back verifying byte-for-byte SHA-256 identity.
    let stored = dut_dir
        .join("images")
        .join("data")
        .join("myir-image-lr3576-buildroot.img");
    let downloaded = std::fs::read(&stored).expect("uploaded image must exist in DUT images dir");
    assert_eq!(downloaded.len(), firmware.len());
    assert_eq!(sha256_hex(&downloaded), expected_sha);

    // 6) Verify zero files were written to `outside_dir`.
    assert!(
        std::fs::read_dir(&outside_dir).unwrap().next().is_none(),
        "outside directory must remain completely empty"
    );

    handle.shutdown_and_join().await;
}
