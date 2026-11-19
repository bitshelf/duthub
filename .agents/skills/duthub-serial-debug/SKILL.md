---
name: duthub-serial-debug
description: Diagnose duthub COM or tty enumeration, port contention, reconnect, serial input and ANSI rendering.
---

# duthub-serial-debug

- Record device path, DUT ID, baud and port owner. Confirm real enumeration and distinguish failed first open from reconnect of a previously working session.
- Separate arrival of serial bytes, emulator frames and GUI rendering. Check focus, alternate screen and synchronized ANSI output with fixtures or authorized hardware.
- Terminal Reset resets the emulator. Use [serial evidence format](../../../docs/reference/REC_LOG_FORMAT.md) and report which checks used real hardware.
