# Duthub MCP — Complete Parameters, `dutState` & Error Recovery Matrix

## 1. Key Tool Parameter Schemas (`camelCase`)

### `serial_read` (Read-Only, Plan-Allowed; ANSI Color/Highlight Codes Stripped)
| Field | Type | Notes |
|---|---|---|
| `dutId` | `string` (required) | Target DUT ID. |
| `mode` | `"stream" \| "screen"` | Default `"stream"` (plain ANSI-free incremental chunks). `"screen"` returns the rendered VT100 terminal screen (and `resourceUri`) with progress-bar overwrites resolved. |
| `sinceSeq` | `number` | Cursor from previous `last_seq` / `lastSeq`. Preserves zero-gap continuity when combined with `waitFor` / `waitRegex`. |
| `limit` | `number` | Max chunks returned (`1..=400`, default `80`). |
| `tail` | `number` | Return newest `1..=400` chunks (or last `tail` lines in `mode="screen"`). Ignores `sinceSeq`. |
| `waitFor` | `string` | Read-only wait until live output (raw or ANSI-stripped) contains this substring. |
| `waitRegex` | `string` | Read-only wait until bounded regex matches (≤256 chars, size-limited against ReDoS). |
| `timeoutMs` | `number` | Wait budget when `waitFor` or `waitRegex` is set (`100..=30000`, default `5000`). |

Response additions in wait/screen modes: `mode`, `matched`, `matchedBy` (`"waitFor" | "waitRegex" | "timeout"`), `elapsedMs`, and `screen` (always in `"screen"` mode or on wait timeout).

### `serial_exec` (`serialWrite`, plus `reset` when `triggerReset=true`)
| Field | Type | Notes |
|---|---|---|
| `dutId` | `string` (required) | Target DUT ID. |
| `text` | `string` | Text/command to send immediately (≤4096 bytes). |
| `keys` | `string[]` | Validated logical terminal keys sent as raw bytes (≤64 keys). Allowed: `enter`/`return`/`cr`, `tab`, `space`, `esc`/`escape`, `backspace`/`bs`, `up`, `down`, `left`, `right`, `home`, `end`, `ctrl+a`..`ctrl+z`. |
| `triggerReset` | `boolean` | Bookmarks serial cursor before triggering relay `reset` so fast boot banners are never lost. |
| `waitFor` | `string` | Substring to wait for before sending `send` / `keys` (e.g. `"Hit any key"`). |
| `send` | `string` | Payload sent after `waitFor` matches (defaults to `"\r"` if `keys` not set). |
| `thenSend` | `string` | Follow-up command sent ~250ms after `send`. |
| `expect` | `string` | Literal completion marker to wait for after sending. |
| `expectRegex` | `string` | Bounded regex completion marker (≤256 chars). |
| `settleMs` | `number` | Quiet-settle window (`200..=5000` ms): completes once output arrives and stays silent for `settleMs`. |
| `stallTimeoutMs` | `number` | Zero-byte stall detection threshold (`200..=30000` ms, default `3000`). |
| `timeoutMs` | `number` | Overall wait budget (`200..=30000` ms, default `5000`). |
| `markLabel` | `string` | Writes `[mark] <label>` log boundary (can be called alone with `read` permission). |
| `requestId` | `string` | Idempotency key (`1..=128` printable ASCII chars). |
| `leaseId` / `sessionName` / `waitMs` / `notifyOnBlocked` | lease controls | Reuse an acquired lease or wait in FIFO queue (`waitMs` default `5000`, max `60000`). |

Response diagnostics: `matchedBy` (`"expect" | "expectRegex" | "settled" | "immediate" | "timeout"`), `bytesObserved`, `stalled`, `keysSent` (when `keys` used), `screenTail` (last 30 rendered lines whenever `matchedBy == "timeout"` or `stalled == true`), plus `output` and `lastSeq`.

### `log_export` (Read-Only, Plan-Allowed)
| Field | Type | Notes |
|---|---|---|
| `dutId` | `string` (required) | Target DUT ID. |
| `sinceSegmentId` | `string` | Optional exclusive lower bound: export segments newer than this segment ID. |
| `tailSegments` | `number` | Optional count keeping only the newest `N` segments. |
| `stripAnsi` | `boolean` | Default `true` (strips ANSI color/highlight escape codes). |
| `inline` | `boolean` | Return `inlineText` directly when payload ≤ 256 KiB (default `true` for small exports). |

