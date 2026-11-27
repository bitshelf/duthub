//! Source-level contracts pinning the MXC sandbox architecture (ADR-12):
//!
//! 1. `duthub-sandbox` forbids `unsafe` code (`#![forbid(unsafe_code)]`) and
//!    has no `windows-sys` dependency.
//! 2. `duthub-sandbox` never falls back to an unsandboxed process (`Command`).
//! 3. Low-level Win32 spawning/container APIs do not appear anywhere in the
//!    workspace — all OS containment goes through `mxc-sdk`.
//! 4. Policy types are not duplicated in `duthub-core`.
//! 5. Security invariants T1–T8 stay present and ungated (no `#[ignore]`).

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
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
fn sandbox_crate_forbids_unsafe_and_drops_raw_win32_deps() {
    let lib = std::fs::read_to_string(manifest_dir().join("src").join("lib.rs")).expect("read lib");
    assert!(
        lib.contains("#![forbid(unsafe_code)]"),
        "duthub-sandbox must declare #![forbid(unsafe_code)]"
    );
    let toml = std::fs::read_to_string(manifest_dir().join("Cargo.toml")).expect("read Cargo.toml");
    assert!(
        !toml.contains("windows-sys"),
        "duthub-sandbox must delegate OS isolation to mxc-sdk instead of windows-sys"
    );
    assert!(
        toml.contains("mxc-sdk"),
        "duthub-sandbox must depend on mxc-sdk"
    );
    assert!(
        !manifest_dir().join("src").join("windows").exists(),
        "legacy src/windows FFI directory must stay removed"
    );
}

#[test]
fn the_crate_never_runs_an_unsandboxed_command() {
    let src = manifest_dir().join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert_ne!(files, [] as [std::path::PathBuf; 0]);
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
fn raw_sandbox_spawning_apis_do_not_exist_in_workspace() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("apps"), &mut files);
    let own_test = manifest_dir().join("tests").join("source_contract.rs");
    let mut scanned = 0usize;
    for file in &files {
        if file == &own_test {
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
                "{} contains `{forbidden}`: OS containment must go through mxc-sdk",
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
        "duthub-core/src/os_sandbox.rs must stay removed"
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
        !content.contains("skip_without_identity"),
        "T3/T4 must run unconditionally under mxc-sdk PSEC without skipping"
    );
}
