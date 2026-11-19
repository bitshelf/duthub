---
name: duthub-ui-design
description: Edit and verify duthub native GPUI forms, tabs, terminal, Agent panels, dialogs and capture views.
---

# duthub-ui-design

- Use the pinned gpui-kit components. Verify hit testing around overlays and title bars. Use [UI specifications](../../../docs/ui-spec/) as a reference and check current GPUI entry points.
- Scope edit state to the active DUT or settings draft. Preserve selection, IME and undo during rerender. Restore terminal focus and Agent state when changing panels.
- Verify the affected keyboard path, narrow layout, theme and DPI behavior. Use the [regression skill](../duthub-regression-test/SKILL.md) for native acceptance.
