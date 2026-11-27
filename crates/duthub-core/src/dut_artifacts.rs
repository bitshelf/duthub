//! DUT artifacts stay beneath the executable directory, with one owner per DUT folder.

use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
pub use cap_std::fs::{Dir, OpenOptions};

const OWNER: &str = ".duthub-dut-id";

pub(crate) fn plain(metadata: &cap_std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT covers junctions and other Windows redirects.
        metadata.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        !metadata.file_type().is_symlink()
    }
}

pub fn name_component(name: &str) -> io::Result<&str> {
    if name.is_empty() || crate::log_archive::dut_folder(name) != name {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "DUT 名称不能作为文件夹名",
        ));
    }
    Ok(name)
}

/// Open the directory relative to an already trusted installation root.
pub fn open(root: &Path, dut_id: &str, name: &str, create: bool) -> io::Result<Dir> {
    let name = name_component(name)?;
    let root = Dir::open_ambient_dir(root, ambient_authority())?;
    if create {
        match root.create_dir(name) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let metadata = root.symlink_metadata(name)?;
    if !metadata.is_dir() || !plain(&metadata) {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "DUT 目录必须是普通目录",
        ));
    }
    let dir = root.open_dir(name)?;
    if create && !dir.exists(OWNER) && dir.entries()?.next().is_none() {
        match dir.open_with(
            OWNER,
            cap_std::fs::OpenOptions::new().create_new(true).write(true),
        ) {
            Ok(mut owner) => owner.write_all(dut_id.as_bytes())?,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let metadata = dir.symlink_metadata(OWNER)?;
    if !metadata.is_file() || !plain(&metadata) {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "DUT 目录归属未知",
        ));
    }
    let mut owner = Vec::new();
    dir.open(OWNER)?
        .take(dut_id.len() as u64 + 1)
        .read_to_end(&mut owner)?;
    if owner != dut_id.as_bytes() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "目录已归属另一台 DUT",
        ));
    }
    Ok(dir)
}

pub fn create_file(
    root: &Path,
    dut_id: &str,
    name: &str,
    filename: &str,
) -> io::Result<(std::fs::File, PathBuf)> {
    let filename = name_component(filename)?;
    let (name, dir) = existing_or_create(root, dut_id, name)?;
    let file = create_new(&dir, filename)?;
    Ok((file, root.join(name).join(filename)))
}

pub fn create_new(dir: &Dir, filename: &str) -> io::Result<std::fs::File> {
    dir.open_with(
        filename,
        cap_std::fs::OpenOptions::new().create_new(true).write(true),
    )
    .map(cap_std::fs::File::into_std)
}

/// Resolve an existing regular file strictly inside an owned DUT directory
/// (either `<dut>/<file_id>` or `<dut>/images/data/<file_id>`), rejecting any
/// traversal, absolute path, reparse point/junction, or cross-DUT folder.
pub fn resolve_owned_file(
    root: &Path,
    dut_id: &str,
    name: &str,
    file_id: &str,
) -> io::Result<PathBuf> {
    let filename = name_component(file_id)?;
    if filename == OWNER {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "cannot access ownership marker",
        ));
    }
    let folder = name_component(name)?;
    let dir = open(root, dut_id, folder, false)?;
    if let Ok(meta) = dir.symlink_metadata(filename) {
        if !meta.is_file() || !plain(&meta) {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "target must be a plain file inside the DUT directory",
            ));
        }
        return Ok(root.join(folder).join(filename));
    }
    if let Ok(images_meta) = dir.symlink_metadata("images")
        && images_meta.is_dir()
        && plain(&images_meta)
        && let Ok(images) = dir.open_dir("images")
        && let Ok(data_meta) = images.symlink_metadata("data")
        && data_meta.is_dir()
        && plain(&data_meta)
        && let Ok(data) = images.open_dir("data")
        && let Ok(meta) = data.symlink_metadata(filename)
    {
        if !meta.is_file() || !plain(&meta) {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "target must be a plain file inside DUT images/data",
            ));
        }
        return Ok(root.join(folder).join("images").join("data").join(filename));
    }
    Err(io::Error::new(
        ErrorKind::NotFound,
        "file not found inside the DUT directory",
    ))
}

