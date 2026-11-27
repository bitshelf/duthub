---
name: duthub-ui-design
description: Edit and verify duthub native GPUI forms, tabs, terminal, Agent panels, dialogs, and capture views.
metadata:
  internal: true
---

# duthub-ui-design

- Build with pinned `gpui-kit` components and localize all user-facing text through `crate::i18n::label`/`message` (keeping `zh-CN.json` and `en-US.json` synchronized). Reference [UI specifications](../../../docs/ui-spec/) for layout contracts.
- Scope input and draft state to the active stable DUT ID or settings target; preserve selection, IME, and undo across frames, and verify active DUT/generation before applying async results. Never stack alert and form dialogs.
- Check hit testing around overlays/title bars, keyboard navigation, narrow layouts, theme contrast, and DPI scaling. Validate rendering changes via [regression testing](../duthub-regression-test/SKILL.md) with `--desktop-self-test`.
