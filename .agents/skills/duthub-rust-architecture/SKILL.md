---
name: duthub-rust-architecture
description: Design or review duthub Rust boundaries, public APIs, unsafe code, runtime ownership, and persistence.
metadata:
  internal: true
---

# duthub-rust-architecture

- Trace callers against [architecture](../../../docs/ARCHITECTURE.md) and [data model](../../../docs/DATA_MODEL.md). Keep GPUI routing, runtime ownership, and persisted JSON/MCP/A2A wire contracts explicit.
- Enforce workspace Rust standards ([AGENTS.md](../../../AGENTS.md)): `#![forbid(unsafe_code)]` on all `crates/*`, `clippy::pedantic` clean, `thiserror` typed errors (`#[non_exhaustive]` on public library error enums), `anyhow` at CLI/app entrypoints, and `duthub_core::util::{lock,read,write}_unpoisoned` instead of bare lock `.unwrap()`.
- Prefer `&str`/`&[T]`, `match` over chained `if / else if`, checked casts (`try_from`/`cast_signed`/`cast_unsigned`), `#[must_use]`, and `Debug` on all public types (manual redacted `Debug` for secret/token holders). Use `#[expect(..., reason = "...")]` instead of `#[allow(...)]`.
- Keep all comments, rustdoc, logs, and diagnostics in English without historical dates. Isolate Win32 FFI `unsafe` in `apps/desktop` with per-block `// SAFETY:` and per-`unsafe fn` `# Safety` docs; never hold sync locks across `.await`.
- Verify cancellation, exclusive leases, late events, reconnect, rollback, and separate display frames from persisted serial evidence. Record scoped contract changes in `specs/` and follow [regression testing](../duthub-regression-test/SKILL.md).
