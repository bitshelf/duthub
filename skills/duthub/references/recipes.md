# Duthub MCP — Copy-Paste Verified Recipes

## 0. Direct LAN HTTP Connectivity Check (Bypassing Client Proxy)

Ensure `NO_PROXY` includes the duthub LAN IP so local proxies never intercept LAN MCP traffic:

```bash
NO_PROXY=<LAN-IP> curl -s -D - -X POST 'http://<LAN-IP>:<PORT>/mcp' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'Authorization: Bearer <TOKEN>' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1"}}}'
```

## 1. Preflight Check

```json
// Tool: dut_list
{}
```
```json
// Tool: dut_status
{"dutId": "<id>"}
```
Verify `dutState == "ready"`, `capabilities`, `bootModes`, `flashPresets`, and `transfer` before issuing write/upload/flash commands.

## 1b. Bulk Export All Archived Logs (Single Request)

Instead of 100+ paginated `log_segment_read` calls, call `log_export`:

```json
// Tool: log_export
{
  "dutId": "<id>",
  "stripAnsi": true
}
```
If the total log size exceeds 256 KiB, download the returned `gzipDownloadUrl` in one HTTP request into **`.duthub/logs/<id>/`** (the standard Agent log directory) and verify `X-Duthub-Sha256`:

```bash
mkdir -p .duthub/logs/<id>
NO_PROXY=<LAN-IP> curl -sSf -D - \
  -H 'Authorization: Bearer <TOKEN>' \
  'http://<LAN-IP>:<PORT>/dut/<id>/logs/export?format=gzip' \
  -o .duthub/logs/<id>/dut-logs.txt.gz
```

## 2. Shell Command with Anti-Echo Sentinel

Splitting the sentinel literal (`__OK_''DONE__`) prevents serial local echo from prematurely matching `expect`:

```json
// Tool: serial_exec
{
  "dutId": "<id>",
  "text": "uname -a; echo __OK_''DONE__\r",
  "expect": "__OK_DONE__",
  "timeoutMs": 8000
}
```

## 3. Command with Quiet-Settle Detection (No Sentinel Needed)

Completes automatically once output stops changing for 800ms:

```json
// Tool: serial_exec
{
  "dutId": "<id>",
  "text": "dmesg | tail -n 30\r",
  "settleMs": 800,
  "timeoutMs": 10000
}
```

## 4. Interrupt a Hung Foreground Process & Inspect Clean Screen

Never hand-code `\x03` in `text`; use validated `keys`:

```json
// Tool: serial_exec
{
  "dutId": "<id>",
  "keys": ["ctrl+c"],
  "settleMs": 400,
  "timeoutMs": 3000
}
```
```json
// Tool: serial_read (clean ANSI-free VT100 screen)
{
  "dutId": "<id>",
  "mode": "screen",
  "tail": 40
}
```

## 5. Cursor-Safe Wait for Boot Banner (Zero Missed Lines)

Pass `sinceSeq` from your previous `serial_read` / `serial_exec` `lastSeq`:

```json
// Tool: serial_read
{
  "dutId": "<id>",
  "sinceSeq": 1240,
  "waitRegex": "(login:|root@.*#)",
  "timeoutMs": 25000
}
```

## 6. Millisecond U-Boot Intercept to Enter Rockchip Loader

When only a `reset` relay is wired (check `dut_status.bootModes`):

```json
// Tool: serial_exec
{
  "dutId": "<id>",
  "triggerReset": true,
  "waitFor": "Hit any key",
  "send": "\r",
  "thenSend": "rockusb 0 mmc 0\r",
  "expect": "rockusb",
  "timeoutMs": 15000
}
```

## 7. Image Upload + Multi-Step Exclusive Lease + Safe Firmware Flash (8 Steps)

