//! duthub-transfer — receiving endpoint for board images over tus 1.0.0.
//!
//! Everything below the HTTP surface comes from upstream crates: `salvo` serves
//! the connection (HTTP/1.1 and prior-knowledge HTTP/2, which is what the
//! `ztus` CLI speaks) and its official `tus` module owns the protocol, the disk
//! storage, the per-upload locks and the finalize-on-complete rename. This crate
//! only adds what a duthub endpoint needs on top:
//!
//! - [`authorize`]: fail-closed Bearer scopes (401 / 403);
//! - [`router`]: the Salvo hoop enforcing them, plus publish-name validation,
//!   the disk budget and client-requested verification;
//! - [`checksum`]: whole-file digests (BLAKE3, or SHA-256 for `ztus`);
//! - [`store`]: the unfinished-upload TTL on top of the upstream disk store.
//!
//! # Clients
//!
//! Any tus 1.0.0 client works. Two are explicitly supported:
//!
//! ```text
//! # ztus: resumable, adaptive chunks, h2c. Verification is opt-in per run.
//! ztus upload boot.img http://host:port/transfer \
//!   --header "Authorization:Bearer $TOKEN" --metadata board:rk3576
//! ztus upload boot.img http://host:port/transfer \
//!   --header "Authorization:Bearer $TOKEN" --checksum sha256
//!
//! # curl: the digest is declared once, on the creation request.
//! curl -si -X POST http://host:port/transfer \
//!   -H "Authorization:Bearer $TOKEN" -H "Tus-Resumable: 1.0.0" \
//!   -H "Upload-Length: $(stat -c %s boot.img)" \
//!   -H "Upload-Metadata: filename $(printf boot.img | base64 -w0)" \
//!   -H "Upload-Checksum: blake3 $(b3sum boot.img | cut -d' ' -f1)"
//! curl -si -X PATCH http://host:port/transfer/<id> \
//!   -H "Authorization:Bearer $TOKEN" -H "Tus-Resumable: 1.0.0" \
//!   -H "Upload-Offset: 0" -H "Content-Type: application/offset+octet-stream" \
//!   --data-binary @boot.img
//! ```
//!
//! # Verification is the client's call
//!
//! The receiver hashes nothing unless the creation request carries
//! `Upload-Checksum: <algorithm> <hex>`: a plain LAN transfer pays no
//! verification cost. When the header is present the whole file is hashed once,
//! after its last byte is committed, and the completing response carries
//! `X-Duthub-Checksum`. A mismatch discards the upload and answers 460.
//!
//! # Status codes
//!
//! `401` missing or unknown token · `403` scope does not cover the request ·
//! `400` invalid metadata or checksum declaration · `404` unknown or swept ·
//! `409` offset mismatch · `410` expired, not yet swept · `412` unsupported tus
//! version · `413` above `Tus-Max-Size` · `460` digest mismatch · `503` upload
//! lock wait timed out · `507` disk budget exhausted.
//!
//! # Layout (owned by the tus `DiskStore`)
//!
//! ```text
//! <transfer_dir>/
//!   data/<filename>            # completed upload, finalized to its upload name
//!   data/<id>.bin              # upload still in flight
//!   metadata/<id>.json         # size, offset and metadata of one upload
//!   metadata/<id>.verified     # actual digest, once verification passed
//! ```
//!
//! An acknowledged offset survives a process crash. The upstream store does
//! **not** `fsync` per chunk (only `flush`), so there is **no power-loss
//! durability guarantee**: a declared digest proves the bytes as the process
//! read them (through the page cache), not that they reached the platter.
//! Hosts that need more must add their own sync or rely on the filesystem.
//!
//! # Stage-two wiring obligations (host side, out of scope here)
//!
//! - the transfer listener port must join the global uniqueness/occupancy
//!   probe (AGENTS.md pit #45) and align with `PortRegistry`
//!   (crates/duthub-a2a/src/ports.rs:41 — reserve/release/is_reserved/owner_of);
//! - persisting `TransferConfig` into `SettingsFile` (serde shape already
//!   follows the section conventions so `docs/examples/settings.json`
//!   roundtrips stay untouched);
//! - `keyring`-backed tokens implement [`authorize::UploadAuthorizer`].

