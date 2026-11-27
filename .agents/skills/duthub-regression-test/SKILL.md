---
name: duthub-regression-test
description: Run duthub Rust regression gates, Clippy pedantic checks, scoped Miri verification, and native desktop tests.
metadata:
  internal: true
---

# duthub-regression-test

- Run `bash scripts/regression` from the repository root in Git Bash on Windows (keeps all Cargo artifacts inside repo `target/`; never set `CARGO_TARGET_DIR` outside the repo). The gate enforces `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` (including `clippy::pedantic`, `undocumented_unsafe_blocks`, `missing_safety_doc`, `await_holding_lock`, and `missing_debug_implementations`), and `cargo test --workspace`.
- Run `cargo test --workspace --all-features` for full-feature validation. Avoid concurrent `cargo` invocations against the same `target/` on Windows.
- When touching `unsafe`, memory layout, or core pure-Rust logic, run `cargo +nightly miri test` on affected pure-Rust targets (e.g., `duthub-core`, `duthub-device`, `duthub-transfer`, `duthub-control`; set `TEMP`/`TMP` to an isolated folder outside any `Cargo.toml` if the host temp dir interferes with sysroot builds). Pair Miri-unsupported Win32 FFI paths (`apps/desktop/src/tray.rs`, `agent_host/job.rs`) with native Windows tests (`tray_win32`, `goose_job_kill`, `agent_host::job::tests`) and report exact coverage boundaries.
- Pass `--desktop-self-test` only when window rendering changed. Report software-fixture, native-Windows, and real-hardware results separately. See [repository rules](../../../AGENTS.md).
