---
name: duthub-rust-architecture
description: Design and review Duthub Rust crate boundaries, ownership, persistence and Windows/Linux behavior.
---

# duthub-rust-architecture

- Follow active callers and keep UI routing in GPUI, hardware actions in the owning service, and shared policy and persistence in core.
- Check cancellation, exclusive access, reconnect, late events, retry safety and the legacy JSON roundtrip. Keep screen snapshots separate from persisted serial evidence.
- Resolve work directory using CLI --workdir, saved preference, then executable directory. Keep bootstrap config beside the executable and DUT artifacts in their owned directory. See [data model](../../../docs/DATA_MODEL.md) and run the workspace regression gate.
