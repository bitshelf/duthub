---
name: duthub-device-provisioning
description: Configure and verify duthub DUTs, USB debugging, flash presets, relay boards and capture bindings.
---

# duthub-device-provisioning

- Use the [data model](../../../docs/DATA_MODEL.md) to distinguish global tools and shared relay boards from per-DUT bindings. Verify drafts and saved state by DUT ID.
- Use live COM, ADB/HDC and DirectShow enumeration. Preserve endpoint identity when names repeat and surface ambiguous selection. Check save, cancel and reopen with two DUTs.
- Verify executable-side configuration and the effective workspace independently. Exercise hardware actions only within the authorized device and action scope.
