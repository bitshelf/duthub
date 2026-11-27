# List every top-level window for a PID, including untitled windows.
param([int]$TargetPid = 0, [int]$Hwnd = 0)
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WE2 {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
$lines = @()
$cb = {
    param($h, $l)
    $p2 = 0
    [WE2]::GetWindowThreadProcessId($h, [ref]$p2) | Out-Null
    if ($TargetPid -gt 0 -and $p2 -ne $TargetPid) { return $true }
    $sb = New-Object System.Text.StringBuilder 256
    [WE2]::GetWindowText($h, $sb, 256) | Out-Null
    $t = $sb.ToString()
    $vis = [WE2]::IsWindowVisible($h)
    $rect = New-Object WE2+RECT
    [WE2]::GetWindowRect($h, [ref]$rect) | Out-Null
    $p = Get-Process -Id $p2 -ErrorAction SilentlyContinue
    if ($vis -or $t -ne '') {
        $script:lines += ('pid={0} proc={1} vis={2} rect=({3},{4})-({5},{6}) title=[{7}] hwnd={8}' -f $p2, $p.ProcessName, $vis, $rect.Left, $rect.Top, $rect.Right, $rect.Bottom, $t, $h)
    }
    return $true
}
[WE2]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
$lines | Out-String