Response: `segmentCount`, `totalRawBytes`, `exportBytes`, `sha256`, `snapshotAtUnixMs`, `downloadUrl`, `gzipDownloadUrl`, and optional `inlineText`. For large multi-MB logs, download `downloadUrl` or `gzipDownloadUrl` in one HTTP `GET` with `Authorization: Bearer <TOKEN>`.

### `dut_control` (`power` / `reset` / `relay` / `flash`)
- `action`: `power_on` | `power_off` | `power_cycle` | `reset` | `power_key_short` | `power_key_long` | `enter_maskrom` | `enter_loader` | `relay_test` | `flash_list` | `flash_update` | `flash_reboot` | `acquire_lease` | `renew_lease` | `release_lease`.
- **`acquire_lease` requires `sessionName`** (non-empty, ≤64 chars, no control characters). Records `acquiredAtUnixMs` so other Agents see `sessionName` and `occupiedSecs` in `dut_list` / `dut_status`.
- `flash_update`: pass `fileId` from `dut_files(category="image")` (`"image/update.img"` or plain `"update.img"`). Optional `presetId` comes from `dut_status.flashPresets`. Large 1.5–5.5 GB images take 45–240s — set MCP HTTP client timeout ≥ 300s.
- Missing image returns `IMAGE_NOT_FOUND` (with available images and upload endpoint hint) without entering `Protected` or `NeedsInspection`.

### HTTP Binary Transfer & Bulk Export Endpoints (Same MCP Port)
- **Streaming `PUT`**: `PUT /dut/{dutId}/image/{filename}` (`?overwrite=true` optional, `X-Content-SHA256: <64-hex>` optional). Requires `DutAction::Flash` grant; accepts `.img`, `.bin`, `.iso` up to 30 GiB; returns `201 Created` with `fileId` and `sha256`.
- **Resumable tus 1.0.0 (`ztus` / `curl`)**: `OPTIONS|POST /dut/{dutId}/transfer` and `HEAD|PATCH|DELETE /dut/{dutId}/transfer/{uploadId}`. Requires `Tus-Resumable: 1.0.0`, `DutAction::Flash` grant, `Upload-Metadata: filename <base64>`, optional `Upload-Checksum: sha256 <hex|base64>`.
- **Bulk log download**: `GET /dut/{dutId}/logs/export` (`?sinceSegmentId=&tailSegments=&stripAnsi=true&format=text|gzip`). Requires `DutAction::Read` grant; returns `X-Duthub-Segments`, `X-Duthub-Total-Bytes`, `X-Duthub-Sha256`.

### `dut_files` Six Standard Categories
- `image` — firmware images (`image/*`, legacy `images/data/*`, and root `.img`/`.bin`/`.iso`)
- `TEST` — test artifacts and scripts
- `压力&老化` — stress and burn-in logs/outputs
- `串口日志` — segmented/continuous serial logs (`.log`, `.bin`, `.rec`)
- `misc` — miscellaneous captures, ADB pulls, and other files
- `feedback` — structured Markdown feedback recorded via `agent_feedback`

---

## 2. Error Code Recovery Matrix

