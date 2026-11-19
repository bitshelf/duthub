//! Non-canonical path spellings must never reach the tus handlers.
//!
//! Salvo collapses `//`, drops `.` and pops `..` before matching, so a request
//! written that way would be routed to the upload handlers while the guard
//! classified the raw path as outside the mount and skipped every check. These
//! cases therefore speak HTTP/1.1 on a raw socket: a normal HTTP client
//! normalizes the path on its side and cannot reproduce the attack.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::*;
use salvo::http::StatusCode;

/// Send one literal request line and read the whole response.
fn raw_http(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the receiver");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

fn status_of(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn post_request(path: &str, metadata: &str, token: Option<&str>) -> String {
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{authorization}Tus-Resumable: 1.0.0\r\n\
         Upload-Length: 4\r\nUpload-Metadata: {metadata}\r\nContent-Length: 0\r\n\
         Connection: close\r\n\r\n"
    )
}

// The raw socket reads below are blocking on purpose; a current-thread test
// runtime would serialize them against the server task and time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_canonical_mount_spellings_are_rejected_before_auth() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, handle, _base) = spawn_server(tmp.path(), None).await;
    let port = handle.endpoint.port();
    let metadata = metadata_header(&[("filename", PUBLISH_NAME)]);

    // Spellings the guard sees are answered with a named 400.
    for spelling in [
        "//transfer",
        "///transfer",
        "/./transfer",
        "/foo/../transfer",
    ] {
        // Both without a token and with a valid one: the spelling itself is
        // the problem, so a token must not change the answer either.
        for token in [None, Some(TOKEN_A)] {
            let response = raw_http(port, &post_request(spelling, &metadata, token));
            assert_eq!(
                status_of(&response),
                400,
                "{spelling} with token {token:?} was served:\n{response}"
            );
            assert!(
                response.contains("invalid_path"),
                "{spelling} must name the path problem:\n{response}"
            );
        }
    }

    // A percent-encoded slash never matches the mount, so the router answers
    // 404 without running the guard. Either way it must not create anything.
    for token in [None, Some(TOKEN_A)] {
        let response = raw_http(port, &post_request("/%2Ftransfer", &metadata, token));
        let status = status_of(&response);
        assert!(
            (400..500).contains(&status),
            "/%2Ftransfer with token {token:?} was served:\n{response}"
        );
    }

    // Nothing was created by any of the rejected spellings.
    assert!(
        server.store().ids().is_empty(),
        "no upload record may exist"
    );
    assert!(!published_path(tmp.path(), PUBLISH_NAME).exists());
    handle.shutdown_and_join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_canonical_spellings_cannot_touch_an_existing_upload() {
    let tmp = tempfile::tempdir().unwrap();
    let (_server, handle, base) = spawn_server(tmp.path(), None).await;
    let http = client();
    let (status, id) = create_upload(&http, &base, TOKEN_A, 4, None).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    let port = handle.endpoint.port();

    let cases = [
        (
            "PATCH",
            "Upload-Offset: 0\r\nContent-Type: application/offset+octet-stream\r\nContent-Length: 4\r\n",
            "PWN!",
        ),
        ("HEAD", "", ""),
        ("DELETE", "", ""),
    ];
    for (method, headers, body) in cases {
        let request = format!(
            "{method} //transfer/{id} HTTP/1.1\r\nHost: 127.0.0.1\r\nTus-Resumable: 1.0.0\r\n\
             {headers}Connection: close\r\n\r\n{body}"
        );
        let response = raw_http(port, &request);
        assert_eq!(
            status_of(&response),
            400,
            "{method} through a non-canonical path was served:\n{response}"
        );
    }

    // The owner's canonical view is untouched: no bytes, no artifact.
    let (status, offset) = head_offset(&http, &format!("{base}/{id}"), Some(TOKEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offset, Some(0));
    assert!(!published_path(tmp.path(), PUBLISH_NAME).exists());
    handle.shutdown_and_join().await;
}