/// Prepare a safe destination path for a new file strictly inside an owned DUT
/// directory without allowing overwrite or path escape.
pub fn prepare_owned_destination(
    root: &Path,
    dut_id: &str,
    name: &str,
    file_id: &str,
) -> io::Result<PathBuf> {
    let filename = name_component(file_id)?;
    if filename == OWNER {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "cannot overwrite ownership marker",
        ));
    }
    let (folder, dir) = existing_or_create(root, dut_id, name)?;
    if dir.symlink_metadata(filename).is_ok() {
        return Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "destination file already exists inside the DUT directory",
        ));
    }
    Ok(root.join(folder).join(filename))
}

pub fn rename_owned(root: &Path, dut_id: &str, old: &str, new: &str) -> io::Result<bool> {
    if old == new {
        return Ok(false);
    }
    let new = name_component(new)?;
    match open(root, dut_id, old, false) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let dir = Dir::open_ambient_dir(root, ambient_authority())?;
    if dir.symlink_metadata(new).is_ok() {
        return Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "目标 DUT 目录已存在",
        ));
    }
    dir.rename(old, &dir, new)?;
    Ok(true)
}

pub fn open_archive(root: &Path, dut_id: &str, original_name: &str) -> io::Result<Dir> {
    let (_, dir) = existing_or_create(root, dut_id, original_name)?;
    open_logs_for_write(&dir)
}

pub fn open_dut_for_write(root: &Path, dut_id: &str, name: &str) -> io::Result<(String, Dir)> {
    existing_or_create(root, dut_id, name)
}

pub fn open_logs_for_write(dir: &Dir) -> io::Result<Dir> {
    open_subdir_for_write(dir, "logs")
}

/// Create a single named child of an owned DUT directory and reject redirected paths.
pub fn open_subdir_for_write(dir: &Dir, name: &str) -> io::Result<Dir> {
    let name = name_component(name)?;
    match dir.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    open_plain_subdir(dir, name)
}

