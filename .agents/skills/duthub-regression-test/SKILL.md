---
name: duthub-regression-test
description: Run duthub Rust and native desktop regression, or verify the archived HTML prototype on request.
---

# duthub-regression-test

- Run `bash scripts/regression` at the repository root using Git Bash on Windows. Use the repository's `target/` directory. The default run checks formatting, Clippy and workspace tests. Add `--desktop-self-test` only when native window rendering needs verification. This runs the window check last.
- Verify the actual executable and persisted configuration when relevant. Report fixture results separately from real hardware or tests with optional prerequisites. [Repository rules](../../../AGENTS.md)
- When asked to verify the prototype, use the [static preview script](scripts/本地预览服务器.cmd) and open `http://127.0.0.1:8765/duthub.html` after it starts. Stop the server with Ctrl+C.
