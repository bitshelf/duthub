# 驱动 duthub 窗口交互：激活 → 可选点击/输入 → 截图
# 用法: powershell -File scripts/drive-window.ps1 [-ClickX 65 -ClickY 55] [-Type "text"] [-Key "Enter"] [-Double] [-Out target\x.png] [-SleepMs 800]
param(
    [switch]$Topmost,
    [int]$ClickX = -1, [int]$ClickY = -1,
    [string]$Type = "", [string]$Key = "",
    [switch]$Double, [switch]$Right,
    [string]$Out = "target\ui-step.png",
    [int]$SleepMs = 700,
    [int]$Wheel = 0
)

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
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
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int hh, uint flags);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] public static extern bool SetFocus(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern IntPtr GetFocus();
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
            # 空标题窗口可能是我们的（按进程过滤）
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
[WinDrv]::ShowWindow($target, 9) | Out-Null
# ALT 按下/释放绕过 Windows 前台锁定（SetForegroundWindow 限制）
[WinDrv]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
[WinDrv]::SetForegroundWindow($target) | Out-Null
[WinDrv]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 150
if ($Topmost) { [WinDrv]::SetWindowPos($target, [IntPtr](-1), 0,0,0,0, 0x0003) | Out-Null } else { [WinDrv]::SetWindowPos($target, [IntPtr](-2), 0,0,0,0, 0x0003) | Out-Null }
[WinDrv]::SetForegroundWindow($target) | Out-Null
Start-Sleep -Milliseconds 250

if ($ClickX -ge 0) {
    $rect = New-Object WinDrv+RECT
    [WinDrv]::GetWindowRect($target, [ref]$rect) | Out-Null
    $sx = $rect.Left + $ClickX; $sy = $rect.Top + $ClickY
    [WinDrv]::SetCursorPos($sx, $sy) | Out-Null
    Start-Sleep -Milliseconds 120
    $down = 0x0002; $up = 0x0004
    if ($Right) { $down = 0x0008; $up = 0x0010 }
    $count = if ($Double) { 2 } else { 1 }
    for ($i = 0; $i -lt $count; $i++) {
        [WinDrv]::mouse_event($down, 0, 0, 0, [UIntPtr]::Zero)
        [WinDrv]::mouse_event($up, 0, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 15
    }
    Start-Sleep -Milliseconds $SleepMs
    if ($Wheel -ne 0) {
        # 滚轮：负值向上滑动一行（0x0800=WHEEL）；每次滚动一行直到归零
        $dir = if ($Wheel -gt 0) { [uint32]0 } else { [uint32]0x80000000 }
        $n = [Math]::Abs($Wheel)
        [WinDrv]::SetCursorPos($sx, $sy) | Out-Null
        for ($i = 0; $i -lt $n; $i++) {
            [WinDrv]::mouse_event(0x0800, 0, 0, $dir, [UIntPtr]::Zero)
            Start-Sleep -Milliseconds 60
        }
    }
}
if ($Type -ne "") {
    [System.Windows.Forms.SendKeys]::SendWait($Type)
    Start-Sleep -Milliseconds 300
}
if ($Key -ne "") {
    [System.Windows.Forms.SendKeys]::SendWait($Key)
    Start-Sleep -Milliseconds 300
}

# 截图
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
$rect2 = New-Object WinDrv+RECT
[WinDrv]::GetWindowRect($target, [ref]$rect2) | Out-Null
$w = $rect2.Right - $rect2.Left; $h = $rect2.Bottom - $rect2.Top
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[WinDrv]::PrintWindow($target, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc); $g.Dispose()
$full = Join-Path (Split-Path $PSScriptRoot -Parent) $Out
$dir = Split-Path $full -Parent
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "SAVED $full"
