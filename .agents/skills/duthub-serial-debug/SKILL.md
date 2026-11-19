---
name: duthub-serial-debug
description: Diagnose COM and tty discovery, port contention, reconnect, serial input and ANSI rendering in Duthub.
---

# duthub-serial-debug

- Record device path, DUT ID, baud and port owner. Enumerate real devices before diagnosing disappearance. State whether checks used hardware or fixtures.
- First-open errors should surface once. Backoff is for reconnecting established sessions. Check the emitted events before retrying a busy port.
- Distinguish transport bytes from GPUI rendering. Check focus on open, alternate screen and synchronized ANSI output. Terminal Reset resets the emulator. Hardware reset requires separate authorization.
