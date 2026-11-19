#requires -Version 5.1
<#
.SYNOPSIS
    Automated acceptance checks for "close hides to the notification area"
    (`docs/design/TRAY_ALERT_DESIGN.md` §10.3).

.DESCRIPTION
    Runs the desktop binary in a throw-away work directory and verifies the
    parts of §10.3 that need no human:

      1. The window opens (MainWindowHandle != 0) and the process stays alive.
      2. WM_CLOSE hides the window while the process keeps running.
      3. The 50 ms service tick is still alive while hidden: the MCP snapshot's
         `ts_unix_ms` keeps increasing.
      4. The hidden window can be brought back (ShowWindow SW_SHOW) and is
         visible again.
      5. A second instance of the same executable refuses to open a window and
         exits, leaving the first one running.

    Everything else in §10.3 is interactive and printed as a manual checklist:
    the balloon/dialog after `notify_user`, "quit" from the tray context menu,
    the float-window close paths, and the "still maximized after restore" check.

    Only processes this script started are stopped (never a foreign duthub.exe).

.PARAMETER Exe
    Desktop binary. Defaults to target/release/duthub.exe, then target/debug.

.PARAMETER WorkDir
    Work directory passed via --workdir. Defaults to a fresh temp directory.

.PARAMETER HiddenSeconds
    How long to wait between the two MCP snapshot samples while hidden.

.PARAMETER SkipSecondInstance
    Skip step 5 (it pops a modal message box that the script dismisses).

.PARAMETER McpUrl / McpToken
    Optional MCP endpoint for the §10.3 ④ check (tools/list must answer with 14
    tools while the window is hidden). Skipped when omitted.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/accept-tray.ps1
#>
[CmdletBinding()]
param(
    [string]$Exe = '',
    [string]$WorkDir = '',
    [int]$HiddenSeconds = 10,
    [switch]$SkipSecondInstance,
    [string]$McpUrl = '',
    [string]$McpToken = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Window helpers. Kept minimal: only the three calls the checks need.
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class TrayAcceptWin32
{
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool ShowWindowAsync(IntPtr hWnd, int cmdShow);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hWnd);

    public const uint WM_CLOSE = 0x0010;
    public const uint WM_QUIT_MENU_DISMISS = 0x0010;
    public const int SW_SHOW = 5;
}
'@

$script:Failures = 0
$script:Skips = 0

function Write-Result {
    param([string]$Name, [bool]$Ok, [string]$Detail = '')
    if ($Ok) {
        Write-Host ("PASS  {0}{1}" -f $Name, $(if ($Detail) { "  ($Detail)" } else { '' })) -ForegroundColor Green
    }
    else {
        $script:Failures++
        Write-Host ("FAIL  {0}{1}" -f $Name, $(if ($Detail) { "  ($Detail)" } else { '' })) -ForegroundColor Red
    }
}

function Write-Skip {
    param([string]$Name, [string]$Reason)
    $script:Skips++
    Write-Host ("SKIP  {0}  ({1})" -f $Name, $Reason) -ForegroundColor Yellow
}

function Write-Step {
    param([string]$Text)
    Write-Host ""
    Write-Host ("=== {0}" -f $Text) -ForegroundColor Cyan
}

function Resolve-Exe {
    param([string]$Requested)
    if ($Requested) {
        if (-not (Test-Path -LiteralPath $Requested)) { throw "executable not found: $Requested" }
        return (Resolve-Path -LiteralPath $Requested).Path
    }
    $root = Split-Path -Parent $PSScriptRoot
    foreach ($candidate in @('target\release\duthub.exe', 'target\debug\duthub.exe')) {
        $path = Join-Path $root $candidate
        if (Test-Path -LiteralPath $path) { return $path }
    }
    throw "duthub.exe not found under $root\target (build it first, or pass -Exe)"
}

function Wait-Until {
    param(
        [scriptblock]$Condition,
        [int]$TimeoutSeconds = 20,
        [int]$PollMs = 100
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if (& $Condition) { return $true }
        Start-Sleep -Milliseconds $PollMs
    }
    return $false
}

function Get-MainWindowHandle {
    param([int]$ProcessId)
    $process = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($null -eq $process) { return [IntPtr]::Zero }
    return $process.MainWindowHandle
}

