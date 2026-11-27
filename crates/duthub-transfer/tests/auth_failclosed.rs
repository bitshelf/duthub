//! Fail-closed authorization matrix, bind policy and config normalization.
//!
//! Two fixed HTTP semantics: 401 = not authenticated (token missing or
//! unknown), 403 = authenticated but the scope does not cover the request.

mod common;

use common::*;
use duthub_transfer::authorize::UploadScope;
use salvo::http::StatusCode;
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn without_authorizer_every_request_is_401_except_options() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(tmp.path());
    // No authorizer → DenyAll semantics.
    let server = duthub_transfer::TransferServer::new_in(tmp.path(), config, None).unwrap();
    let handle = server.serve(0).await.unwrap();
    let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
    let http = client();

    let options = http
        .request(reqwest::Method::OPTIONS, &base)
        .send()
        .await
        .unwrap();
    assert_eq!(options.status().as_u16(), 204, "OPTIONS stays anonymous");

    let resource = format!("{base}/00000000-0000-4000-8000-0000000000e1");
    let cases = [
        (
            http.post(&base)
                .header("Tus-Resumable", TUS_VERSION)
                .header("Upload-Length", "10"),
            "POST",
        ),
        (
            http.head(&resource).header("Tus-Resumable", TUS_VERSION),
            "HEAD",
        ),
        (
            http.patch(&resource)
                .header("Tus-Resumable", TUS_VERSION)
                .header("Upload-Offset", "0")
                .header("Content-Type", CHUNK_CONTENT_TYPE),
            "PATCH",
        ),
        (
            http.delete(&resource).header("Tus-Resumable", TUS_VERSION),
            "DELETE",
        ),
        (
            http.get(&resource).header("Tus-Resumable", TUS_VERSION),
            "GET",
        ),
    ];
    for (request, label) in cases {
        // Both without a token and with any token: 401 either way.
        let response = request.try_clone().unwrap().send().await.unwrap();
        assert_eq!(response.status().as_u16(), 401, "{label} without token");
        let response = request
            .header("Authorization", "Bearer whatever-token")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            401,
            "{label} with an unknown token"
        );
    }

    // POST with a valid metadata shape is still 401: auth runs before
    // everything, and the challenge is advertised.
    let response = post_upload(
        &http,
        &base,
        Some("whatever-token"),
        10,
        &default_metadata(),
    )
    .await;
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(header(&response, "www-authenticate"), "Bearer");
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"],
        "unauthorized"
    );
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn authenticated_but_out_of_scope_is_403() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();

    // Wrong board (valid name) → 403; oversized declaration → 403.
    let metadata = metadata_header(&[("filename", PUBLISH_NAME), ("board", "boardX")]);
    let response = post_upload(&http, &base, Some(TOKEN_A), 1024, &metadata).await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "board outside the scope → 403"
    );

    let (status, _) = create_upload(&http, &base, TOKEN_A, (1 << 20) + 1, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "declared length above the scope ceiling → 403"
    );

    // Expired scope → 403.
    let config = test_config(&tmp.path().join("expired"));
    let scopes = HashMap::from([(
        TOKEN_A.to_string(),
        UploadScope {
            subject: "agent-a".to_string(),
            boards: vec!["board1".to_string()],
            max_bytes: 1 << 20,
            expires_at_epoch: Some(1), // far in the past
        },
    )]);
    let server = duthub_transfer::TransferServer::new_in(
        tmp.path().join("expired"),
        config,
        Some(Arc::new(
            duthub_transfer::authorize::StaticTokenAuthorizer::new(scopes),
        )),
    )
    .unwrap();
    let handle2 = server.serve(0).await.unwrap();
    let base2 = format!("http://127.0.0.1:{}/transfer", handle2.endpoint.port());
    let (status, _) = create_upload(&http, &base2, TOKEN_A, 1024, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expired scope → 403");
    handle2.shutdown_and_join().await;
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn non_loopback_binding_without_authorizer_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let config = TransferConfig {
        lan_access: true,
        ..test_config(tmp.path())
    };
    let server = duthub_transfer::TransferServer::new_in(tmp.path(), config, None).unwrap();
    let error = server.serve(0).await.unwrap_err();
    assert!(
        format!("{error}").contains("bind policy"),
        "bind policy violation surfaced: {error}"
    );
}

