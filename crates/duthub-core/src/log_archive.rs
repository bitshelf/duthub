use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;

pub const CONTINUOUS_SERIAL_LOG: &str = "serial.txt";

#[must_use]
pub fn dut_folder(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .take(80)
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);
    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
    if cleaned.is_empty() {
        "DUT".into()
    } else if reserved {
        format!("_{cleaned}")
    } else {
        cleaned.into()
    }
}

#[must_use]
pub fn archive_prefix(dut_id: &str) -> String {
    let hex = crate::util::hex_lower(dut_id.as_bytes());
    format!("dut-{hex}-")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub id: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn numbered_suffix(suffix: &str) -> bool {
    suffix.is_empty()
        || suffix
            .strip_prefix('.')
            .is_some_and(|n| n.parse::<u32>().is_ok())
}

fn matching_files(dir: &Path, dut_id: &str, archived: bool) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let prefix = archive_prefix(dut_id);
    let legacy = format!("sess-{dut_id}.log");
    entries
        .flatten()
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            let name = entry.file_name();
            let name = name.to_str()?;
            let belongs = if archived {
                name.starts_with(&prefix)
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
            } else {
                name.strip_prefix(&legacy).is_some_and(numbered_suffix)
            };
            belongs.then(|| entry.path())
        })
        .collect()
}

