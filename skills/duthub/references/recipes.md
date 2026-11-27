# Duthub MCP — Copy-Paste Verified Recipes

## 0. Direct LAN HTTP Connectivity Smoke Check (Bypassing Client Proxy)

Always pass `--noproxy '*'` (or set `NO_PROXY`) so local HTTP proxies never intercept LAN MCP traffic:

```bash
curl --noproxy '*' -s -D - -X POST 'http://<LAN-IP>:<PORT>/mcp' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "Authorization: Bearer $DUTHUB_TOKEN" \
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
Verify `dutState == "ready"` before issuing write/control commands.

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

## 7. Multi-Step Exclusive Lease + Safe Firmware Flash

1. Acquire exclusive lease with mandatory `sessionName`:
```json
// Tool: dut_control
{
  "dutId": "<id>",
  "action": "acquire_lease",
  "sessionName": "rk3576-flash-verify",
  "waitMs": 15000
}
```
2. List firmware images under `image` category (pass the returned `id` as `fileId`):
```json
// Tool: dut_files
{"dutId": "<id>", "category": "image"}
```
3. Enter Loader (`enter_loader`, `adb_exec(op="reboot_loader")`, or U-Boot recipe), verify `inLoader == true`, then flash (set HTTP client timeout ≥ 300s):
```json
// Tool: dut_control
{"dutId": "<id>", "action": "flash_list", "leaseId": "<leaseId>"}
```
```json
// Tool: dut_control
{"dutId": "<id>", "action": "flash_update", "fileId": "<id-from-dut_files>", "leaseId": "<leaseId>"}
```
4. Always release the lease so waiting Agents proceed immediately:
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

## 10. Desktop Notification & Agent Usage Feedback

```json
// Tool: notify_user
{
  "dutId": "<id>",
  "title": "Flash verified",
  "message": "New image booted and passed smoke checks.",
  "level": "info"
}
```
```json
// Tool: agent_feedback
{
  "dutId": "<id>",
  "category": "usability",
  "severity": "medium",
  "title": "Short summary of friction or suggestion",
  "detail": "Observed behavior and concrete improvement suggestion.",
  "toolName": "serial_exec",
  "sessionName": "rk3576-flash-verify"
}
```