#[tokio::test]
async fn lan_binding_with_authorizer_serves_on_wildcard() {
    let tmp = tempfile::tempdir().unwrap();
    let config = TransferConfig {
        lan_access: true,
        ..test_config(tmp.path())
    };
    let (_server, handle, _base) = spawn_server(tmp.path(), Some(config)).await;
    assert!(handle.endpoint.ip().is_unspecified());
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn base_path_writings_route_identically() {
    // "transfer", "/transfer", "/transfer/" must all normalize to the same
    // routing behavior.
    let tmp = tempfile::tempdir().unwrap();
    let mut results = Vec::new();
    for (index, spelling) in ["transfer", "/transfer", "/transfer/"]
        .into_iter()
        .enumerate()
    {
        let config = TransferConfig {
            base_path: spelling.to_string(),
            transfer_dir: Some(
                tmp.path()
                    .join(format!("run-{index}"))
                    .to_string_lossy()
                    .to_string(),
            ),
            ..test_config(tmp.path())
        };
        let (_server, handle, _base) = spawn_server(tmp.path(), Some(config)).await;
        let base = format!("http://127.0.0.1:{}/transfer", handle.endpoint.port());
        let http = client();
        let options = http
            .request(reqwest::Method::OPTIONS, &base)
            .send()
            .await
            .unwrap()
            .status();
        let expected = blake3_hex(b"normalized");
        let (created, id) = create_upload(
            &http,
            &base,
            TOKEN_A,
            11,
            Some(&format!("blake3 {expected}")),
        )
        .await;
        assert_eq!(
            created,
            StatusCode::CREATED,
            "spelling {spelling:?} must route POST"
        );
        let url = format!("{base}/{}", id.unwrap());
        let patched = patch_bytes(&http, &url, Some(TOKEN_A), 0, b"normalized".to_vec())
            .await
            .status();
        assert_eq!(patched.as_u16(), 204);
        results.push((options, created, patched));
        handle.shutdown_and_join().await;
    }
    assert!(
        results.windows(2).all(|pair| pair[0] == pair[1]),
        "identical routing: {results:?}"
    );
}

#[tokio::test]
async fn trailing_slash_post_creates_like_the_bare_mount() {
    // Salvo routes `/transfer/` to the same creation handler as `/transfer`.
    // The three configured spellings already normalize to one mount, and a
    // client that adds a trailing slash on the wire is served the same way.
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let response = post_upload(
        &client(),
        &format!("{base}/"),
        Some(TOKEN_A),
        10,
        &default_metadata(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(header(&response, "location").starts_with("/transfer/"));
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn paths_outside_the_mount_are_not_ours() {
    // The guard does not claim foreign paths: without a token they are the
    // router's 404, not a 401 that would advertise the mount.
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let other = base.replace("/transfer", "/other");
    let response = client().post(&other).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // A shared prefix is not the mount either.
    let sibling = format!("{base}s");
    let response = client().post(&sibling).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    handle.shutdown_and_join().await;
}

#[tokio::test]
async fn transfer_dir_rejections_fail_at_construction() {
    let tmp = tempfile::tempdir().unwrap();
    for bad in [
        "../outside",
        "D:images",
        "\\images",
        "\\\\srv\\share\\images",
        "data/../..",
    ] {
        let config = TransferConfig {
            transfer_dir: Some(bad.to_string()),
            ..test_config(tmp.path())
        };
        let Err(error) = duthub_transfer::TransferServer::new_in(tmp.path(), config, None) else {
            panic!("expected a config error for {bad:?}");
        };
        assert!(
            matches!(error, duthub_transfer::TransferError::Config(_)),
            "{bad:?}"
        );
    }
}
