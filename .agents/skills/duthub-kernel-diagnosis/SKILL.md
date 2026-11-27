---
name: duthub-kernel-diagnosis
description: Diagnose embedded Linux boot failures, kernel panics, and hangs using duthub evidence and the matching kernel tree.
metadata:
  internal: true
---

# duthub-kernel-diagnosis

- Identify the DUT ID, session or archive segment, and deployed kernel version. Preserve timestamps/offsets and consult [raw record semantics](../../../docs/reference/REC_LOG_FORMAT.md) when byte/line boundaries matter.
- Match symbols and addresses against the deployed kernel tree/config, separating boot metadata, command echo, and runtime kernel logs.
- Use [MCP evidence pagination](../../../docs/API_CONTRACT.md) to inspect logs cleanly. Report verified observations, ranked hypotheses, and the least invasive next check; require explicit authorization before any serial write, reset, power, or flash action.
