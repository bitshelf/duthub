---
name: duthub-rust-architecture
description: Design or review duthub Rust boundaries, runtime ownership, data structures and persistence.
---

# duthub-rust-architecture

- Trace production callers using the [architecture](../../../docs/ARCHITECTURE.md) and [data model](../../../docs/DATA_MODEL.md). Keep GPUI routing, runtime ownership and persisted contracts explicit.
- Verify cancellation, exclusive access, late events, reconnect and rollback. Keep display frames and persisted serial evidence distinct. Check the wire and fixture contracts before retiring a reader or compatibility path.
- For scoped refactors, record observable behavior and acceptance in the corresponding `specs/` feature. Use the [documentation index](../../../docs/README.md) and the [regression skill](../duthub-regression-test/SKILL.md).
