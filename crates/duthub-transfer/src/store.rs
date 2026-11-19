//! The unfinished-upload TTL on top of the upstream tus `DiskStore`.
//!
//! Storage, locking and finalize-on-complete all belong to salvo's tus module.
//! It leaves expiration to the storage backend on purpose: its own
//! [`DiskStore`] reports no `Expiration` extension so that completed artifacts
//! are never at the mercy of an upload timeout. This decorator adds exactly
//! that extension and nothing else. Every data operation is delegated
//! unchanged; the advertised capability set is the upstream one minus the two
//! creation variants that would let a request skip the declared-length checks
//! (`creation-defer-length`, `creation-with-upload`).

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use salvo::async_trait;
use salvo::tus::{
    ByteStream, DataStore, DiskStore, Extension, Locker, TusError, TusResult, UploadInfo,
};

/// Slack added on top of the declared upload length when checking the budget
/// (the per-upload metadata record and the verification marker).
pub const BUDGET_SLACK_BYTES: u64 = 64 * 1024;

/// Pluggable free-disk-space probe. Production uses `fs4`; tests inject a fake
/// to make the 507 budget path deterministic.
pub trait FreeSpace: Send + Sync {
    fn available_bytes(&self, dir: &Path) -> io::Result<u64>;
}

/// Production probe backed by `fs4::available_space`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Fs4FreeSpace;

impl FreeSpace for Fs4FreeSpace {
    fn available_bytes(&self, dir: &Path) -> io::Result<u64> {
        fs4::available_space(dir)
    }
}

/// Disk budget: `declared + slack + reserved + other in-flight remainders`
/// versus the currently available bytes.
pub fn budget_required(
    declared_len: u64,
    reserved_disk_bytes: u64,
    slack: u64,
    inflight_remaining: u64,
) -> u64 {
    declared_len
        .saturating_add(slack)
        .saturating_add(reserved_disk_bytes)
        .saturating_add(inflight_remaining)
}

/// [`DiskStore`] plus the `expiration` extension for unfinished uploads.
#[derive(Clone)]
pub struct ExpiringDiskStore {
    inner: DiskStore,
    root: PathBuf,
    ttl: Duration,
    /// How long the sweep waits for a per-upload lock held by a live writer.
    lock_wait: Duration,
    /// The locker the tus handlers use, so the sweep never deletes under a
    /// PATCH that is being written.
    locker: Arc<dyn Locker>,
}

impl ExpiringDiskStore {
    pub fn new(
        root: impl Into<PathBuf>,
        ttl: Duration,
        lock_wait: Duration,
        locker: Arc<dyn Locker>,
    ) -> Self {
        let root = root.into();
        Self {
            inner: DiskStore::new().disk_root(root.clone()),
            root,
            ttl,
            lock_wait,
            locker,
        }
    }

    /// Storage root (the `data/` and `metadata/` directories live below it).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Upload ids known to the store: the `<id>.json` records under `metadata/`.
    pub fn ids(&self) -> Vec<String> {
        read_ids_in(&self.root)
    }

