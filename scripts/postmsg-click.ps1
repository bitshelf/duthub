# post WM_LBUTTONDOWN/UP directly to duthub hwnd (bypass hit-test backstop)
param([int]$Cx = 63, [int]$Cy = 18, [string]$Out = "")
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class PM {
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int hh, uint f);
}
"@
$h = [IntPtr]44635390
[PM]::SetWindowPos($h, [IntPtr](-1), 0,0,0,0, 0x0003) | Out-Null
[PM]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 300
$lp = [IntPtr](($Cy -shl 16) -bor ($Cx -band 0xFFFF))
[PM]::PostMessage($h, 0x0200, [IntPtr]::Zero, $lp) | Out-Null   # WM_MOUSEMOVE
Start-Sleep -Milliseconds 80
[PM]::PostMessage($h, 0x0201, [IntPtr]1, $lp) | Out-Null       # WM_LBUTTONDOWN MK_LBUTTON
Start-Sleep -Milliseconds 60
[PM]::PostMessage($h, 0x0202, [IntPtr]::Zero, $lp) | Out-Null  # WM_LBUTTONUP
Start-Sleep -Milliseconds 900
Write-Output ("posted click at client($Cx,$Cy) lp=0x" + $lp.ToString("X"))
