---
name: duthub-serial-debug
description: Diagnose duthub COM or tty enumeration, port contention, reconnect, serial input, garble detection, and ANSI rendering.
metadata:
  internal: true
---

# duthub-serial-debug

- Record device path, stable DUT ID, baud, and port owner. Confirm live enumeration and distinguish initial open failure (reported once, no reconnect loop) from mid-session disconnect (backoff reconnect).
- Keep terminal snapshots, garble telemetry (`GarbleMeter`), and raw RX/TX audit logs (`SessionLogWriter`) separate; verify ordered raw bytes, focus ownership, alternate screen, and synchronized ANSI output.
- Confirm terminal Reset clears only emulator state without breaking logging or leases. Use [raw record format](../../../docs/reference/REC_LOG_FORMAT.md) and distinguish fixture results from real hardware checks.
