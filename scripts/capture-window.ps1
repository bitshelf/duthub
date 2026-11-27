# Capture the duthub window as a PNG for UI inspection.
# Usage: powershell -File scripts/capture-window.ps1 [output path]
param([string]$OutPath = "target\ui-check.png")

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinDrv {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, System.Text.StringBuilder text, int count);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

$target = [IntPtr]::Zero
$cb = {
    param($hWnd, $lParam)
    if ([WinDrv]::IsWindowVisible($hWnd)) {
        $sb = New-Object System.Text.StringBuilder 256
        [WinDrv]::GetWindowText($hWnd, $sb, 256) | Out-Null
        if ($sb.ToString() -eq "duthub" -or $sb.ToString() -eq "") {
            # An untitled window can still match the process filter.
            $procId = 0
            [WinDrv]::GetWindowThreadProcessId($hWnd, [ref]$procId) | Out-Null
            $proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
            if ($proc -and ($proc.ProcessName -eq "duthub-GUI" -or $proc.ProcessName -eq "duthub" -or $proc.ProcessName -eq "duthub-desktop")) { $script:target = $hWnd; return $false }
        }
        if ($sb.ToString() -eq "duthub") { $script:target = $hWnd; return $false }
    }
    return $true
}
[WinDrv]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
if ($target -eq [IntPtr]::Zero) { Write-Output "WINDOW NOT FOUND"; exit 1 }

# Restore the window if minimized and bring it to the foreground.
[WinDrv]::ShowWindow($target, 9) | Out-Null   # SW_RESTORE
Start-Sleep -Milliseconds 400
[WinDrv]::SetForegroundWindow($target) | Out-Null
Start-Sleep -Milliseconds 600

$rect = New-Object WinDrv+RECT
[WinDrv]::GetWindowRect($target, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left; $h = $rect.Bottom - $rect.Top
if ($w -le 0 -or $h -le 0) { Write-Output "BAD SIZE"; exit 1 }

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
# GPUI and GPU-composited windows require PW_RENDERFULLCONTENT=2.
[WinDrv]::PrintWindow($target, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc)
$g.Dispose()
$full = Join-Path (Split-Path $PSScriptRoot -Parent) $OutPath
$dir = Split-Path $full -Parent
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "SAVED $full ($w x $h)"
