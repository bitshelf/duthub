# Debug the same window matching callback used by capture-window.ps1.
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WD {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
}
"@
$target = [IntPtr]::Zero
$cb = {
    param($hWnd, $lParam)
    if ([WD]::IsWindowVisible($hWnd)) {
        $sb = New-Object System.Text.StringBuilder 256
        [WD]::GetWindowText($hWnd, $sb, 256) | Out-Null
        $t = $sb.ToString()
        if ($t -eq "duthub" -or $t -eq "") {
            $procId = 0
            [WD]::GetWindowThreadProcessId($hWnd, [ref]$procId) | Out-Null
            $proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
            $pn = if ($proc) { $proc.ProcessName } else { "<none>" }
            if ($t -eq "" -and $pn -notlike "*Serial*" -and $pn -notlike "*duthub*") { return $true }
            Write-Host "CANDIDATE title=[$t] pid=$procId proc=$pn"
            $script:target = $hWnd
            return $false
        }
    }
    return $true
}
[WD]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
if ($target -eq [IntPtr]::Zero) { Write-Host "NOT FOUND" } else { Write-Host "FOUND hwnd=$target" }