function Get-SnapshotTimestamp {
    param([string]$WorkRoot)
    $snapshot = Join-Path $WorkRoot 'duthub-data\runtime\mcp-state.json'
    if (-not (Test-Path -LiteralPath $snapshot)) { return $null }
    try {
        $json = Get-Content -LiteralPath $snapshot -Raw | ConvertFrom-Json
    }
    catch {
        return $null
    }
    if ($json.PSObject.Properties.Name -contains 'ts_unix_ms') { return [int64]$json.ts_unix_ms }
    return $null
}

function Stop-StartedProcess {
    param([System.Diagnostics.Process]$Process)
    if ($null -eq $Process) { return }
    try {
        if (-not $Process.HasExited) {
            # Only a process this script started is ever stopped (AGENTS.md:
            # never kill a foreign duthub.exe).
            Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
            $Process.WaitForExit(5000) | Out-Null
        }
    }
    catch {
        Write-Host ("WARN  could not stop pid {0}: {1}" -f $Process.Id, $_.Exception.Message) -ForegroundColor Yellow
    }
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

$exePath = Resolve-Exe -Requested $Exe
$workRoot = if ($WorkDir) { $WorkDir } else { Join-Path ([System.IO.Path]::GetTempPath()) ("duthub-accept-tray-" + [guid]::NewGuid().ToString('N')) }
New-Item -ItemType Directory -Path $workRoot -Force | Out-Null

Write-Host ("exe     : {0}" -f $exePath)
Write-Host ("workdir : {0}" -f $workRoot)

$app = $null
try {
    # -----------------------------------------------------------------------
    # 1. Window opens, process alive
    # -----------------------------------------------------------------------
    Write-Step '1. start and wait for the main window'
    $app = Start-Process -FilePath $exePath -ArgumentList @('--workdir', $workRoot) -PassThru
    $opened = Wait-Until -Condition { (Get-MainWindowHandle -ProcessId $app.Id) -ne [IntPtr]::Zero } -TimeoutSeconds 60
    $hwnd = Get-MainWindowHandle -ProcessId $app.Id
    Write-Result -Name 'main window opens' -Ok ($opened -and -not $app.HasExited) -Detail ("pid={0} hwnd={1}" -f $app.Id, $hwnd)

    if (-not $opened) {
        throw 'the window never appeared; nothing else can be checked'
    }

    # -----------------------------------------------------------------------
    # 2. WM_CLOSE hides instead of exiting
    # -----------------------------------------------------------------------
    Write-Step '2. WM_CLOSE must hide, not exit'
    [void][TrayAcceptWin32]::PostMessage($hwnd, [TrayAcceptWin32]::WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero)
    $hidden = Wait-Until -Condition {
        (-not $app.HasExited) -and ((Get-MainWindowHandle -ProcessId $app.Id) -eq [IntPtr]::Zero)
    } -TimeoutSeconds 10
    Write-Result -Name 'close hides and keeps the process alive' -Ok ($hidden -and -not $app.HasExited) -Detail 'MainWindowHandle == 0'

    if ($hidden) {
        # -------------------------------------------------------------------
        # 3. The service tick still runs while hidden
        # -------------------------------------------------------------------
        Write-Step ("3. hidden service tick (snapshot ts_unix_ms, {0}s apart)" -f $HiddenSeconds)
        $first = Get-SnapshotTimestamp -WorkRoot $workRoot
        Start-Sleep -Seconds $HiddenSeconds
        $second = Get-SnapshotTimestamp -WorkRoot $workRoot
        if ($null -eq $first -or $null -eq $second) {
            Write-Skip -Name 'snapshot timestamp increases while hidden' -Reason "no ts_unix_ms in $workRoot\duthub-data\runtime\mcp-state.json"
        }
        else {
            Write-Result -Name 'snapshot timestamp increases while hidden' -Ok ($second -gt $first) -Detail ("{0} -> {1}" -f $first, $second)
        }

        # -------------------------------------------------------------------
        # 4. Optional: MCP tools/list while hidden (§10.3 ④)
        # -------------------------------------------------------------------
        Write-Step '4. MCP tools/list while hidden'
        if (-not $McpUrl) {
            Write-Skip -Name 'tools/list answers with 14 tools while hidden' -Reason 'pass -McpUrl and -McpToken'
        }
        else {
            $headers = @{}
            if ($McpToken) { $headers['Authorization'] = "Bearer $McpToken" }
            $body = '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
            try {
                $response = Invoke-WebRequest -Uri $McpUrl -Method Post -Headers $headers -ContentType 'application/json' -Body $body -UseBasicParsing -TimeoutSec 15
                $payload = $response.Content | ConvertFrom-Json
                $toolCount = @($payload.result.tools).Count
                Write-Result -Name 'tools/list answers while hidden' -Ok ($response.StatusCode -eq 200 -and $toolCount -eq 14) -Detail ("status={0} tools={1}" -f $response.StatusCode, $toolCount)
            }
            catch {
                Write-Result -Name 'tools/list answers while hidden' -Ok $false -Detail $_.Exception.Message
            }
        }

        # -------------------------------------------------------------------
        # 5. Restore
        # -------------------------------------------------------------------
        Write-Step '5. restore the hidden window'
        [void][TrayAcceptWin32]::ShowWindowAsync($hwnd, [TrayAcceptWin32]::SW_SHOW)
        $restored = Wait-Until -Condition { (Get-MainWindowHandle -ProcessId $app.Id) -ne [IntPtr]::Zero } -TimeoutSeconds 10
        Write-Result -Name 'restored window becomes visible again' -Ok $restored
    }

    # -----------------------------------------------------------------------
    # 6. Second instance refuses to start (§2.3)
    # -----------------------------------------------------------------------
    Write-Step '6. second instance exits, first one survives'
    if ($SkipSecondInstance) {
        Write-Skip -Name 'second instance refuses to start' -Reason '-SkipSecondInstance'
    }
    else {
        $second = Start-Process -FilePath $exePath -ArgumentList @('--workdir', $workRoot) -PassThru
        $exited = Wait-Until -Condition { $second.HasExited } -TimeoutSeconds 20
        if (-not $exited) {
            # The "already running" message box is modal; dismiss it by closing
            # the process' own dialog window, then check it exits.
            [void]$second.Refresh()
            $dialog = $second.MainWindowHandle
            if ($dialog -ne [IntPtr]::Zero) {
                [void][TrayAcceptWin32]::PostMessage($dialog, [TrayAcceptWin32]::WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero)
            }
            $exited = Wait-Until -Condition { $second.HasExited } -TimeoutSeconds 10
        }
        Write-Result -Name 'second instance exits' -Ok $exited -Detail ("exit code={0}" -f $(if ($exited) { $second.ExitCode } else { 'still running' }))
        Write-Result -Name 'first instance survives' -Ok (-not $app.HasExited)
        Stop-StartedProcess -Process $second
    }
}
catch {
    Write-Host ("ERROR " + $_.Exception.Message) -ForegroundColor Red
    $script:Failures++
}
finally {
    Stop-StartedProcess -Process $app
}

# ---------------------------------------------------------------------------
# Manual checklist (§10.3 items that need a human)
# ---------------------------------------------------------------------------

Write-Host ''
Write-Host 'Manual checks (need a person at the desktop):' -ForegroundColor Cyan
Write-Host '  * notify_user while hidden shows a balloon; after restore the in-app dialog appears.'
Write-Host '  * tray context menu has exactly "show main window" and "quit"; quit exits within 5 s.'
Write-Host '  * hide with a maximized window, restore: still maximized.'
Write-Host '  * float window open -> close -> hidden -> restore: float button still works.'
Write-Host '  * float window open -> quit from the tray menu: process exits, no orphan window.'
Write-Host '  * hold-test a relay channel, then close: the log shows the OFF mark and STATUS reads released.'
Write-Host ("workdir kept for inspection: {0}" -f $workRoot)

Write-Host ''
Write-Host ("Summary: {0} failure(s), {1} skipped" -f $script:Failures, $script:Skips) -ForegroundColor $(if ($script:Failures -eq 0) { 'Green' } else { 'Red' })
exit $(if ($script:Failures -eq 0) { 0 } else { 1 })