1. Check `dut_status` (`capabilities.flash == true`, `flashPresets`, `transfer`) and list existing images:
```json
// Tool: dut_files
{"dutId": "<id>", "category": "image"}
```
2. If the image is not staged yet, upload it to the **same MCP HTTP port** (requires `flash` grant):
   - **Option A — Streaming `PUT` (`curl`, single command, supports 5GB+ files)**:
     ```bash
     SHA=$(sha256sum update.img | awk '{print $1}')
     NO_PROXY=<LAN-IP> curl -fS -T update.img \
       -H 'Authorization: Bearer <TOKEN>' \
       -H "X-Content-SHA256: $SHA" \
       'http://<LAN-IP>:<PORT>/dut/<id>/image/update.img'
     ```
     *(Append `?overwrite=true` to intentionally replace an existing image of the same name.)*
   - **Option B — Resumable high-speed tus 1.0.0 (`ztus`, HTTP/2 `h2c` with large tuned windows & automatic resume — recommended for multi-GB images)**:
     ```bash
     mkdir -p .duthub/logs/<id>
     NO_PROXY=<LAN-IP> ztus upload update.img \
       'http://<LAN-IP>:<PORT>/dut/<id>/transfer' \
       --header 'Authorization: Bearer <TOKEN>' \
       --checksum sha256 2>&1 | tee .duthub/logs/<id>/upload.log
     ```
   *(You may upload a **different** image filename concurrently while another image is flashing; uploading the same `activeFlashFile` is rejected with `409 FILE_IN_USE`.)*
3. Acquire exclusive lease with mandatory `sessionName`:
```json
// Tool: dut_control
{
  "dutId": "<id>",
  "action": "acquire_lease",
  "sessionName": "rk3576-flash-verify",
  "waitMs": 15000
}
```
4. Enter Loader (`adb_exec(op="reboot_loader")`, `enter_loader`, or U-Boot recipe), confirm `inLoader == true`, then flash (set HTTP client timeout ≥ 300s):
```json
// Tool: dut_control
{"dutId": "<id>", "action": "flash_list", "leaseId": "<leaseId>"}
```
```json
// Tool: dut_control
{"dutId": "<id>", "action": "flash_update", "fileId": "image/update.img", "leaseId": "<leaseId>"}
```
5. Reboot into the flashed image and verify boot via `serial_read(sinceSeq, waitRegex)` / `adb_exec(op="shell", command="uname -a")`:
```json
// Tool: dut_control
{"dutId": "<id>", "action": "flash_reboot", "leaseId": "<leaseId>"}
```
6. Always release the lease so waiting Agents proceed immediately:
```json
// Tool: dut_control
{"dutId": "<id>", "action": "release_lease", "leaseId": "<leaseId>"}
```

## 8. ADB Debugging & Scoped File Pull

```json
// Tool: adb_exec (shell)
{"dutId": "<id>", "op": "shell", "command": "uname -a && getprop ro.build.display.id"}
```
```json
// Tool: adb_exec (pull into DUT folder, then read via returned resourceUri)
{"dutId": "<id>", "op": "pull", "remotePath": "/proc/version", "fileId": "version.txt"}
```

## 9. HDMI Snapshot & Recording

```json
// Tool: media_streams (snapshot -> read artifact.resourceUri)
{"dutId": "<id>", "op": {"kind": "snapshot"}}
```
```json
// Tool: media_streams (recordStart -> note recording.recordingId -> recordStop)
{"dutId": "<id>", "op": {"kind": "recordStart"}}
```
```json
// Tool: media_streams
{"dutId": "<id>", "op": {"kind": "recordStop", "recordingId": 1}}
```

## 10. Desktop Notification & Structured Agent Feedback

```json
// Tool: notify_user
{
  "dutId": "<id>",
  "title": "Flash verified",
  "message": "New image booted and passed smoke checks.",
  "level": "info"
}
```

Before calling `agent_feedback`, check `dut_files(dutId="<id>", category="feedback")` to avoid duplicates, submit **one issue per entry**, follow the 4-part structure below, and **never include Bearer tokens or host paths**:

```json
// Tool: agent_feedback
{
  "dutId": "<id>",
  "category": "usability",
  "severity": "medium",
  "title": "Concise single-issue summary (<=120 chars)",
  "detail": "1. Goal & preflight: dutState=ready, capabilities.flash=true.\n2. Exact call & error: dut_control(action=flash_update, fileId=...) returned IMAGE_NOT_FOUND.\n3. Impact: Blocked automated flash step.\n4. Suggestion: Keep available image list and upload hint in the error response.",
  "toolName": "dut_control",
  "sessionName": "rk3576-flash-verify"
}
```
