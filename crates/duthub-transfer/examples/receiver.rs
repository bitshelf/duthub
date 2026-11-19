//! Standalone receiver for manual acceptance with real clients (`ztus`, `curl`).
//!
//! Serves one transfer endpoint with a single static token until interrupted:
//!
//! ```text
//! cargo run -p duthub-transfer --example receiver -- --port 18080 \
//!   --token my-token --dir /tmp/images --lan
//!
//! ztus upload boot.img http://127.0.0.1:18080/transfer \
//!   --header "Authorization:Bearer my-token" --metadata board:rk3576
//! curl -si -X OPTIONS http://127.0.0.1:18080/transfer
//! ```
//!
//! A static token is for local trials only; products plug in a keyring-backed
//! `UploadAuthorizer` instead.

use std::collections::HashMap;
use std::sync::Arc;

use duthub_transfer::TransferServer;
use duthub_transfer::authorize::{StaticTokenAuthorizer, UploadScope};
use duthub_transfer::config::TransferConfig;

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
        if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = arg("--token").unwrap_or_else(|| "dev-token".to_owned());
    let port: u16 = arg("--port").map_or(Ok(18080), |value| value.parse())?;
    // `arg` is value-oriented; a bare `--lan` flag would otherwise read as
    // absent (or as whatever argument follows it).
    let lan = std::env::args().any(|arg| arg == "--lan");
    // `dev-token` is public; binding a LAN listener with it would publish an
    // unauthenticated upload surface to the whole network.
    if lan && (token == "dev-token" || token.trim().is_empty()) {
        return Err(
            "--lan requires an explicit --token (the built-in dev-token is not allowed)".into(),
        );
    }
    let boards: Vec<String> = arg("--boards")
        .unwrap_or_else(|| "board1,rk3576".to_owned())
        .split(',')
        .filter(|board| !board.is_empty())
        .map(str::to_owned)
        .collect();

    // `--dir` is this standalone receiver's work root; without it the root is
    // the executable directory. The transfer root stays inside the work root.
    let dir = arg("--dir");
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let work_root = dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or(exe_dir);
    let server = TransferServer::new_in(
        work_root,
        TransferConfig {
            transfer_dir: dir.as_ref().map(|_| ".".to_owned()),
            lan_access: lan,
            ..TransferConfig::default()
        },
        Some(Arc::new(StaticTokenAuthorizer::new(HashMap::from([(
            token.clone(),
            UploadScope {
                subject: "manual-trial".to_owned(),
                boards,
                max_bytes: u64::MAX,
                expires_at_epoch: None,
            },
        )])))),
    )?;

    let handle = server.serve(port).await?;
    println!(
        "serving http://{}/transfer  root={}  token={token}",
        handle.endpoint,
        server.transfer_dir().display()
    );
    // `serve` owns the periodic sweep; block until the process is interrupted.
    std::future::pending::<()>().await;
    handle.shutdown_and_join().await;
    Ok(())
}
