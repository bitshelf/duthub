# try: raise duthub to topmost band, re-test WFP, click via sendinput
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class TM {
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int hh, uint f);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder t, int c);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    public struct POINT { public int X, Y; }
}
"@
$h = [IntPtr]44635390
$r1 = [TM]::SetWindowPos($h, [IntPtr](-1), 0,0,0,0, 0x0003)  # HWND_TOPMOST
Write-Output ("settopmost=" + $r1)
Start-Sleep -Milliseconds 400
$p = New-Object TM+POINT; $p.X = 425; $p.Y = 150
$wfp = [TM]::WindowFromPoint($p)
$pid2 = 0
[TM]::GetWindowThreadProcessId($wfp, [ref]$pid2) | Out-Null
$sb = New-Object System.Text.StringBuilder 256
[TM]::GetWindowText($wfp, $sb, 256) | Out-Null
Write-Output ("after-topmost WFP(425,150) -> hwnd=$wfp pid=$pid2 title=[" + $sb + "]")
if ($wfp -eq $h) {
    [TM]::SetCursorPos(425, 150) | Out-Null
    Start-Sleep -Milliseconds 150
    [TM]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero)
    [TM]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
    Write-Output "clicked sparkle (topmost)"
}