    /// Offload directory enumeration to Tokio's blocking pool so async handlers
    /// never stall worker threads on large metadata folders.
    pub async fn ids_async(&self) -> Vec<String> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || read_ids_in(&root))
            .await
            .unwrap_or_default()
    }

    /// Best-effort snapshot of every upload record (unreadable ones are skipped).
    pub async fn uploads(&self) -> Vec<UploadInfo> {
        let mut uploads = Vec::new();
        for id in self.ids_async().await {
            if let Ok(upload) = self.inner.get_upload_file_info(&id).await {
                uploads.push(upload);
            }
        }
        uploads
    }

    /// Bytes still to land for every unfinished upload (budget input).
    pub async fn inflight_remaining_bytes(&self) -> u64 {
        self.uploads()
            .await
            .iter()
            .filter(|upload| Self::is_unfinished(upload))
            .filter_map(|upload| {
                upload
                    .size
                    .map(|size| size.saturating_sub(upload.offset.unwrap_or(0)))
            })
            .sum()
    }

    /// Sidecar recording the digest a completed upload was verified against.
    pub fn verified_marker(&self, id: &str) -> io::Result<PathBuf> {
        self.marker(id, "verified")
    }

    /// Sidecar flagging an upload that must not be published, for as long as
    /// its files are still on disk.
    pub fn failed_marker(&self, id: &str) -> io::Result<PathBuf> {
        self.marker(id, "failed")
    }

    /// Build a marker path. The id is client-derived at the HTTP edge, so it is
    /// validated here too: this seam is public and must not be able to escape
    /// `metadata/` even when a future caller forgets the guard.
    fn marker(&self, id: &str, kind: &str) -> io::Result<PathBuf> {
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe upload id",
            ));
        }
        Ok(self.root.join("metadata").join(format!("{id}.{kind}")))
    }

    /// Payload bytes currently occupied by the store (completed artifacts and
    /// in-flight staging). Ledger/marker files are bookkeeping overhead and do
    /// not count against the DUT capacity.
    pub fn used_bytes(&self) -> u64 {
        dir_size(&self.root.join("data"))
    }

    /// Non-blocking wrapper around [`Self::used_bytes`] for async handlers.
    pub async fn used_bytes_async(&self) -> u64 {
        let data_dir = self.root.join("data");
        tokio::task::spawn_blocking(move || dir_size(&data_dir))
            .await
            .unwrap_or(0)
    }

    /// Evict the oldest completed artifacts until at least `needed` payload
    /// bytes are freed, or nothing evictable is left. In-flight uploads are
    /// never touched: a writer may hold them and the client cannot re-create
    /// them. Returns the bytes freed.
    pub async fn evict_completed(&self, needed: u64) -> u64 {
        if needed == 0 {
            return 0;
        }
        let mut candidates: Vec<(std::time::SystemTime, u64, String)> = Vec::new();
        for id in self.ids_async().await {
            let Ok(upload) = self.inner.get_upload_file_info(&id).await else {
                continue;
            };
            let finished = matches!(
                (upload.size, upload.offset),
                (Some(size), Some(offset)) if size == offset
            );
            if !finished {
                continue;
            }
            let Some(storage) = upload.storage.as_ref() else {
                continue;
            };
            let Ok(metadata) = tokio::fs::metadata(&storage.path).await else {
                continue;
            };
            if metadata.len() == 0 {
                continue;
            }
            candidates.push((
                metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                metadata.len(),
                id,
            ));
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.2.cmp(&right.2)));

        let mut freed = 0u64;
        for (_, size, id) in candidates {
            if freed >= needed {
                break;
            }
            // The same per-upload lock the handlers use: an upload that is
            // somehow active is skipped, not forced.
            let lock = tokio::time::timeout(self.lock_wait, self.locker.write_lock(&id)).await;
            let Ok(Ok(_lock)) = lock else {
                continue;
            };
            if self.inner.remove(&id).await.is_ok() {
                self.forget_markers(&id);
                freed = freed.saturating_add(size);
                tracing::info!(
                    artifact_id = %id,
                    bytes = size,
                    "evicted the oldest completed artifact for the per-DUT quota"
                );
            }
        }
        freed
    }

    /// Flag an upload as rejected (its bytes are not what the client declared).
    /// Write this *before* removing its files: a removal can fail while Windows
    /// still holds a handle on a freshly written file, and the flag is what
    /// tells the sweep to finish the job.
    pub async fn flag_failed(&self, id: &str, reason: &str) -> io::Result<()> {
        let marker = self.failed_marker(id)?;
        // Own state, written before any store call, so the directory may not
        // exist yet on an empty root.
        tokio::fs::create_dir_all(self.root.join("metadata")).await?;
        tokio::fs::write(marker, reason).await
    }

    /// Drop every sidecar of an upload. Best effort.
    fn forget_markers(&self, id: &str) {
        if let Ok(marker) = self.verified_marker(id) {
            let _ = std::fs::remove_file(marker);
        }
        if let Ok(marker) = self.failed_marker(id) {
            let _ = std::fs::remove_file(marker);
        }
    }

    /// `true` while an upload still has bytes (or its length) outstanding.
    fn is_unfinished(upload: &UploadInfo) -> bool {
        // Same rule the tus handlers apply before answering 410 Gone.
        !matches!((upload.size, upload.offset), (Some(size), Some(offset)) if size == offset)
    }

    /// `true` once an unfinished upload is older than `ttl`. Completed uploads
    /// never expire: they are published artifacts.
    pub fn is_expired(upload: &UploadInfo, ttl: Duration) -> bool {
        if ttl.is_zero() || !Self::is_unfinished(upload) {
            return false;
        }
        let Ok(created) = chrono::DateTime::parse_from_rfc3339(&upload.creation_date) else {
            return false;
        };
        let Ok(ttl) = chrono::Duration::from_std(ttl) else {
            return false;
        };
        created
            .checked_add_signed(ttl)
            .is_some_and(|deadline| chrono::Utc::now() > deadline)
    }

    /// Ids the sweep must look at: those with a record, plus those whose only
    /// trace left is their own failed flag.
    fn sweep_ids(&self) -> Vec<String> {
        let mut ids = self.ids();
        let Ok(read_dir) = std::fs::read_dir(self.root.join("metadata")) else {
            return ids;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "failed") {
                continue;
            }
            if let Some(id) = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
        ids
    }

    /// One sweep: discard every flagged upload, then every unfinished upload
    /// whose TTL has elapsed.
    async fn sweep(&self) -> u32 {
        let mut removed = 0;
        for id in self.sweep_ids() {
            let expired = match self.inner.get_upload_file_info(&id).await {
                Ok(upload) => Self::is_expired(&upload, self.ttl),
                Err(_) => false,
            };
            if !self.failed_marker(&id).is_ok_and(|marker| marker.exists()) && !expired {
                continue;
            }
            // Take the same per-upload lock the handlers hold, so a PATCH in
            // flight finishes (or times out) before its upload disappears. A
            // lock that stays busy simply keeps the upload for the next round.
            let lock = tokio::time::timeout(self.lock_wait, self.locker.write_lock(&id)).await;
            let Ok(Ok(_lock)) = lock else {
                continue;
            };
            match self.inner.remove(&id).await {
                Ok(()) => {
                    self.forget_markers(&id);
                    removed += 1;
                }
                // A flagged upload whose record is already gone leaves nothing
                // but its marker behind.
                Err(TusError::NotFound) if !expired => {
                    self.forget_markers(&id);
                    removed += 1;
                }
                Err(_) => {}
            }
        }
        removed + self.sweep_orphans().await
    }

    /// Remove what no upload record points at any more. Upstream finalize can
    /// leave `<id>.bin` behind when the source unlink fails; a crash can leave
    /// `metadata-tmp`; a corrupt ledger or a manually deleted artifact leaves a
    /// stale record. Anything younger than the TTL is left for the next round,
    /// so a live writer never loses its staging file.
    async fn sweep_orphans(&self) -> u32 {
        let mut removed = 0;
        let known: std::collections::HashSet<String> = self.ids().into_iter().collect();

        // Payload files no ledger names (upstream's leftover after a failed
        // source unlink, or a crashed create).
        if let Ok(read_dir) = std::fs::read_dir(self.root.join("data")) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "bin") || !is_stale(&path, self.ttl) {
                    continue;
                }
                let Some(id) = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                else {
                    continue;
                };
                if known.contains(&id) {
                    continue;
                }
                if std::fs::remove_file(&path).is_ok() {
                    self.forget_markers(&id);
                    removed += 1;
                }
            }
        }

        // Atomic-metadata temporaries from a crash mid-commit.
        if let Ok(read_dir) = std::fs::read_dir(self.root.join("metadata-tmp")) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if is_stale(&path, self.ttl) && std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }

        for id in known {
            let sidecar = self.root.join("metadata").join(format!("{id}.json"));
            let part = self.root.join("data").join(format!("{id}.bin"));
            match std::fs::read(&sidecar) {
                // Ledger vanished between the listing and here, or a corrupt
                // one cannot describe its upload: both are garbage once stale.
                Err(error)
                    if error.kind() != io::ErrorKind::NotFound && is_stale(&sidecar, self.ttl) =>
                {
                    let _ = std::fs::remove_file(&sidecar);
                    let _ = std::fs::remove_file(&part);
                    self.forget_markers(&id);
                    removed += 1;
                }
                Ok(bytes) => {
                    // The file is readable but not valid JSON: the ledger is
                    // corrupt and cannot describe its upload. Remove it once
                    // stale instead of retrying forever.
                    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
                        if is_stale(&sidecar, self.ttl) {
                            let _ = std::fs::remove_file(&sidecar);
                            let _ = std::fs::remove_file(&part);
                            self.forget_markers(&id);
                            removed += 1;
                        }
                        continue;
                    }
                    // A finished record whose published file was deleted by
                    // hand leaves a tombstone; drop it once it is old enough.
                    let stale = is_stale(&sidecar, self.ttl);
                    match self.inner.get_upload_file_info(&id).await {
                        Ok(upload) => {
                            let finished = matches!(
                                (upload.size, upload.offset),
                                (Some(size), Some(offset)) if size == offset
                            );
                            let missing = upload.storage.as_ref().is_none_or(|storage| {
                                !std::path::Path::new(&storage.path).exists()
                            });
                            if finished && missing && stale && self.inner.remove(&id).await.is_ok()
                            {
                                let _ = std::fs::remove_file(&part);
                                self.forget_markers(&id);
                                removed += 1;
                            }
                        }
                        Err(TusError::NotFound) => {
                            if stale {
                                let _ = std::fs::remove_file(&sidecar);
                                let _ = std::fs::remove_file(&part);
                                self.forget_markers(&id);
                                removed += 1;
                            }
                        }
                        // Transient I/O error: leave it for the next round.
                        Err(_) => {}
                    }
                }
                Err(_) => {}
            }
        }

        // A verification marker whose upload record is gone has no meaning.
        if let Ok(read_dir) = std::fs::read_dir(self.root.join("metadata")) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "verified")
                    || !is_stale(&path, self.ttl)
                {
                    continue;
                }
                let Some(id) = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                else {
                    continue;
                };
                if self
                    .root
                    .join("metadata")
                    .join(format!("{id}.json"))
                    .exists()
                {
                    continue;
                }
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }

        removed
    }
}