#[must_use]
pub fn log_sequence(name: &str) -> Option<u64> {
    let (prefix, time) = name.strip_suffix(".txt")?.rsplit_once('_')?;
    let (prefix, date) = prefix.rsplit_once('_')?;
    let (dut_name, sequence) = prefix.rsplit_once('-')?;
    if dut_name.is_empty() {
        return None;
    }
    if date.len() != 8
        || time.len() != 6
        || !date
            .bytes()
            .chain(time.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    sequence.parse().ok()
}

pub(crate) fn open_logs(dir: &Dir) -> io::Result<Dir> {
    open_plain_subdir(dir, "logs")
}

fn open_plain_subdir(dir: &Dir, name: &str) -> io::Result<Dir> {
    let metadata = dir.symlink_metadata(name)?;
    if !metadata.is_dir() || !plain(&metadata) {
        return Err(ErrorKind::PermissionDenied.into());
    }
    dir.open_dir(name)
}

#[derive(Debug, serde::Serialize)]
pub struct Artifact {
    pub id: String,
    pub bytes: u64,
}

/// Maximum directory recursion depth when enumerating DUT artifacts.
const MAX_LIST_DEPTH: usize = 16;
/// Maximum number of artifact entries returned by [`list_files`].
const MAX_LIST_ENTRIES: usize = 10_000;

/// List regular files in the owned directory without following junctions or symlinks.
pub fn list_files(root: &Path, dut_id: &str, name: &str) -> io::Result<Vec<Artifact>> {
    let dir = open(root, dut_id, name, false)?;
    let mut files = Vec::new();
    let mut pending = vec![(String::new(), 0usize)];
    while let Some((prefix, depth)) = pending.pop() {
        if files.len() >= MAX_LIST_ENTRIES {
            break;
        }
        let entries = if prefix.is_empty() {
            dir.entries()?
        } else {
            let metadata = dir.symlink_metadata(&prefix)?;
            if !metadata.is_dir() || !plain(&metadata) {
                continue;
            }
            dir.read_dir(&prefix)?
        };
        for entry in entries.flatten() {
            if files.len() >= MAX_LIST_ENTRIES {
                break;
            }
            let filename = entry.file_name();
            let Some(filename) = filename.to_str().filter(|name| public_filename(name)) else {
                continue;
            };
            let id = if prefix.is_empty() {
                filename.to_owned()
            } else {
                format!("{prefix}/{filename}")
            };
            if let Ok(metadata) = dir.symlink_metadata(&id)
                && plain(&metadata)
            {
                if metadata.is_dir() {
                    if depth + 1 < MAX_LIST_DEPTH {
                        pending.push((id, depth + 1));
                    }
                } else if metadata.is_file() {
                    files.push(Artifact {
                        id,
                        bytes: metadata.len(),
                    });
                }
            }
        }
    }
    files.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    Ok(files)
}

pub fn read_file(
    root: &Path,
    dut_id: &str,
    name: &str,
    file_id: &str,
    offset: u64,
) -> io::Result<(Vec<u8>, u64, bool)> {
    if file_id.len() > 4096 || !file_id.split('/').all(public_filename) {
        return Err(ErrorKind::InvalidInput.into());
    }
    let dir = open(root, dut_id, name, false)?;
    let mut parent = dir;
    let mut parts = file_id.split('/');
    let filename = parts.next_back().ok_or(ErrorKind::InvalidInput)?;
    for directory in parts {
        let metadata = parent.symlink_metadata(directory)?;
        if !metadata.is_dir() || !plain(&metadata) {
            return Err(ErrorKind::PermissionDenied.into());
        }
        parent = parent.open_dir(directory)?;
    }
    let metadata = parent.symlink_metadata(filename)?;
    if !metadata.is_file() || !plain(&metadata) {
        return Err(ErrorKind::PermissionDenied.into());
    }
    let mut file = parent.open(filename)?;
    let size = file.metadata()?.len();
    let offset = offset.min(size);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(16 * 1024).read_to_end(&mut bytes)?;
    let next = offset + bytes.len() as u64;
    Ok((bytes, next, next >= size))
}

fn public_filename(filename: &str) -> bool {
    !filename.is_empty()
        && !filename.starts_with('.')
        && !filename.ends_with([' ', '.'])
        && filename.encode_utf16().count() <= 255
        && !filename.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
        && !filename.chars().any(char::is_control)
        && !crate::path_guard::is_windows_reserved_name(filename)
}

fn existing_or_create(root: &Path, dut_id: &str, original_name: &str) -> io::Result<(String, Dir)> {
    match open(root, dut_id, original_name, false) {
        Ok(dir) => return Ok((original_name.to_owned(), dir)),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let base = Dir::open_ambient_dir(root, ambient_authority())?;
    for entry in base.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir()
            && let Some(name) = name.to_str()
            && let Ok(dir) = open(root, dut_id, name, false)
        {
            return Ok((name.to_owned(), dir));
        }
    }
    open(root, dut_id, original_name, true).map(|dir| (original_name.to_owned(), dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_cannot_overwrite_or_reuse_another_dut_directory() {
        let root = tempfile::tempdir().unwrap();
        let (mut file, path) = create_file(root.path(), "dut-a", "RK3576", "image.img").unwrap();
        file.write_all(b"data").unwrap();
        assert_eq!(path, root.path().join("RK3576").join("image.img"));
        assert!(create_file(root.path(), "dut-b", "RK3576", "distinct.png").is_err());
        assert!(create_file(root.path(), "dut-a", "RK3576", "image.img").is_err());
        assert!(create_file(root.path(), "dut-a", "../RK3576", "x").is_err());
        assert!(create_file(root.path(), "dut-a", "RK3576", "../x").is_err());
        assert!(!root.path().join("x").exists());
    }

    #[test]
    fn renamed_dut_uses_its_new_directory_for_late_results() {
        let root = tempfile::tempdir().unwrap();
        let logs = open_archive(root.path(), "dut-a", "Original").unwrap();
        create_new(&logs, "boot.log")
            .unwrap()
            .write_all(b"boot")
            .unwrap();
        drop(logs);
        assert!(rename_owned(root.path(), "dut-a", "Original", "RK3576").unwrap());
        let (_, screenshot) = create_file(root.path(), "dut-a", "Original", "frame.png").unwrap();
        assert_eq!(screenshot, root.path().join("RK3576/frame.png"));
        assert!(!root.path().join("Original").exists());
        let (bytes, next, eof) =
            read_file(root.path(), "dut-a", "RK3576", "logs/boot.log", 0).unwrap();
        assert_eq!((bytes.as_slice(), next, eof), (b"boot".as_slice(), 4, true));
        for path in [
            "../outside",
            "C:/Windows",
            "logs/../frame.png",
            "frame.png:stream",
        ] {
            assert!(read_file(root.path(), "dut-a", "RK3576", path, 0).is_err());
        }
    }
}
