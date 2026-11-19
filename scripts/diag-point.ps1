# 诊断：指定屏幕点上的窗口归属 + 光标往返 + DPI
param([int]$Sx = 425, [int]$Sy = 150)
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class DG {
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern IntPtr ChildWindowFromPoint(IntPtr h, POINT p);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder t, int c);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  public struct POINT { public int X, Y; }
  public struct RECT { public int L, T, R, B; }
}
"@
$p = New-Object DG+POINT
$p.X = $Sx; $p.Y = $Sy
$hw = [DG]::WindowFromPoint($p)
$pid2 = 0
[DG]::GetWindowThreadProcessId($hw, [ref]$pid2) | Out-Null
$proc = Get-Process -Id $pid2 -ErrorAction SilentlyContinue
$sb = New-Object System.Text.StringBuilder 256
[DG]::GetWindowText($hw, $sb, 256) | Out-Null
$r = New-Object DG+RECT
[DG]::GetWindowRect($hw, [ref]$r) | Out-Null
Write-Output ("POINT($Sx,$Sy) -> hwnd=$hw pid=$pid2 proc={0} vis={1} title=[{2}] rect=({3},{4})-({5},{6}) dpi={7}" -f $proc.ProcessName, [DG]::IsWindowVisible($hw), $sb.ToString(), $r.L, $r.T, $r.R, $r.B, [DG]::GetDpiForWindow($hw))

# 光标往返
[DG]::SetCursorPos($Sx, $Sy) | Out-Null
Start-Sleep -Milliseconds 200
$cp = New-Object DG+POINT
[DG]::GetCursorPos([ref]$cp) | Out-Null
Write-Output ("cursor after SetCursorPos($Sx,$Sy): ($($cp.X),$($cp.Y))")

# Duthub 窗口的 DPI
$dw = Get-Process -Name Duthub -ErrorAction SilentlyContinue | Select-Object -First 1
if ($dw) {
    Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class DG2 {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  public struct RECT { public int L, T, R, B; }
}
"@
    $found = [IntPtr]::Zero
    $cb = {
        param($h, $l)
        $p2 = 0
        [DG2]::GetWindowThreadProcessId($h, [ref]$p2) | Out-Null
        if ($p2 -eq $dw.Id -and [DG2]::IsWindowVisible($h)) { $script:found = $h; return $false }
        return $true
    }
    [DG2]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
    if ($found -ne [IntPtr]::Zero) {
        $rr = New-Object DG2+RECT
        [DG2]::GetWindowRect($found, [ref]$rr) | Out-Null
        Write-Output ("Duthub hwnd=$found dpi={0} rect=({1},{2})-({3},{4}) size={5}x{6}" -f [DG2]::GetDpiForWindow($found), $rr.L, $rr.T, $rr.R, $rr.B, ($rr.R-$rr.L), ($rr.B-$rr.T))
    }
}
