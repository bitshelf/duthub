---
name: duthub-device-provisioning
description: Configure and verify duthub DUTs, USB debugging, flash presets, relay boards, tool downloads, and capture bindings.
metadata:
  internal: true
---

# duthub-device-provisioning

- Use the [data model](../../../docs/DATA_MODEL.md) to separate global settings, `.duthub/tools/` (`duthub-tooldown`, resolved explicit > downloaded > `PATH`), and shared relay boards from per-DUT bindings keyed by stable DUT ID.
- Use live COM, ADB/HDC, and capture enumeration without conflating saved bindings with live candidates; preserve endpoint identity when names repeat and surface ambiguous selection explicitly.
- Verify save, cancel, and reopen across multiple DUTs, plus `settings.json`/`duts.json` roundtrip invariants. Exercise real serial, relay, power, or flash hardware only within confirmed DUT, port, channel, and user authorization.
