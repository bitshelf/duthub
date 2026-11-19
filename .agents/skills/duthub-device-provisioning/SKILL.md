---
name: duthub-device-provisioning
description: Configure and verify Duthub DUTs, USB debugging, flash presets, relay boards and capture bindings.
---

# duthub-device-provisioning

- Keep global tool paths, relay boards and preset catalogs separate from DUT bindings. Use stable DUT IDs. Test save, cancel, rename, delete, reopen and JSON roundtrips.
- Populate port and ADB/HDC selectors using live enumeration. Keep saved bindings separate from discovery. Flag ambiguous devices. Use physical capture endpoint identity when DirectShow names repeat.
- Verify the effective work directory separately from bootstrap configuration. Check that two DUTs use separate owned folders and that changing one binding does not change the other. See [data model](../../../docs/DATA_MODEL.md).
