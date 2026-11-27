# Move to window-relative coordinates and send wheel events of 120 units per notch.
# Usage: powershell -File scripts/ui-wheel.ps1 [-X 700] [-Y 600] [-Down 3] [-Up 3] [-SleepMs 800]
param(
    [int]$X = 700, [int]$Y = 600,
    [int]$Down = 0, [int]$Up = 0,
    [string]$Out = "target/ui-verify/wheel.png",
    [int]$SleepMs = 800
)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WheelDrv {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, System.Text.StringBuilder text, int count);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int hh, uint flags);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, int data, UIntPtr extra);
    public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
$target = [IntPtr]::Zero
$cb = {
    param($hWnd, $lParam)
    if ([WheelDrv]::IsWindowVisible($hWnd)) {
        $sb = New-Object System.Text.StringBuilder 256
        [WheelDrv]::GetWindowText($hWnd, $sb, 256) | Out-Null
        if ($sb.ToString() -eq "duthub" -or $sb.ToString() -eq "") {
            $procId = 0
            [WheelDrv]::GetWindowThreadProcessId($hWnd, [ref]$procId) | Out-Null
            $proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
            if ($proc -and ($proc.ProcessName -eq "duthub-GUI" -or $proc.ProcessName -eq "duthub" -or $proc.ProcessName -eq "duthub-desktop")) { $script:target = $hWnd; return $false }
        }
        if ($sb.ToString() -eq "duthub") { $script:target = $hWnd; return $false }
    }
    return $true
}
[WheelDrv]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
if ($target -eq [IntPtr]::Zero) { Write-Output "WINDOW NOT FOUND"; exit 1 }
[WheelDrv]::ShowWindow($target, 9) | Out-Null
[WheelDrv]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
[WheelDrv]::SetForegroundWindow($target) | Out-Null
[WheelDrv]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 150
[WheelDrv]::SetWindowPos($target, [IntPtr](-2), 0,0,0,0, 0x0003) | Out-Null
[WheelDrv]::SetForegroundWindow($target) | Out-Null
Start-Sleep -Milliseconds 250
$rect = New-Object WheelDrv+RECT
[WheelDrv]::GetWindowRect($target, [ref]$rect) | Out-Null
$sx = $rect.Left + $X; $sy = $rect.Top + $Y
[WheelDrv]::SetCursorPos($sx, $sy) | Out-Null
Start-Sleep -Milliseconds 150
for ($i = 0; $i -lt $Down; $i++) {
    [WheelDrv]::mouse_event(0x0800, 0, 0, -120, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 80
}
for ($i = 0; $i -lt $Up; $i++) {
    [WheelDrv]::mouse_event(0x0800, 0, 0, 120, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 80
}
Start-Sleep -Milliseconds $SleepMs
$bmp = New-Object System.Drawing.Bitmap ($rect.Right - $rect.Left), ($rect.Bottom - $rect.Top)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[WheelDrv]::PrintWindow($target, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc); $g.Dispose()
$full = Join-Path (Split-Path $PSScriptRoot -Parent) $Out
$dir = Split-Path $full -Parent
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "SAVED $full"
