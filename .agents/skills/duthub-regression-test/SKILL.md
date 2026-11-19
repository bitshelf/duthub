---
name: duthub-regression-test
description: Run Duthub Rust and native desktop regressions, or verify the archived HTML prototype when requested.
---

# duthub-regression-test

- From the repository root run `bash scripts/regression` for formatting, Clippy, workspace tests and desktop self-test. Use Git Bash on Windows and isolate CARGO_TARGET_DIR from concurrent builds.
- Verify the affected native entry point and persisted state. Check the actual executable path for release validation. Distinguish fixtures from hardware checks.
- For the archived HTML prototype, use the static preview. Wait for the server to start, then open `http://127.0.0.1:8765/duthub.html`. Stop with Ctrl+C.
  - [本地预览服务器.cmd](scripts/本地预览服务器.cmd) serves the static prototype from docs/.