/// Read upload ids (`<id>.json`) from `<root>/metadata`.
fn read_ids_in(root: &Path) -> Vec<String> {
    let Ok(read_dir) = std::fs::read_dir(root.join("metadata")) else {
        return Vec::new();
    };
    read_dir
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .collect()
}

/// `true` once `path` is older than `ttl`. A zero TTL disables the sweep, so
/// nothing is ever stale.
fn is_stale(path: &Path, ttl: Duration) -> bool {
    if ttl.is_zero() {
        return false;
    }
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= ttl)
}

/// Sum of all file sizes below `dir` (best effort).
fn dir_size(dir: &Path) -> u64 {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return 0;
    };
    read_dir
        .filter_map(|item| item.ok())
        .map(|item| {
            if item.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                dir_size(&item.path())
            } else {
                item.metadata().map(|metadata| metadata.len()).unwrap_or(0)
            }
        })
        .sum()
}

#[async_trait]
impl DataStore for ExpiringDiskStore {
    fn extensions(&self) -> HashSet<Extension> {
        HashSet::from([
            Extension::Creation,
            Extension::Termination,
            Extension::Expiration,
        ])
    }

    fn get_expiration(&self) -> Option<Duration> {
        Some(self.ttl)
    }

    async fn create(&self, file: UploadInfo) -> TusResult<UploadInfo> {
        self.inner.create(file).await
    }

