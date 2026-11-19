---
name: duthub-kernel-diagnosis
description: Diagnose embedded Linux boot failures, kernel panics and hangs using Duthub evidence and the matching kernel tree.
---

# duthub-kernel-diagnosis

- Resolve DUT ID, session or archive segment, and exact deployed kernel version. Record timestamps or offsets and consult raw bytes when line boundaries or encoding matter.
- Distinguish boot metadata and command echo from runtime evidence. Match symbols against the deployed tree and configuration before proposing a fix.
- Report observations, competing hypotheses and the least invasive check. Use the [MCP contract](../../../docs/API_CONTRACT.md) for log pagination. Hardware changes require separate authorization.
