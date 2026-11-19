---
name: duthub-kernel-diagnosis
description: Diagnose embedded Linux boot failures, kernel panics and hangs using duthub evidence and the matching kernel tree.
---

# duthub-kernel-diagnosis

- Identify the DUT, session or archive segment, and deployed kernel version. Preserve timestamps or offsets and consult [raw record semantics](../../../docs/reference/REC_LOG_FORMAT.md) when line boundaries matter.
- Match symbols against the deployed tree and configuration. Distinguish boot metadata, command echo and runtime evidence.
- Use the [MCP contract](../../../docs/API_CONTRACT.md) for evidence pagination. Report observations, hypotheses and the least invasive check. Hardware changes need their own authorization.
