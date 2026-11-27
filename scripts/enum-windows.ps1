# List visible top-level windows with their titles and process names.
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WE {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
}
"@
$lines = @()
$cb = {
    param($h, $l)
    $sb = New-Object System.Text.StringBuilder 256
    [WE]::GetWindowText($h, $sb, 256) | Out-Null
    $t = $sb.ToString()
    $p2 = 0
    [WE]::GetWindowThreadProcessId($h, [ref]$p2) | Out-Null
    $p = Get-Process -Id $p2 -ErrorAction SilentlyContinue
    if ([WE]::IsWindowVisible($h) -and $t -ne '') {
        $script:lines += ('pid={0} proc={1} title=[{2}] hwnd={3}' -f $p2, $p.ProcessName, $t, $h)
    }
    return $true
}
[WE]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
$lines | Out-String
