---
name: duthub-a2a-integration
description: Design or verify duthub A2A Agent Card discovery, loopback JSON-RPC, and DUT-scoped evidence.
metadata:
  internal: true
---

# duthub-a2a-integration

- Follow the [A2A contract](../../../docs/API_CONTRACT.md#a2a-端点实验性未经产品签字) and `crates/duthub-a2a` (`#![forbid(unsafe_code)]`). Each saved per-DUT listener binds `127.0.0.1` fail-closed; task state is in memory only.
- Verify minimal Agent Card discovery (no DUT names, COM ports, paths, hostnames, or tokens), constant-time Bearer authorization, strict 3-method JSON-RPC surface (`SendMessage`/`GetTask`/`CancelTask`), per-DUT evidence scope, and redacted `Debug`/logs.
- Run `cargo test -p duthub-a2a` and check [ADR signoff](../../../docs/decisions/ADR_SIGNOFF.md) before claiming LAN support or product commitments.
