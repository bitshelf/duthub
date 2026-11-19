---
name: duthub-ui-design
description: Edit and verify Duthub native GPUI forms, tabs, terminal, Agent panels, dialogs and capture views.
---

# duthub-ui-design

- Use the repository pin of gpui-kit and its existing components. Verify hit testing and popup layering when controls overlap title bars or overlays.
- Keep inputs scoped to their DUT or settings draft. Refresh when the edited object changes so typing, selection and IME survive rerender. Preserve terminal and Agent state across panel switches.
- Verify affected native paths, keyboard focus, menu dismissal, narrow layouts, themes and DPI-sensitive splitters. Follow the [regression skill](../duthub-regression-test/SKILL.md).
