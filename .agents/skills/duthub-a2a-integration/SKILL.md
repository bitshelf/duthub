---
name: duthub-a2a-integration
description: Design or verify duthub A2A Agent Card discovery, loopback JSON-RPC and DUT-scoped evidence.
---

# duthub-a2a-integration

- Follow the [A2A contract](../../../docs/API_CONTRACT.md#a2a-端点实验性未经产品签字) and `crates/duthub-a2a`. The desktop starts saved per-DUT loopback listeners. Task state is in memory.
- Verify Agent Card discovery, Bearer authorization, DUT scope, task completion and cancellation. Use the A2A end-to-end tests and redact credentials. Check [ADR status](../../../docs/decisions/ADR_SIGNOFF.md) before claiming LAN support or a product commitment.
