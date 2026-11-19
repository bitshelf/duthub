//! Proxy-mode behavior test over loopback only (no external network).
//!
//! Proves the two things that cannot be seen from a built client:
//! - `ProxyConfig::Off` really ignores `HTTP_PROXY` (request reaches a local origin);
//! - `ProxyConfig::Env` really honors it (request is routed into a dead proxy and fails).

use duthub_tooldown::ProxyConfig;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// Minimal HTTP/1.1 origin server: answers every request with 200 "ok".
async fn spawn_origin() -> std::io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                // Drain request headers, then answer.
                let mut buf = [0u8; 4096];
                loop {
                    match conn.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") => break,
                        Ok(_) => {}
                    }
                }
                let _ = conn
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = conn.flush().await;
            });
        }
    });
    Ok(addr)
}

#[tokio::test]
async fn off_mode_ignores_proxy_env_and_env_mode_honors_it() {
    let origin = spawn_origin().await.expect("bind loopback origin");
    let url = format!("http://{origin}/");

    // Dead proxy endpoint: port 1 on loopback refuses connections.
    let dead_proxy = "http://127.0.0.1:1";

    // SAFETY: single-threaded current-thread runtime, and no other test in this crate
    // reads proxy env vars; the DropGuard restores the environment even on panic.
    unsafe { std::env::set_var("HTTP_PROXY", dead_proxy) };
    let _proxy_guard = std::mem::DropGuard::new((), |()| {
        // SAFETY: same single-threaded test invariant as `set_var` above.
        unsafe { std::env::remove_var("HTTP_PROXY") };
    });

    // Env mode: reqwest must route via the (dead) proxy -> transport error.
    let env_client = ProxyConfig::Env.build_client().expect("env client");
    let via_env = env_client.get(&url).send().await;
    assert!(
        via_env.is_err(),
        "Env mode must honor HTTP_PROXY; got a direct-response success: {via_env:?}"
    );

    // Off mode: env ignored, request reaches the loopback origin.
    let off_client = ProxyConfig::Off.build_client().expect("off client");
    let response = off_client
        .get(&url)
        .send()
        .await
        .expect("Off mode must ignore HTTP_PROXY and reach the origin");
    assert!(response.status().is_success());
    let body = response.text().await.expect("body");
    assert_eq!(body, "ok");
}
