//! Zip member validation (zip-slip guard) and flattened extraction of declared entries.

use std::io::{Read, Seek};
use std::path::Path;

use crate::error::ToolInstallError;
#[cfg(unix)]
use crate::spec::EntryKind;
use crate::spec::ToolSpec;

/// Per-member uncompressed size cap — a sanity guard against archive bombs. Far above
/// anything the built-in tools ship (the biggest member is a ~100MB exe).
const MAX_MEMBER_BYTES: u64 = 512 * 1024 * 1024;

/// Outcome of a successful extraction.
#[derive(Debug)]
pub(crate) struct Extraction {
    /// Extracted basenames in `spec.entries` declaration order (all entries were found).
    pub files: Vec<String>,
}

/// Validate an archive member path and return its path components.
///
/// Rejects (zip-slip guard): empty names, NUL bytes, absolute paths (`/x`, `//unc`),
/// drive letters and drive-relative names (`C:/x`, `c:x`) and any `..` component.
/// Both `/` and `\` count as separators — Windows treats them interchangeably, so a
/// `..\` escape is as dangerous as `../`.
pub(crate) fn validate_member_name(name: &str) -> Result<Vec<String>, ToolInstallError> {
    let reject = || ToolInstallError::UnsafeZipEntry {
        member: name.to_string(),
    };
    if name.is_empty() || name.contains('\0') {
        return Err(reject());
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        // Covers "/abs" and, after backslash folding, the UNC prefix "//server/...".
        return Err(reject());
    }
    let first = normalized.split('/').next().unwrap_or_default();
    let bytes = first.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        // "C:/x" and drive-relative "C:x" alike.
        return Err(reject());
    }
    let components: Vec<String> = normalized.split('/').map(str::to_string).collect();
    if components.iter().any(|c| c == "..") {
        return Err(reject());
    }
    Ok(components)
}

