//! Deny-read planning and application for the protected paths.
//!
//! Derived from openai/codex `codex-rs/windows-sandbox-rs` (Apache-2.0),
//! `deny_read_acl.rs`: the same "lexical path + existing canonical target"
//! planning, the same materialisation of missing paths (so a sandbox cannot
//! create a previously absent denied path and read it in the same run), and the
//! same rollback of this call's ACEs when one of them fails.
//!
//! The ACE is issued to the **sandbox identity** SID, never to the host user:
//! a deny ACE for the host user SID would also deny duthub itself, which has to
//! keep reading its own configuration and evidence directories.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Security::PSID;

use super::acl::{self, PinnedPath};
use crate::policy::SandboxError;

/// The exact paths that receive a deny-read ACE.
///
/// Both the lexical path (what the policy named, so a later run can materialise
/// it) and, when it exists, its canonical target (so the same object cannot be
/// reached through its resolved location) are included. Filesystem roots are
/// refused after canonicalisation: an ACE there would deny a whole volume.
pub(crate) fn plan(paths: &[PathBuf]) -> Result<Vec<PathBuf>, SandboxError> {
    let mut planned: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for path in paths {
        push_unique(&mut planned, &mut seen, path.clone());
        match std::fs::canonicalize(path) {
            Ok(canonical) => push_unique(&mut planned, &mut seen, canonical),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(SandboxError::Setup(format!(
                    "无法规范化拒绝读取路径 {}: {error}",
                    path.display()
                )));
            }
        }
    }
    if let Some(root) = planned
        .iter()
        .find(|path| path.is_absolute() && path.parent().is_none())
    {
        return Err(SandboxError::PolicyRejected(format!(
            "拒绝在文件系统根目录上写 deny ACE: {}",
            root.display()
        )));
    }
    Ok(planned)
}

fn push_unique(planned: &mut Vec<PathBuf>, seen: &mut HashSet<String>, path: PathBuf) {
    if seen.insert(path_key(&path)) {
        planned.push(path);
    }
}

fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    // `canonicalize` returns a `\\?\` verbatim path; the plain spelling and the
    // verbatim spelling name the same object, so both collapse to one key.
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// Apply the deny-read ACEs, rolling back everything this call added if any step
/// fails (a half-applied boundary is worse than none: callers would report
/// protection that only covers some of the paths).
pub(crate) fn apply(paths: &[PathBuf], sid: PSID) -> Result<Vec<PathBuf>, SandboxError> {
    let planned = plan(paths)?;
    let mut applied = Vec::new();
    let mut added_here: Vec<PathBuf> = Vec::new();
    for path in planned {
        let result = (|| -> Result<bool, SandboxError> {
            if !path.exists() {
                std::fs::create_dir_all(&path).map_err(|error| {
                    SandboxError::Setup(format!("无法创建拒绝读取路径 {}: {error}", path.display()))
                })?;
            }
            let pinned = PinnedPath::open(&path)?;
            acl::deny_read_pinned(&pinned, sid)
        })();
        match result {
            Ok(added) => {
                if added {
                    added_here.push(path.clone());
                }
                applied.push(path);
            }
            Err(error) => {
                for applied_path in &added_here {
                    if let Ok(pinned) = PinnedPath::open(applied_path) {
                        let _ = acl::revoke_pinned(&pinned, sid);
                    }
                }
                return Err(error);
            }
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duthub-deny-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create dir");
        dir
    }

    #[test]
    fn missing_paths_are_planned_not_dropped() {
        let dir = scratch("missing");
        let missing = dir.join("future-secret");
        assert_eq!(
            plan(std::slice::from_ref(&missing)).expect("plan"),
            vec![missing.clone()]
        );
        // Applying materialises it, so the sandbox cannot create-and-read it.
        let sid = super::super::OwnedSid::from_string("S-1-5-21-1-2-3-1700").expect("sid");
        let applied = apply(std::slice::from_ref(&missing), sid.as_ptr()).expect("apply");
        assert!(missing.is_dir(), "缺失路径必须被创建为目录");
        assert!(applied.iter().any(|path| path == &missing));
        assert!(acl::has_deny_read_ace(&missing, sid.as_ptr()).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_object_under_two_spellings_is_planned_once() {
        let dir = scratch("canonical");
        let file = dir.join("secret.env");
        std::fs::write(&file, "secret").expect("write");
        let planned = plan(std::slice::from_ref(&file)).expect("plan");
        assert!(planned.contains(&file));
        assert_eq!(
            planned.len(),
            1,
            "同一对象的不同写法只应产生一个 ACE: {planned:?}"
        );
        // A path that canonicalises to a *different* object (a directory
        // symlink/junction) lands in the plan twice, so both spellings get an
        // ACE. Creating one needs either Developer Mode or SeCreateSymbolicLink;
        // without it the case is reported as skipped.
        let target = dir.join("target");
        let link = dir.join("link");
        std::fs::create_dir_all(&target).expect("create link target");
        match std::os::windows::fs::symlink_dir(&target, &link) {
            Ok(()) => {
                let planned = plan(std::slice::from_ref(&link)).expect("plan through symlink");
                assert_eq!(planned.len(), 2, "{planned:?}");
                // And a pinned open refuses to follow it (fail closed, no ACE
                // can be written through a reparse point).
                assert!(
                    PinnedPath::open(&link).is_err(),
                    "PinnedPath::open must refuse a reparse point"
                );
            }
            Err(error) => eprintln!("SKIP symlink half: 无法创建目录符号链接（{error}）"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicates_and_trailing_separators_collapse() {
        let dir = scratch("dupes");
        let with_separator = PathBuf::from(format!("{}\\", dir.display()));
        let planned = plan(&[dir.clone(), with_separator]).expect("plan");
        assert_eq!(planned.len(), 1, "{planned:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filesystem_roots_are_refused() {
        let cwd = std::env::current_dir().expect("cwd");
        let root = cwd.ancestors().last().expect("filesystem root");
        let error = plan(&[root.to_path_buf()]).expect_err("root must be refused");
        assert!(matches!(error, SandboxError::PolicyRejected(_)), "{error}");
    }
}