#![forbid(unsafe_code)]

pub mod authorize;
pub mod checksum;
pub mod config;
pub mod names;
pub mod router;
pub mod store;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use salvo::Router;
use salvo::conn::{Listener, TcpListener};
use salvo::server::{Server, ServerHandle};
use salvo::tus::{DataStore, MemoryLocker, Tus, options::MaxSize};
use tokio::task::JoinHandle;

use crate::authorize::UploadAuthorizer;
use crate::config::{TransferConfig, TransferConfigError};
use crate::router::{TransferGuard, TransferState};
use crate::store::{ExpiringDiskStore, FreeSpace, Fs4FreeSpace};

/// How long shutdown lets in-flight requests drain before connections are cut.
/// An interrupted upload is not lost: the client resumes from the acknowledged
/// offset.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Server assembly failures.
#[derive(Debug)]
pub enum TransferError {
    /// Filesystem or socket problem while setting up the listener.
    Io(std::io::Error),
    /// Configuration could not be resolved.
    Config(TransferConfigError),
    /// Bind policy violation: non-loopback listener without an authorizer.
    BindPolicy(String),
    /// The transfer directory is already served by another instance/process.
    Locked(String),
    /// The executable directory could not be located for path anchoring.
    ExeDir,
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "transfer io error: {error}"),
            Self::Config(error) => write!(f, "transfer config error: {error}"),
            Self::BindPolicy(msg) => write!(f, "bind policy violation: {msg}"),
            Self::Locked(msg) => write!(f, "transfer directory locked: {msg}"),
            Self::ExeDir => write!(f, "cannot locate the executable directory"),
        }
    }
}

impl std::error::Error for TransferError {}

impl TransferError {
    /// Keep the underlying `io::Error` when Salvo has one, so a refused bind
    /// still reports the OS reason verbatim.
    fn from_salvo(error: salvo::Error) -> Self {
        match error {
            salvo::Error::Io(error) => Self::Io(error),
            other => Self::Io(std::io::Error::other(other)),
        }
    }
}

/// Assembled transfer surface: config + storage + the tus router.
///
/// All state is behind `Arc` or internally shared, so the host may keep one
/// instance and serve it from a task.
pub struct TransferServer {
    config: TransferConfig,
    transfer_dir: PathBuf,
    base: String,
    store: Arc<ExpiringDiskStore>,
    /// Shared with the tus handlers: one lock map for writers and the sweep.
    locker: MemoryLocker,
    free_space: Arc<dyn FreeSpace>,
    authorizer: Option<Arc<dyn UploadAuthorizer>>,
    /// DUT served by this instance; `None` for a standalone root.
    dut_id: Option<String>,
}

