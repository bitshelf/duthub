//! Source-level contracts that pin the security architecture (ADR-12 §3):
//!
//! 1. `unsafe` exists only in `src/windows/`, and every `unsafe` block there
//!    carries a `SAFETY:` note.
//! 2. This crate never falls back to an unsandboxed process (`Command`).
//! 3. Process-spawning / container APIs used by the boundary do not appear in
//!    any other crate or app, so `duthub-sandbox` stays the single owner.
//! 4. The policy types are not duplicated in `duthub-core`.
//! 5. The security tests stay present and ungated (no `#[ignore]`).

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    // Read at runtime: `env!` bakes an absolute path into the binary and
    // breaks after a directory rename (AGENTS.md pitfall #4).
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir()
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn rust_files(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, output);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            output.push(path);
        }
    }
}

#[test]
fn unsafe_is_confined_to_the_windows_backend() {
    let src = manifest_dir().join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(
        !files.is_empty(),
        "no sources found under {}",
        src.display()
    );
    for file in &files {
        let relative = file.strip_prefix(&src).expect("under src");
        let content = std::fs::read_to_string(file).expect("read source");
        if relative.starts_with("windows") {
            if !content.contains("unsafe") {
                continue;
            }
            assert!(
                content.contains("SAFETY:"),
                "{} uses unsafe without a SAFETY note",
                relative.display()
            );
            let blocks = content.matches("unsafe {").count()
                + content.matches("unsafe fn").count()
                + content.matches("unsafe extern").count();
            let notes = content.matches("SAFETY:").count();
            assert!(
                notes >= blocks,
                "{}: {blocks} unsafe blocks but only {notes} SAFETY notes",
                relative.display()
            );
        } else {
            // Documentation legitimately talks *about* unsafe; the contract is
            // about code. Strip line comments (`//`, `///`, `//!`) before the
            // check so prose cannot trip the gate.
            let code: String = content
                .lines()
                .map(|line| {
                    if line.trim_start().starts_with("//") {
                        ""
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !code.contains("unsafe"),
                "{} must stay unsafe-free (all unsafe belongs in src/windows)",
                relative.display()
            );
        }
    }
}

#[test]
fn the_crate_never_runs_an_unsandboxed_command() {
    let src = manifest_dir().join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    for file in &files {
        let content = std::fs::read_to_string(file).expect("read source");
        for forbidden in [
            "std::process::Command",
            "process::Command",
            "Command::new",
            "std::process::Child",
        ] {
            assert!(
                !content.contains(forbidden),
                "{} contains `{forbidden}`: no unsandboxed fallback is allowed",
                file.display()
            );
        }
    }
}

#[test]
fn spawning_apis_stay_inside_the_sandbox_crate() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("apps"), &mut files);
    let own_src = manifest_dir().join("src");
    // This test file contains the forbidden names as data, so scanning it would
    // always fail; every other crate, app and test stays in scope.
    let own_test = manifest_dir().join("tests").join("source_contract.rs");
    let mut scanned = 0usize;
    for file in &files {
        if file.starts_with(&own_src) || file == &own_test {
            continue;
        }
        let content = std::fs::read_to_string(file).expect("read source");
        scanned += 1;
        for forbidden in [
            "CreateProcessAsUserW",
            "CreateProcessWithTokenW",
            "CreateProcessW",
            "ProcessThreadAttributeList",
            "PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES",
            "SECURITY_CAPABILITIES",
        ] {
            assert!(
                !content.contains(forbidden),
                "{} contains `{forbidden}`: the sandbox boundary must stay in duthub-sandbox",
                file.display()
            );
        }
    }
    assert!(
        scanned > 50,
        "expected to scan the workspace, saw {scanned} files"
    );
}

#[test]
fn policy_types_are_not_duplicated_in_core() {
    assert!(
        !manifest_dir()
            .join("..")
            .join("duthub-core")
            .join("src")
            .join("os_sandbox.rs")
            .exists(),
        "duthub-core/src/os_sandbox.rs must be gone: one policy implementation only"
    );
    let core_lib = std::fs::read_to_string(
        manifest_dir()
            .join("..")
            .join("duthub-core")
            .join("src")
            .join("lib.rs"),
    )
    .expect("read core lib.rs");
    assert!(
        !core_lib.contains("os_sandbox"),
        "duthub-core must not declare an os_sandbox module"
    );
}

#[test]
fn security_invariants_stay_present_and_ungated() {
    let tests = manifest_dir().join("tests").join("security_invariants.rs");
    let content = std::fs::read_to_string(&tests).expect("read security invariants");
    for name in [
        "t1_script_writes_inside_its_cell",
        "t2_script_cannot_write_outside_its_cell",
        "t3_script_cannot_read_the_host_config",
        "t4_script_cannot_read_another_dut_cell",
        "t5_script_has_no_loopback_network",
        "t6_grandchild_inherits_the_sandbox",
        "t7_timeout_kills_the_whole_tree",
        "t8_over_limit_requests_are_refused_without_spawning",
    ] {
        assert!(
            content.contains(name),
            "security test `{name}` disappeared; it is the regression gate for the boundary"
        );
    }
    assert!(
        !content.contains("#[ignore]"),
        "security tests must always run, never be ignored"
    );
    assert!(
        content.contains("#[cfg(not(windows))]"),
        "non-Windows builds must keep an explicit Unsupported assertion"
    );
}