    async fn remove(&self, id: &str) -> TusResult<()> {
        let result = self.inner.remove(id).await;
        // The markers are ours, not the store's. They must never outlive their
        // upload — except the failed flag while the files are still stuck, so
        // the sweep keeps retrying.
        if !matches!(result, Err(TusError::Internal(_))) {
            self.forget_markers(id);
        }
        result
    }

    async fn write(&self, id: &str, offset: u64, stream: ByteStream) -> TusResult<u64> {
        self.inner.write(id, offset, stream).await
    }

    async fn get_upload_file_info(&self, id: &str) -> TusResult<UploadInfo> {
        self.inner.get_upload_file_info(id).await
    }

    async fn declare_upload_length(&self, id: &str, length: u64) -> TusResult<()> {
        self.inner.declare_upload_length(id, length).await
    }

    async fn delete_expired(&self) -> TusResult<u32> {
        Ok(self.sweep().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use salvo::tus::MemoryLocker;

    fn store(dir: &Path, ttl: Duration) -> ExpiringDiskStore {
        ExpiringDiskStore::new(
            dir,
            ttl,
            Duration::from_millis(50),
            Arc::new(MemoryLocker::new()),
        )
    }

    /// An upload created `age` ago, already holding `written` of `size` bytes.
    async fn seed(store: &ExpiringDiskStore, id: &str, size: u64, written: u64, age: Duration) {
        let created = chrono::Utc::now() - chrono::Duration::from_std(age).unwrap();
        store
            .create(UploadInfo {
                id: id.to_owned(),
                size: Some(size),
                offset: Some(0),
                metadata: None,
                storage: None,
                creation_date: created.to_rfc3339(),
            })
            .await
            .unwrap();
        if written > 0 {
            let payload = vec![0u8; written as usize];
            let stream = Box::pin(futures_util::stream::once(async move {
                Ok::<_, io::Error>(Bytes::from(payload))
            }));
            store.write(id, 0, stream).await.unwrap();
        }
    }

    fn info(size: Option<u64>, offset: Option<u64>, created_age: Duration) -> UploadInfo {
        UploadInfo {
            id: "upload".to_owned(),
            size,
            offset,
            metadata: None,
            storage: None,
            creation_date: (chrono::Utc::now() - chrono::Duration::from_std(created_age).unwrap())
                .to_rfc3339(),
        }
    }

    #[test]
    fn budget_required_sums_declared_slack_reserved_and_inflight() {
        assert_eq!(budget_required(1000, 500, 64, 200), 1764);
        // Saturating math keeps absurd values from panicking.
        assert_eq!(budget_required(u64::MAX, 1, 1, 1), u64::MAX);
    }

    #[test]
    fn only_unfinished_uploads_expire() {
        let ttl = Duration::from_secs(60);
        let old = Duration::from_secs(120);
        let fresh = Duration::from_secs(10);
        // Completed uploads outlive the TTL, including the zero-length one.
        assert!(!ExpiringDiskStore::is_expired(
            &info(Some(3), Some(3), old),
            ttl
        ));
        assert!(!ExpiringDiskStore::is_expired(
            &info(Some(0), Some(0), old),
            ttl
        ));
        assert!(!ExpiringDiskStore::is_expired(
            &info(Some(3), Some(0), fresh),
            ttl
        ));
        assert!(ExpiringDiskStore::is_expired(
            &info(Some(3), Some(0), old),
            ttl
        ));
        // A deferred length counts as unfinished.
        assert!(ExpiringDiskStore::is_expired(
            &info(None, Some(0), old),
            ttl
        ));
        // A zero TTL disables expiration, mirroring the tus handlers.
        assert!(!ExpiringDiskStore::is_expired(
            &info(Some(3), Some(0), old),
            Duration::ZERO
        ));
    }

    #[test]
    fn capabilities_are_the_narrowed_upstream_set() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_secs(60));
        let extensions = store.extensions();
        assert_eq!(
            extensions,
            HashSet::from([
                Extension::Creation,
                Extension::Termination,
                Extension::Expiration
            ])
        );
        assert_eq!(store.get_expiration(), Some(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn sweep_removes_only_stale_unfinished_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        let ttl = Duration::from_secs(60);
        let store = store(tmp.path(), ttl);
        let old = Duration::from_secs(120);
        seed(&store, "stale", 10, 4, old).await;
        seed(&store, "complete", 4, 4, old).await;
        seed(&store, "fresh", 10, 0, Duration::from_secs(5)).await;

        assert_eq!(store.delete_expired().await.unwrap(), 1);

        let mut remaining = store.ids();
        remaining.sort();
        assert_eq!(remaining, ["complete", "fresh"]);
    }

    #[tokio::test]
    async fn sweep_skips_an_upload_whose_writer_holds_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_secs(60));
        seed(&store, "stale", 10, 0, Duration::from_secs(120)).await;

        let held = store.locker.write_lock("stale").await.unwrap();
        assert_eq!(
            store.delete_expired().await.unwrap(),
            0,
            "writer still active"
        );
        assert_eq!(store.ids(), ["stale"]);
        drop(held);

        assert_eq!(store.delete_expired().await.unwrap(), 1);
        assert!(store.ids().is_empty());
    }