fn segment_files(work_dir: &Path, dut_id: &str) -> Vec<(String, PathBuf)> {
    let logs = work_dir.join("logs");
    let mut paths = Vec::new();
    if let Some(root) = portable_root(work_dir)
        && let Ok(entries) = fs::read_dir(root)
    {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(dut_dir) = crate::dut_artifacts::open(root, dut_id, name, false) else {
                continue;
            };
            let log_dirs = [
                (
                    crate::dut_artifacts::SERIAL_LOGS_DIR,
                    crate::dut_artifacts::open_logs(&dut_dir),
                ),
                (
                    crate::dut_artifacts::LEGACY_LOGS_DIR,
                    crate::dut_artifacts::open_legacy_logs(&dut_dir),
                ),
            ];
            for (subdir, opened) in log_dirs {
                let Ok(dir) = opened else {
                    continue;
                };
                if let Ok(files) = dir.entries() {
                    for file in files
                        .flatten()
                        .filter(|file| file.file_type().is_ok_and(|kind| kind.is_file()))
                    {
                        let filename = file.file_name();
                        if let Some(filename) = filename.to_str()
                            && (filename == CONTINUOUS_SERIAL_LOG
                                || crate::dut_artifacts::log_sequence(filename).is_some())
                        {
                            paths.push((
                                format!("dut/{name}/{subdir}/{filename}"),
                                entry.path().join(subdir).join(filename),
                            ));
                        }
                    }
                }
            }
        }
    }
    if portable_root(work_dir).is_some() {
        paths.sort_by_cached_key(|(_, path)| {
            std::cmp::Reverse((
                fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .ok(),
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(crate::dut_artifacts::log_sequence),
            ))
        });
        return paths;
    }
    if let Ok(folders) = fs::read_dir(&logs) {
        for entry in folders
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        {
            for path in matching_files(&entry.path(), dut_id, true) {
                let id = format!(
                    "{}/{}",
                    entry.file_name().to_string_lossy(),
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                paths.push((id, path));
            }
        }
    }
    for (scope, dir) in std::iter::once(("flat", logs)) {
        for path in matching_files(&dir, dut_id, false) {
            let id = format!(
                "{scope}/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            paths.push((id, path));
        }
    }
    paths.sort_unstable_by(|(id_a, a), (id_b, b)| {
        let key = |p: &Path| {
            let filename = p.file_name().unwrap_or_default().to_string_lossy();
            let (stem, suffix) = filename.split_once(".log").unwrap_or((&filename, ""));
            (
                stem.to_owned(),
                suffix.trim_start_matches('.').parse::<u32>().unwrap_or(0),
            )
        };
        key(b).cmp(&key(a)).then_with(|| id_a.cmp(id_b))
    });
    paths
}

fn portable_root(work_dir: &Path) -> Option<&Path> {
    work_dir.parent().filter(|_| {
        work_dir
            .file_name()
            .is_some_and(|name| name == "duthub-data")
    })
}

fn open_segment(work_dir: &Path, dut_id: &str, id: &str, path: &Path) -> std::io::Result<File> {
    if id.starts_with("dut/")
        && let Some(root) = portable_root(work_dir)
    {
        let relative = id
            .strip_prefix("dut/")
            .ok_or(std::io::ErrorKind::InvalidData)?;
        let (name, file) = relative
            .split_once('/')
            .ok_or(std::io::ErrorKind::InvalidData)?;
        let dir = crate::dut_artifacts::open(root, dut_id, name, false)?;
        let dir = if file.starts_with(&format!("{}/", crate::dut_artifacts::SERIAL_LOGS_DIR)) {
            crate::dut_artifacts::open_logs(&dir)?
        } else if file.starts_with(&format!("{}/", crate::dut_artifacts::LEGACY_LOGS_DIR)) {
            crate::dut_artifacts::open_legacy_logs(&dir)?
        } else {
            dir
        };
        return dir
            .open(path.file_name().ok_or(std::io::ErrorKind::InvalidData)?)
            .map(cap_std::fs::File::into_std);
    }
    let root = portable_root(work_dir).unwrap_or(work_dir);
    let relative = path
        .strip_prefix(root)
        .map_err(|_| std::io::ErrorKind::PermissionDenied)?;
    let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    dir.open(relative).map(cap_std::fs::File::into_std)
}

#[must_use]
pub fn list(work_dir: &Path, dut_id: &str, offset: usize, limit: usize) -> Vec<Segment> {
    segment_files(work_dir, dut_id)
        .into_iter()
        .skip(offset)
        .take(limit)
        .filter_map(|(id, path)| {
            let file = open_segment(work_dir, dut_id, &id, &path).ok()?;
            let bytes = file.metadata().ok()?.len();
            let mut start = Vec::new();
            let label = BufReader::new(file.take(2048))
                .read_until(b'\n', &mut start)
                .ok()
                .and_then(|_| std::str::from_utf8(&start).ok())
                .and_then(|line| line.trim_end().strip_prefix("[mark] "))
                .map(str::to_owned);
            Some(Segment { id, bytes, label })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::dut_folder;

    #[test]
    fn portable_segments_use_the_dut_owner() {
        use super::{list, read};
        use std::io::Write;

        let portable = tempfile::tempdir().unwrap();
        let archive = portable.path().join("duthub-data");
        let target = crate::dut_artifacts::open_archive(portable.path(), "a", "RK3576").unwrap();
        let filename = "RK3576-001_20260918_114520.txt";
        crate::dut_artifacts::create_new(&target, filename)
            .unwrap()
            .write_all(b"serial evidence")
            .unwrap();
        let entries = list(&archive, "a", 0, 20);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].id,
            format!(
                "dut/RK3576/{}/{filename}",
                crate::dut_artifacts::SERIAL_LOGS_DIR
            )
        );
        assert!(list(&archive, "b", 0, 20).is_empty());
        assert!(read(&archive, "b", &entries[0].id, 0).is_err());
        assert_eq!(
            read(&archive, "a", &entries[0].id, 0).unwrap().text,
            "serial evidence"
        );

        // Legacy `logs/` files remain readable alongside the new `串口日志/` folder.
        let (_, owner) =
            crate::dut_artifacts::open_dut_for_write(portable.path(), "a", "RK3576").unwrap();
        let legacy_dir = crate::dut_artifacts::open_subdir_for_write(&owner, "logs").unwrap();
        let legacy_name = "RK3576-000_20260918_114500.txt";
        crate::dut_artifacts::create_new(&legacy_dir, legacy_name)
            .unwrap()
            .write_all(b"legacy evidence")
            .unwrap();
        let combined = list(&archive, "a", 0, 20);
        assert_eq!(combined.len(), 2);
        let legacy_id = format!("dut/RK3576/logs/{legacy_name}");
        assert_eq!(
            read(&archive, "a", &legacy_id, 0).unwrap().text,
            "legacy evidence"
        );
    }

    #[test]
    fn dut_folders_preserve_names_and_handle_windows_reserved_paths() {
        assert_eq!(dut_folder("RK3576 开发板"), "RK3576 开发板");
        assert_eq!(dut_folder("COM30"), "COM30");
        assert_eq!(dut_folder("LPT1.txt"), "_LPT1.txt");
        assert_eq!(dut_folder("NUL"), "_NUL");
        assert_eq!(dut_folder(".."), "DUT");
        assert_eq!(dut_folder("board/test"), "board_test");
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentRead {
    pub text: String,
    pub next_offset: u64,
    pub eof: bool,
}

pub fn read(
    work_dir: &Path,
    dut_id: &str,
    segment_id: &str,
    offset: u64,
) -> std::io::Result<SegmentRead> {
    let (id, path) = segment_files(work_dir, dut_id)
        .into_iter()
        .find(|(id, _)| id == segment_id)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "Log segment not found")
        })?;
    let mut file = open_segment(work_dir, dut_id, &id, &path)?;
    let size = file.metadata()?.len();
    let offset = offset.min(size);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024).read_to_end(&mut bytes)?;
    let mut next_offset = offset + bytes.len() as u64;
    if next_offset < size
        && let Err(err) = std::str::from_utf8(&bytes)
        && err.error_len().is_none()
    {
        bytes.truncate(err.valid_up_to());
        next_offset = offset + bytes.len() as u64;
    }
    Ok(SegmentRead {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        next_offset,
        eof: next_offset >= size,
    })
}
