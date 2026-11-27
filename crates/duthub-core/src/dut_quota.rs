//! Per-DUT retention for application-managed file writes.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, ErrorKind, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::dut_artifacts::{Dir, OpenOptions, create_new, open_logs_for_write, plain};

pub const DEFAULT_DUT_STORAGE_GIB: u32 = 10;
pub const GIB: u64 = 1024 * 1024 * 1024;

pub struct DutQuota {
    bytes: AtomicU64,
    meters: Mutex<HashMap<String, Arc<Mutex<Meter>>>>,
}

impl std::fmt::Debug for DutQuota {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DutQuota")
            .field("limit_bytes", &self.limit())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Meter {
    measured_at: Option<Instant>,
    used: u64,
    active: HashSet<String>,
}

impl DutQuota {
    #[must_use]
    pub fn new(bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes: AtomicU64::new(bytes),
            meters: Mutex::new(HashMap::new()),
        })
    }

    pub fn limit(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn set_limit(&self, bytes: u64) {
        self.bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn create(
        self: &Arc<Self>,
        dut_id: &str,
        owner: &Dir,
        path: &str,
    ) -> io::Result<QuotaFile> {
        self.open(dut_id, owner, path, |owner| create_new(owner, path))
    }

    pub fn append_log(
        self: &Arc<Self>,
        dut_id: &str,
        owner: &Dir,
        name: &str,
    ) -> io::Result<QuotaFile> {
        let name = crate::dut_artifacts::name_component(name)?;
        self.open(dut_id, owner, &format!("logs/{name}"), |owner| {
            let dir = open_logs_for_write(owner)?;
            match dir.symlink_metadata(name) {
                Ok(meta) if meta.is_file() && plain(&meta) => {}
                Ok(_) => return Err(ErrorKind::PermissionDenied.into()),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            dir.open_with(name, OpenOptions::new().create(true).append(true))
                .map(cap_std::fs::File::into_std)
        })
    }

    fn open(
        self: &Arc<Self>,
        dut_id: &str,
        owner: &Dir,
        path: &str,
        open_file: impl FnOnce(&Dir) -> io::Result<File>,
    ) -> io::Result<QuotaFile> {
        let meter = self
            .meters
            .lock()
            .unwrap()
            .entry(dut_id.to_owned())
            .or_default()
            .clone();
        let owner = owner.try_clone()?;
        let file = {
            let mut usage = crate::util::lock_unpoisoned(&meter);
            if !usage.active.insert(path.to_owned()) {
                return Err(ErrorKind::AlreadyExists.into());
            }
            match usage
                .make_room(&owner, 1, self.limit())
                .and_then(|()| open_file(&owner))
            {
                Ok(file) => file,
                Err(error) => {
                    usage.active.remove(path);
                    return Err(error);
                }
            }
        };
        Ok(QuotaFile {
            file,
            meter,
            owner,
            path: path.to_owned(),
            quota: self.clone(),
        })
    }
}

impl Meter {
    fn make_room(&mut self, dir: &Dir, requested: u64, limit: u64) -> io::Result<()> {
        if requested > limit {
            return Err(io::Error::new(
                ErrorKind::StorageFull,
                "写入大小超过 DUT 容量上限",
            ));
        }
        if self
            .measured_at
            .is_some_and(|time| time.elapsed() < Duration::from_secs(2))
            && self.used <= limit - requested
        {
            return Ok(());
        }
        let (used, mut files) = measure(dir)?;
        self.used = used;
        self.measured_at = Some(Instant::now());
        if self.used <= limit - requested {
            return Ok(());
        }
        files.retain(|(_, path, _)| !self.active.contains(path));
        let reclaimable = files
            .iter()
            .fold(0u64, |total, (_, _, bytes)| total.saturating_add(*bytes));
        if reclaimable < self.used.saturating_sub(limit - requested) {
            return Err(io::Error::new(
                ErrorKind::StorageFull,
                "DUT 容量已满，当前没有足够的旧文件可清理",
            ));
        }
        files.sort_unstable();
        for (_, path, bytes) in files {
            if dir
                .symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.is_file() && plain(&metadata))
                && dir.remove_file(&path).is_ok()
            {
                self.used = self.used.saturating_sub(bytes);
                if self.used <= limit - requested {
                    return Ok(());
                }
            }
        }
        Err(io::Error::new(
            ErrorKind::StorageFull,
            "DUT 容量已满，当前没有可清理的旧文件",
        ))
    }
}

type RetainedFile = (Option<cap_std::time::SystemTime>, String, u64);

fn measure(dir: &Dir) -> io::Result<(u64, Vec<RetainedFile>)> {
    let mut used = 0u64;
    let mut files = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(prefix) = pending.pop() {
        let entries = if prefix.is_empty() {
            dir.entries()?
        } else {
            let metadata = dir.symlink_metadata(&prefix)?;
            if !metadata.is_dir() || !plain(&metadata) {
                continue;
            }
            dir.read_dir(&prefix)?
        };
        for entry in entries {
            let entry = entry?;
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                continue;
            };
            let path = if prefix.is_empty() {
                filename.to_owned()
            } else {
                format!("{prefix}/{filename}")
            };
            let metadata = dir.symlink_metadata(&path)?;
            if !plain(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                used = used.saturating_add(metadata.len());
                if metadata.len() > 0 && !path.split('/').any(|part| part.starts_with('.')) {
                    files.push((metadata.modified().ok(), path, metadata.len()));
                }
            }
        }
    }
    Ok((used, files))
}

pub struct QuotaFile {
    file: File,
    meter: Arc<Mutex<Meter>>,
    owner: Dir,
    path: String,
    quota: Arc<DutQuota>,
}

impl std::fmt::Debug for QuotaFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl QuotaFile {
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.file.metadata()?.len() == 0)
    }
}

impl Write for QuotaFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut meter = crate::util::lock_unpoisoned(&self.meter);
        meter.make_room(&self.owner, buffer.len() as u64, self.quota.limit())?;
        let written = self.file.write(buffer)?;
        meter.used = meter.used.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for QuotaFile {
    fn drop(&mut self) {
        let mut meter = crate::util::lock_unpoisoned(&self.meter);
        meter.active.remove(&self.path);
        if self
            .file
            .metadata()
            .is_ok_and(|metadata| metadata.len() == 0)
        {
            let _ = self.owner.remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn quota_evicts_the_oldest_inactive_file_in_only_the_writing_dut() {
        let root = tempfile::tempdir().unwrap();
        let (_, a) = crate::dut_artifacts::open_dut_for_write(root.path(), "a", "RK3576").unwrap();
        let (_, b) = crate::dut_artifacts::open_dut_for_write(root.path(), "b", "Other").unwrap();
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        for (name, timestamp) in [("old.img", 0), ("new.img", 10)] {
            let mut file = create_new(&a, name).unwrap();
            file.write_all(&[0; 32]).unwrap();
            file.set_modified(start + Duration::from_secs(timestamp))
                .unwrap();
        }
        create_new(&b, "protected.img")
            .unwrap()
            .write_all(&[0; 80])
            .unwrap();
        let quota = DutQuota::new(100);
        let mut active = quota.create("a", &a, "active.txt").unwrap();
        active.write_all(&[1; 20]).unwrap();
        let mut new = quota.create("a", &a, "latest.txt").unwrap();
        new.write_all(&[2; 20]).unwrap();
        assert!(!a.exists("old.img"));
        assert!(a.exists("new.img"));
        assert!(a.exists("active.txt"));
        assert!(b.exists("protected.img"));
        assert!(measure(&a).unwrap().0 <= quota.limit());
    }
}