    #[tokio::test]
    async fn inflight_bytes_count_only_what_has_not_landed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_secs(60));
        seed(&store, "half", 100, 40, Duration::ZERO).await;
        seed(&store, "done", 100, 100, Duration::ZERO).await;
        assert_eq!(store.inflight_remaining_bytes().await, 60);
    }

    #[tokio::test]
    async fn remove_drops_the_verification_marker_with_the_upload() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_secs(60));
        seed(&store, "upload", 4, 4, Duration::ZERO).await;
        let marker = store.verified_marker("upload").unwrap();
        std::fs::write(&marker, "blake3:abc").unwrap();

        store.remove("upload").await.unwrap();

        assert!(!marker.exists());
        assert!(store.ids().is_empty());
    }

    #[tokio::test]
    async fn sweep_removes_orphan_payloads_temporaries_and_markers() {
        // Leftovers no ledger points at: upstream's unlink of the staging file
        // failed, a crash left a metadata temp, a ledger is corrupt, or a
        // verification marker outlived its record.
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_millis(1));
        for dir in ["data", "metadata", "metadata-tmp"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        std::fs::write(tmp.path().join("data").join("deadbeef.bin"), b"x").unwrap();
        std::fs::write(tmp.path().join("metadata-tmp").join("x.json.tmp"), b"{}").unwrap();
        std::fs::write(tmp.path().join("metadata").join("bad.json"), b"{not json").unwrap();
        std::fs::write(
            tmp.path().join("metadata").join("orphan.verified"),
            b"blake3:x",
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let removed = store.delete_expired().await.unwrap();
        assert!(removed >= 4, "removed={removed}");
        assert!(!tmp.path().join("data").join("deadbeef.bin").exists());
        assert!(!tmp.path().join("metadata-tmp").join("x.json.tmp").exists());
        assert!(!tmp.path().join("metadata").join("bad.json").exists());
        assert!(!tmp.path().join("metadata").join("orphan.verified").exists());
    }

    #[tokio::test]
    async fn sweep_discards_flagged_uploads_even_when_complete() {
        // A complete upload normally never expires; the failed flag is the
        // one thing that makes the sweep take it anyway.
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_secs(3600));
        seed(&store, "rejected", 4, 4, Duration::ZERO).await;
        seed(&store, "healthy", 4, 4, Duration::ZERO).await;
        store
            .flag_failed("rejected", "checksum mismatch")
            .await
            .unwrap();

        assert_eq!(store.delete_expired().await.unwrap(), 1);
        assert_eq!(store.ids(), ["healthy"]);
        assert!(!store.failed_marker("rejected").unwrap().exists());
    }

    #[tokio::test]
    async fn sweep_clears_a_failed_flag_whose_upload_is_already_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_secs(3600));
        store.flag_failed("vanished", "x").await.unwrap();

        assert_eq!(store.delete_expired().await.unwrap(), 1);
        assert!(!store.failed_marker("vanished").unwrap().exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn failed_flag_survives_a_stuck_removal_until_the_sweep_retries() {
        // Windows holds the completed file open (an AV scan, an indexer):
        // the immediate removal fails, and only the flag remembers that this
        // upload must not stay.
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_secs(3600));
        seed(&store, "stuck", 4, 4, Duration::ZERO).await;
        store
            .flag_failed("stuck", "checksum mismatch")
            .await
            .unwrap();
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(tmp.path().join("data").join("stuck.bin"))
            .unwrap();

        assert!(store.remove("stuck").await.is_err(), "handle is held");
        assert!(
            store.failed_marker("stuck").unwrap().exists(),
            "the flag must outlive a failed removal"
        );
        assert_eq!(store.delete_expired().await.unwrap(), 0);
        assert_eq!(store.ids(), ["stuck"]);
        drop(blocker);

        assert_eq!(store.delete_expired().await.unwrap(), 1);
        assert!(store.ids().is_empty());
        assert!(!store.failed_marker("stuck").unwrap().exists());
    }

    #[tokio::test]
    async fn data_operations_are_delegated_to_the_disk_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), Duration::from_secs(60));
        seed(&store, "upload", 4, 4, Duration::ZERO).await;

        let upload = store.get_upload_file_info("upload").await.unwrap();
        assert_eq!(upload.offset, Some(4));
        // Upstream layout below the configured root.
        assert_eq!(
            upload
                .storage
                .as_ref()
                .map(|storage| storage.type_name.as_str()),
            Some("file")
        );
        assert_eq!(store.root(), tmp.path());
    }
}