impl TransferServer {
    /// Build a standalone server anchored at the executable directory, with the
    /// production free-space probe (`fs4`). Products should prefer
    /// [`TransferServer::new_in`] with the resolved work directory.
    pub fn new(
        config: TransferConfig,
        authorizer: Option<Arc<dyn UploadAuthorizer>>,
    ) -> Result<Self, TransferError> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .ok_or(TransferError::ExeDir)?;
        Self::new_in(exe_dir, config, authorizer)
    }

    /// Build a server whose transfer root must stay inside `work_root`: the
    /// effective work directory the host resolved (`--workdir` / settings /
    /// exe directory). Every file this receiver writes lands below it.
    pub fn new_in(
        work_root: impl Into<PathBuf>,
        config: TransferConfig,
        authorizer: Option<Arc<dyn UploadAuthorizer>>,
    ) -> Result<Self, TransferError> {
        Self::with_free_space_in(work_root, config, authorizer, Arc::new(Fs4FreeSpace))
    }

    /// [`TransferServer::new_in`] with an injected free-space probe (tests fake
    /// it to make the 507 budget path deterministic).
    pub fn with_free_space_in(
        work_root: impl Into<PathBuf>,
        config: TransferConfig,
        authorizer: Option<Arc<dyn UploadAuthorizer>>,
        free_space: Arc<dyn FreeSpace>,
    ) -> Result<Self, TransferError> {
        let work_root = work_root.into();
        let base = config::normalize_base_path(&config.base_path).map_err(TransferError::Config)?;
        let transfer_dir = config::resolve_transfer_dir(config.transfer_dir.as_deref(), &work_root)
            .map_err(TransferError::Config)?;
        config::ensure_transfer_dir_writable(&transfer_dir, &work_root)
            .map_err(TransferError::Config)?;
        Self::assemble(config, base, transfer_dir, free_space, authorizer, None)
    }

    /// Build the receiver for one DUT. `dut_dir` is the directory the host
    /// already resolved for that DUT (ownership `.duthub-dut-id`); the transfer
    /// root becomes `<dut_dir>/images`, every upload is bound to `dut_id`, and
    /// the authorizer must be the DUT's own (its tokens are the endpoint
    /// boundary). Creating or mismatching the ownership marker fails closed, so
    /// two DUTs can never share a directory.
    pub fn for_dut(
        dut_id: impl Into<String>,
        dut_dir: impl Into<PathBuf>,
        config: TransferConfig,
        authorizer: Option<Arc<dyn UploadAuthorizer>>,
    ) -> Result<Self, TransferError> {
        let dut_id = dut_id.into();
        let dut_dir = dut_dir.into();
        config::ensure_transfer_dir_writable(&dut_dir, &dut_dir).map_err(TransferError::Config)?;
        let marker = dut_dir.join(".duthub-dut-id");
        match std::fs::read(&marker) {
            Ok(bytes) if bytes == dut_id.as_bytes() => {}
            Ok(_) => {
                return Err(TransferError::Config(
                    config::TransferConfigError::TransferDir(format!(
                        "{} belongs to another DUT",
                        dut_dir.display()
                    )),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&marker, dut_id.as_bytes()).map_err(TransferError::Io)?;
            }
            Err(error) => return Err(TransferError::Io(error)),
        }
        let base = config::normalize_base_path(&config.base_path).map_err(TransferError::Config)?;
        let transfer_dir = dut_dir.join("images");
        config::ensure_transfer_dir_writable(&transfer_dir, &dut_dir)
            .map_err(TransferError::Config)?;
        Self::assemble(
            config,
            base,
            transfer_dir,
            Arc::new(Fs4FreeSpace),
            authorizer,
            Some(dut_id),
        )
    }

    fn assemble(
        config: TransferConfig,
        base: String,
        transfer_dir: PathBuf,
        free_space: Arc<dyn FreeSpace>,
        authorizer: Option<Arc<dyn UploadAuthorizer>>,
        dut_id: Option<String>,
    ) -> Result<Self, TransferError> {
        let locker = MemoryLocker::new();
        let store = Arc::new(ExpiringDiskStore::new(
            transfer_dir.clone(),
            config.upload_ttl(),
            config.lock_timeout(),
            Arc::new(locker.clone()),
        ));
        Ok(Self {
            config,
            transfer_dir,
            base,
            store,
            locker,
            free_space,
            authorizer,
            dut_id,
        })
    }

    /// Router for hosts that embed the surface into their own Salvo service.
    /// The periodic sweep only runs inside [`TransferServer::serve`]; hosts
    /// that embed the router drive [`TransferServer::gc_now`] themselves.
    pub fn transfer_router(&self) -> Router {
        let mut tus = Tus::new()
            .path(self.base.clone())
            .max_size(MaxSize::Fixed(self.config.max_upload_bytes))
            .store(ExpiringDiskStore::clone(&self.store))
            .locker(self.locker.clone());
        let options = tus.options_mut();
        options.lock_drain_timeout =
            Some(u64::try_from(self.config.lock_timeout().as_millis()).unwrap_or(u64::MAX));
        // A completed upload is a published artifact. The upload session may
        // not take it back.
        options.disable_termination_for_finished_uploads = true;

        let state = Arc::new(TransferState {
            authorizer: self.authorizer.clone(),
            store: Arc::clone(&self.store),
            free_space: Arc::clone(&self.free_space),
            config: self.config.clone(),
            base: self.base.clone(),
            dut_id: self.dut_id.clone(),
            create_gate: tokio::sync::Mutex::new(()),
            verify_gate: Arc::new(tokio::sync::Semaphore::new(
                usize::try_from(self.config.verify_concurrency.max(1)).unwrap_or(1),
            )),
        });
        Router::new()
            .hoop(TransferGuard(state))
            .push(tus.into_router())
    }

    /// Bind policy (mirrors duthub-mcp `transport.rs::assert_bind_allowed`):
    /// loopback is always allowed; anything non-loopback requires a configured
    /// authorizer — fail closed.
    fn assert_bind_allowed(&self, addr: &SocketAddr) -> Result<(), TransferError> {
        if addr.ip().is_loopback() || self.authorizer.is_some() {
            return Ok(());
        }
        Err(TransferError::BindPolicy(format!(
            "transfer listener {addr} needs an authorizer; unauthenticated endpoints may only bind loopback"
        )))
    }

    /// Claim the transfer directory for this process. Two servers (or two
    /// processes) over one root cannot see each other's in-memory upload locks,
    /// so a second writer could truncate a live upload; the lock file fails
    /// closed instead. Released when the serve handle drops.
    fn acquire_dir_lock(&self) -> Result<std::fs::File, TransferError> {
        let path = self.transfer_dir.join(".duthub-transfer.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(TransferError::Io)?;
        file.try_lock().map_err(|_| {
            TransferError::Locked(format!(
                "{} is already served by another instance",
                self.transfer_dir.display()
            ))
        })?;
        Ok(file)
    }

    /// Serve on loopback (`lanAccess = false`) or `0.0.0.0` (`lanAccess = true`).
    /// Starts the periodic sweep; the returned handle shuts both down.
    pub async fn serve(&self, port: u16) -> Result<TransferListenerHandle, TransferError> {
        let addr = if self.config.lan_access {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))
        } else {
            SocketAddr::from((Ipv4Addr::LOCALHOST, port))
        };
        self.assert_bind_allowed(&addr)?;
        let lock = self.acquire_dir_lock()?;
        let acceptor = TcpListener::new(addr)
            .try_bind()
            .await
            .map_err(TransferError::from_salvo)?;
        let endpoint = acceptor.local_addr().map_err(TransferError::Io)?;

        let server = Server::new(acceptor);
        let shutdown = server.handle();
        let router = self.transfer_router();
        let (gc_stop_tx, gc_stop_rx) = tokio::sync::watch::channel(false);
        let gc_store = Arc::clone(&self.store);
        let gc_period = Duration::from_secs(self.config.gc_interval_secs.max(1));
        let join = tokio::spawn(async move {
            let gc = tokio::spawn(gc_loop(gc_store, gc_period, gc_stop_rx));
            if let Err(error) = server.try_serve(router).await {
                tracing::error!(error = %error, "transfer listener stopped with an error");
            }
            let _ = gc_stop_tx.send(true);
            let _ = gc.await;
        });
        Ok(TransferListenerHandle {
            endpoint,
            shutdown,
            join: Some(join),
            _lock: Some(lock),
        })
    }

    /// Run one sweep now and return the number of expired uploads it removed.
    /// The periodic sweep only runs inside [`TransferServer::serve`].
    pub async fn gc_now(&self) -> u32 {
        match self.store.delete_expired().await {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(error = %error, "transfer sweep failed");
                0
            }
        }
    }

    /// Resolved transfer directory.
    pub fn transfer_dir(&self) -> &Path {
        &self.transfer_dir
    }

    /// Normalized mount path, e.g. `/transfer`.
    pub fn base_path(&self) -> &str {
        &self.base
    }

    /// Store access for hosts and tests asserting the on-disk state.
    pub fn store(&self) -> &ExpiringDiskStore {
        &self.store
    }
}

