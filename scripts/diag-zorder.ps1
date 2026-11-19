# z-order walk with styles (diagnostic)
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class ZD {
  [DllImport("user32.dll")] public static extern IntPtr GetTopWindow();
  [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
  [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int idx);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder t, int c);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  public struct RECT { public int L, T, R, B; }
}
"@
$h = [ZD]::GetTopWindow()
$count = 0
while ($h -ne [IntPtr]::Zero -and $count -lt 80) {
    $p2 = 0
    [ZD]::GetWindowThreadProcessId($h, [ref]$p2) | Out-Null
    $proc = Get-Process -Id $p2 -ErrorAction SilentlyContinue
    $sb = New-Object System.Text.StringBuilder 128
    [ZD]::GetWindowText($h, $sb, 128) | Out-Null
    $r = New-Object ZD+RECT
    [ZD]::GetWindowRect($h, [ref]$r) | Out-Null
    $ex = [ZD]::GetWindowLong($h, -20)
    $vis = [ZD]::IsWindowVisible($h)
    $icon = [ZD]::IsIconic($h)
    $area = ($r.R - $r.L) * ($r.B - $r.T)
    if ($vis -and -not $icon -and $area -gt 5000) {
        Write-Output ('z{0} pid={1} proc={2} ex=0x{3:X} rect=({4},{5})-({6},{7}) title=[{8}] hwnd={9}' -f $count, $p2, $proc.ProcessName, $ex, $r.L, $r.T, $r.R, $r.B, $sb.ToString(), $h)
    }
    $h = [ZD]::GetWindow($h, 2)
    $count++
}
