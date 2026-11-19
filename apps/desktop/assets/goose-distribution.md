# goose distribution provenance

goose is **not bundled** with duthub and no goose bytes are shipped in `duthub.exe`
or next to the release. This statement is published as
`licenses/goose/PROVENANCE.md`; `scripts/release-gate.ps1` asserts that the shipped
copy is byte-identical to this file and that the facts below still match the code.

## Source and license

- Project: AAIF/goose (formerly block/goose), <https://github.com/aaif-goose/goose>
- Download URL (rolling "latest" release asset, `duthub-tooldown` `TOOL_GOOSE`):
  <https://github.com/aaif-goose/goose/releases/latest/download/goose-x86_64-pc-windows-msvc.zip>
- Archive member installed: `goose.exe` (installed to `<config dir>/tools/goose/`)
- License: Apache-2.0 (full text and any upstream NOTICE ship with the upstream
  release package, which is the copy installed on the user's machine)
- Copyright: the AAIF/goose contributors

duthub does not modify, patch, re-sign or redistribute goose. The program runs as a
separate process; duthub does not link it as a Rust library.

## Version gate

Minimum/recommended version: 1.60.2. `agent_host::registry` rejects older downloads
and external programs; an absent goose is reported as `NotInstalled` and the UI
points the user to the tool-downloads page. Nothing is downloaded or updated
automatically.

## Integrity policy

The rolling upstream asset is not pinned, because its bytes change with every
upstream release (a pin would make every update fail with `ChecksumMismatch`).
Instead, the install kernel records the measured sha256 of the downloaded archive
in `<config dir>/tools/goose/manifest.json` and marks it `checksumVerified: false`
— an unverified download, recorded honestly. Before launch, the runtime re-hashes
the installed `goose.exe` against that record; if the bytes no longer match, the
downloaded install is skipped and resolution falls through to `goose` on PATH.

duthub does not verify a signature chain for goose and makes no Authenticode claim
for it (the program is not shipped by duthub, so there is nothing for the release
gate to inspect).

## See also

- `docs/decisions/ADR-3-GOOSE-BUNDLE.md` §9 — distribution contract superseding the
  former embedded-asset terms.
- `NOTICE` — the same facts in the release notice.
- `sbom/goose-audit-status.txt` in each release — goose runtime dependencies are
  **UNVERIFIED** at release time; auditing them is the upstream release's job.