| Error Code / Prefix | Cause | Deterministic Recovery |
|---|---|---|
| `SESSION_NOT_OPEN` | Serial port is not open in `duthub.exe` | Read history with `log_segments`, or call `notify_user` asking operator to open the port. |
| `DUT_BUSY` (`holder` = GUI) | Desktop user actively typed (finite TTL) or explicitly took over control (auto-notifies desktop when `notifyOnBlocked != false`) | **Do not retry-loop.** Read-only tools still work. If `remaining_secs` is set, wait briefly or pass `waitMs`; if held indefinitely, call `notify_user` to ask the operator to release control (or launch `duthub.exe --release-gui-lease`). |
| `DUT_BUSY` (`holder` = Agent) | Another Agent holds lease (`sessionName`, `occupiedSecs`, `queueLen` returned in error `data`) | Increase `waitMs` (up to `60000`, or `120000` for `acquire_lease`) to queue in FIFO order; if `occupiedSecs` looks stuck, call `notify_user`. |
| `SESSION_NAME_REQUIRED` | `acquire_lease` called without valid `sessionName` | Retry with a concise descriptive `sessionName` (e.g. `"ci-flash-rk3576"`). |
| `INVALID_KEY` | Unknown logical key in `serial_exec.keys` (zero bytes sent) | Fix the key name to the allowed list (`ctrl+c`, `esc`, `enter`, `up`, etc.) and retry. |
| `INVALID_REGEX` | Malformed or oversized regex in `waitRegex` / `expectRegex` | Simplify pattern (≤256 chars) and retry. |
| `DUT_PROTECTED` | Active protected operation (e.g. `flash_update`) | Wait and poll `dut_status` until `dutState == "ready"`. Note: uploading a **different** image filename is allowed concurrently while flashing. |
| `FILE_IN_USE` (`409`) | Attempted to upload/overwrite the exact image file currently being flashed (`activeFlashFile`) | Upload under a different filename to run concurrently, or wait until `flash_update` completes. |
| `UPLOAD_IN_PROGRESS` (`409` / MCP) | Same `(dutId, filename)` already has an active upload, or `flash_update` targeted an image still being uploaded | Wait for the active upload to finish before flashing or re-uploading that filename (different filenames can run concurrently). |
| `MANUAL_CONTROL` | Human Manual Takeover is enabled | Stop all write/control calls; wait until operator turns off Manual Takeover. |
| `NEEDS_INSPECTION` | Hardware fault locked the DUT | Call `notify_user` summarizing the failure and wait for operator clearance. |
| `INVALID_FILE_ID` / `IMAGE_NOT_FOUND` | Bad image filename/path or image not staged inside the DUT folder | Pass a plain filename or `image/<name>` (`.img`/`.bin`/`.iso`) from `dut_files(category="image")`; if absent, upload via `PUT /dut/{dutId}/image/{filename}` (`curl -T`) or tus 1.0.0 `/dut/{dutId}/transfer` (`ztus`). |
| `PLAN_MODE_ONLY` / `UNAUTHORIZED` | Credential lacks required DUT/action grant | Do not brute-force retries; inspect `dut_status.capabilities`, report the missing permission (`serialWrite`/`flash`/`adb`/etc.), or ask the administrator to grant it. |
| `REQUEST_ID_CONFLICT` | Same `requestId` reused with a different payload | Generate a fresh `requestId` for the new payload. |

---

## 3. HTTP Transport Troubleshooting

| HTTP Status | Typical Cause | Resolution |
|---|---|---|
| `401 Unauthorized` | Missing/invalid/disabled Bearer token, wrong listener port, or `agentAccess.enabled == false` | Verify `Authorization: Bearer <TOKEN>`, listener port, and that Agent Access is enabled in `duthub.exe`. |
| `403 Forbidden: Host header is not allowed` | Request `Host` is not loopback, detected host LAN IP/hostname (when `lanAccess` is enabled), or listed in `agentAccess.allowedHosts` | Enable `lanAccess` for LAN IPs, or add custom hostnames/reverse-proxy authorities to `agentAccess.allowedHosts` in `.duthub/settings.json` and restart `duthub.exe`. |
| `403 Forbidden: Origin header is not allowed` | Client sent a browser `Origin` header | Omit `Origin` from CLI/agent HTTP requests (duthub rejects browser cross-origin requests by design). |
| `403 Forbidden (UNAUTHORIZED)` on upload/export | Valid token lacks `flash` (for upload) or `read` (for log export), or targets a different DUT on a DUT-scoped listener | Check `dut_status.capabilities`; ask the operator to grant the required permission instead of retrying. |
| `409 Conflict` on upload | Image already exists without `?overwrite=true`, tus `Upload-Offset` mismatch, or DUT is in `manual_takeover`/`flashing`/`needs_inspection` | For `PUT`, use a new filename or `?overwrite=true`; for tus, query `HEAD` for the server's `Upload-Offset`; for DUT lock states, wait or call `notify_user`. |
| `422` / `460` on upload | Uploaded bytes did not match declared `X-Content-SHA256` / `Upload-Checksum` | Re-compute SHA-256 and re-upload cleanly (the corrupt temp file was discarded automatically). |
| `429 Too Many Requests` | Repeated failed Bearer attempts from this source IP | Stop probing; wait for `Retry-After` (60s) and fix the token before retrying. |
| `502 Bad Gateway` (with `Proxy-Connection`) | Client-side HTTP proxy intercepted the LAN request | Add the duthub LAN IP to `NO_PROXY`. duthub itself never emits `Proxy-Connection`. |