/// Periodic sweep of unfinished uploads whose TTL has elapsed.
async fn gc_loop(
    store: Arc<ExpiringDiskStore>,
    period: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is immediate, which also clears leftovers at startup.
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(error) = store.delete_expired().await {
                    tracing::warn!(error = %error, "transfer sweep failed");
                }
            }
            _ = stop.changed() => break,
        }
    }
}

/// Lifecycle handle for a served transfer listener (mirrors the MCP
/// `ListenerHandle` shape: explicit shutdown, join, Drop safety).
#[derive(Debug)]
pub struct TransferListenerHandle {
    /// Actual bind address (port resolved when `port: 0` was requested).
    pub endpoint: SocketAddr,
    shutdown: ServerHandle,
    join: Option<JoinHandle<()>>,
    /// Held for the life of the listener: closing the file releases the
    /// exclusive directory lock (F-08).
    _lock: Option<std::fs::File>,
}

impl TransferListenerHandle {
    /// Signal shutdown, let in-flight requests drain, and wait for the service
    /// task to finish.
    pub async fn shutdown_and_join(mut self) {
        self.shutdown.stop_graceful(Some(SHUTDOWN_GRACE));
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for TransferListenerHandle {
    fn drop(&mut self) {
        // Dropping without an explicit shutdown still releases the port.
        self.shutdown.stop_graceful(Some(SHUTDOWN_GRACE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_config(dir: &Path) -> TransferConfig {
        TransferConfig {
            transfer_dir: Some(dir.to_string_lossy().to_string()),
            max_upload_bytes: 1 << 20,
            reserved_disk_bytes: 0,
            ..TransferConfig::default()
        }
    }

    #[test]
    fn new_resolves_dir_and_base_path() {
        let tmp = tempfile::tempdir().unwrap();
        let server = TransferServer::new_in(tmp.path(), test_config(tmp.path()), None).unwrap();
        assert_eq!(server.transfer_dir(), tmp.path());
        assert_eq!(server.store().root(), tmp.path());
        assert_eq!(server.base_path(), "/transfer");
    }

    #[test]
    fn transfer_dir_rejections_surface_as_config_errors() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["../out", "D:images", "\\images", "\\\\srv\\share\\imgs"] {
            let config = TransferConfig {
                transfer_dir: Some(bad.to_string()),
                ..test_config(tmp.path())
            };
            let error = match TransferServer::new_in(tmp.path(), config, None) {
                Err(error) => error,
                Ok(_) => panic!("expected a config error for {bad:?}"),
            };
            assert!(matches!(error, TransferError::Config(_)), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn bind_policy_fails_closed_for_non_loopback_without_authorizer() {
        let tmp = tempfile::tempdir().unwrap();
        let config = TransferConfig {
            lan_access: true,
            ..test_config(tmp.path())
        };
        let server = TransferServer::new_in(tmp.path(), config, None).unwrap();
        let error = server.serve(0).await.unwrap_err();
        assert!(matches!(error, TransferError::BindPolicy(_)));
    }

    #[tokio::test]
    async fn bind_policy_allows_loopback_and_authorized_lan() {
        let tmp = tempfile::tempdir().unwrap();
        // Loopback without authorizer is allowed (all writes then 401).
        let plain = TransferServer::new_in(
            tmp.path().join("a"),
            test_config(tmp.path().join("a").as_path()),
            None,
        )
        .unwrap();
        let handle = plain.serve(0).await.unwrap();
        assert!(handle.endpoint.ip().is_loopback());
        handle.shutdown_and_join().await;
        // LAN with an authorizer is allowed and binds 0.0.0.0.
        let scopes = HashMap::new();
        let authorized = TransferServer::new_in(
            tmp.path().join("b"),
            {
                let mut config = test_config(tmp.path().join("b").as_path());
                config.lan_access = true;
                config
            },
            Some(Arc::new(crate::authorize::StaticTokenAuthorizer::new(
                scopes,
            ))),
        )
        .unwrap();
        let handle = authorized.serve(0).await.unwrap();
        assert!(handle.endpoint.ip().is_unspecified());
        handle.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn gc_now_reports_zero_on_an_empty_root() {
        let tmp = tempfile::tempdir().unwrap();
        let server = TransferServer::new_in(tmp.path(), test_config(tmp.path()), None).unwrap();
        assert_eq!(server.gc_now().await, 0);
    }
}