/// Extract the entries declared by `spec` from `reader` into the flat directory `dest`.
///
/// Every archive member name is validated first — the zip-slip guard applies to the whole
/// archive, not only to extracted entries. Member basenames are matched case-insensitively;
/// the first match in archive order wins. Directory members and non-matching members are
/// skipped. A declared entry missing from the archive is a hard error (half-truthful
/// archives must not install).
pub(crate) fn extract_tool<R: Read + Seek>(
    reader: R,
    spec: &ToolSpec,
    dest: &Path,
) -> Result<Extraction, ToolInstallError> {
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|source| ToolInstallError::Zip { source })?;

    // Pass 1 (central directory only, zero decompression): validate EVERY member
    // name against zip-slip and locate the archive index for each declared entry.
    let mut matched_indices: Vec<Option<(usize, String, String, u64)>> =
        vec![None; spec.entries.len()];
    for index in 0..archive.len() {
        let member = archive
            .by_index_raw(index)
            .map_err(|source| ToolInstallError::Zip { source })?;
        let name = member
            .name()
            .map_err(|source| ToolInstallError::Zip { source })?
            .into_owned();
        let components = validate_member_name(&name)?;
        if member.is_dir() {
            continue;
        }
        // Basename matched case-insensitively; the original spelling lands on disk.
        let base = components.last().cloned().unwrap_or_default();
        let matched = spec
            .entries
            .iter()
            .zip(matched_indices.iter())
            .position(|(entry, slot)| slot.is_none() && base.eq_ignore_ascii_case(entry.file_name));
        let Some(pos) = matched else { continue };

        let declared = member.size();
        if declared > MAX_MEMBER_BYTES {
            return Err(ToolInstallError::OversizedZipEntry {
                member: name,
                declared,
            });
        }
        matched_indices[pos] = Some((index, name, base, declared));
    }

    // Every declared entry must have been found before touching disk.
    for (entry, slot) in spec.entries.iter().zip(matched_indices.iter()) {
        if slot.is_none() {
            return Err(ToolInstallError::MissingArchiveEntry {
                tool: spec.id.to_string(),
                file_name: entry.file_name.to_string(),
            });
        }
    }

    // Pass 2: stream each matched entry directly to disk in 64 KiB chunks.
    std::fs::create_dir_all(dest).map_err(|source| ToolInstallError::Io {
        path: dest.to_owned(),
        source,
    })?;
    let mut files = Vec::with_capacity(spec.entries.len());
    let mut buf = vec![0u8; 64 * 1024];
    for (_entry, slot) in spec.entries.iter().zip(matched_indices) {
        let (index, name, base, declared) = slot.expect("missing entries rejected above");
        let member = archive
            .by_index(index)
            .map_err(|source| ToolInstallError::Zip { source })?;
        let target = dest.join(&base);
        let mut out = std::fs::File::create(&target).map_err(|source| ToolInstallError::Io {
            path: target.clone(),
            source,
        })?;
        let mut limited = member.take(declared);
        let mut copied: u64 = 0;
        loop {
            let read = limited
                .read(&mut buf)
                .map_err(|source| ToolInstallError::ArchiveRead {
                    member: name.clone(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            std::io::Write::write_all(&mut out, &buf[..read]).map_err(|source| {
                ToolInstallError::Io {
                    path: target.clone(),
                    source,
                }
            })?;
            copied += read as u64;
        }
        if copied != declared {
            let _ = std::fs::remove_file(&target);
            return Err(ToolInstallError::ArchiveRead {
                member: name,
                source: std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "member ended before its declared size",
                ),
            });
        }
        #[cfg(unix)]
        if _entry.kind == EntryKind::Executable {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));
        }
        files.push(base);
    }
    Ok(Extraction { files })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::spec::{ExtractEntry, ToolSpec, exe, support};

    static DEMO_ENTRIES: [ExtractEntry; 2] = [exe("tool.exe"), support("helper.dll")];

    /// Build an in-memory zip for the extraction tests.
    pub(crate) fn build_zip(members: &[(&str, &[u8])]) -> std::io::Cursor<Vec<u8>> {
        use std::io::Write as _;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::write::ZipWriter::new(&mut buf);
            for (name, data) in members {
                writer
                    .start_file(*name, zip::write::SimpleFileOptions::default())
                    .expect("start_file");
                writer.write_all(data).expect("write member");
            }
            writer.finish().expect("finish zip");
        }
        buf.set_position(0);
        buf
    }

    fn demo_spec() -> ToolSpec {
        ToolSpec {
            id: "demo",
            url: "https://example.invalid/demo.zip",
            pinned_sha256: None,
            entries: &DEMO_ENTRIES,
        }
    }

    #[test]
    fn extracts_matched_members_and_skips_the_rest() {
        let zip = build_zip(&[
            ("tools/tool.exe", &b"MZtool"[..]),
            ("tools/helper.dll", &b"MZdll"[..]),
            ("docs/readme.txt", &b"docs are skipped"[..]),
        ]);
        let dest = tempfile::tempdir().unwrap();
        let extraction = extract_tool(zip, &demo_spec(), dest.path()).unwrap();
        assert_eq!(extraction.files, ["tool.exe", "helper.dll"]);
        assert_eq!(
            std::fs::read(dest.path().join("tool.exe")).unwrap(),
            b"MZtool"
        );
        assert_eq!(
            std::fs::read(dest.path().join("helper.dll")).unwrap(),
            b"MZdll"
        );
        assert!(!dest.path().join("readme.txt").exists());
    }

    #[test]
    fn basename_match_is_case_insensitive() {
        let zip = build_zip(&[
            ("Bin/TOOL.EXE", &b"MZtool"[..]),
            ("bin/Helper.DLL", &b"MZdll"[..]),
        ]);
        let dest = tempfile::tempdir().unwrap();
        let extraction = extract_tool(zip, &demo_spec(), dest.path()).unwrap();
        assert_eq!(extraction.files, ["TOOL.EXE", "Helper.DLL"]);
    }

    #[test]
    fn rejects_parent_dir_escapes() {
        for name in ["../evil.exe", "a/../../evil.exe", "tools\\..\\..\\evil.exe"] {
            let zip = build_zip(&[("tools/tool.exe", &b"MZtool"[..]), (name, &b"evil"[..])]);
            let dest = tempfile::tempdir().unwrap();
            let err = extract_tool(zip, &demo_spec(), dest.path())
                .expect_err("zip-slip member must be rejected");
            let ToolInstallError::UnsafeZipEntry { member } = err else {
                panic!("expected UnsafeZipEntry for {name:?}, got {err:?}");
            };
            assert_eq!(member, name);
            assert!(!dest.path().join("evil.exe").exists());
        }
    }

    #[test]
    fn rejects_absolute_paths_and_drive_letters() {
        for name in [
            "/abs/evil.exe",
            "//server/share/evil.exe",
            "C:/evil.exe",
            "c:\\evil.exe",
            "C:evil.exe",
        ] {
            let zip = build_zip(&[("tools/tool.exe", &b"MZtool"[..]), (name, &b"evil"[..])]);
            let dest = tempfile::tempdir().unwrap();
            let err = extract_tool(zip, &demo_spec(), dest.path())
                .expect_err("absolute/drive member must be rejected");
            assert!(
                matches!(err, ToolInstallError::UnsafeZipEntry { .. }),
                "expected UnsafeZipEntry for {name:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn missing_declared_entry_is_a_hard_error() {
        let zip = build_zip(&[("tools/tool.exe", &b"MZtool"[..])]);
        let dest = tempfile::tempdir().unwrap();
        let err = extract_tool(zip, &demo_spec(), dest.path())
            .expect_err("missing helper.dll must fail the install");
        let ToolInstallError::MissingArchiveEntry { tool, file_name } = err else {
            panic!("expected MissingArchiveEntry, got {err:?}");
        };
        assert_eq!((tool.as_str(), file_name.as_str()), ("demo", "helper.dll"));
        assert!(
            !dest.path().join("tool.exe").exists(),
            "no partial extraction"
        );
    }
}
