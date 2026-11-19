# z-order walk v4: first 30 visible, plus mark target windows
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class ZD {
    public struct WinInfo { public IntPtr Hwnd; public bool Vis; public uint Pid; public int L, T, R, B; public string Title; public int ExStyle; }
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder t, int c);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int idx);
    public struct RECT { public int L, T, R, B; }
    public static List<WinInfo> List() {
        var all = new List<WinInfo>();
        EnumWindows((h, l) => {
            uint pid; GetWindowThreadProcessId(h, out pid);
            RECT r; GetWindowRect(h, out r);
            var sb = new StringBuilder(512); GetWindowText(h, sb, 512);
            all.Add(new WinInfo { Hwnd = h, Vis = IsWindowVisible(h), Pid = pid, L = r.L, T = r.T, R = r.R, B = r.B, Title = sb.ToString(), ExStyle = GetWindowLong(h, -20) });
            return true;
        }, IntPtr.Zero);
        return all;
    }
}
"@
$wins = [ZD]::List()
$visIdx = 0
foreach ($w in $wins) {
    $proc = Get-Process -Id $w.Pid -ErrorAction SilentlyContinue
    $pn = if ($proc) { $proc.ProcessName } else { "?" }
    $area = ($w.R - $w.L) * ($w.B - $w.T)
    if ($w.Vis -and $area -gt 5000 -and $visIdx -lt 28) {
        Write-Output ('vis#{0} pid={1} proc={2} ex=0x{3:X} rect=({4},{5})-({6},{7}) title=[{8}] hwnd={9}' -f $visIdx, $w.Pid, $pn, $w.ExStyle, $w.L, $w.T, $w.R, $w.B, $w.Title, $w.Hwnd)
        $visIdx++
    }
    if ($w.Hwnd -eq [IntPtr]44635390) { Write-Output ('TARGET-DUTHUB rawIdx={0} vis={1} ex=0x{2:X}' -f $visIdx, $w.Vis, $w.ExStyle) }
    if ($w.Hwnd -eq [IntPtr]202382) { Write-Output ('BACKSTOP rawIdx={0} vis={1} ex=0x{2:X}' -f $visIdx, $w.Vis, $w.ExStyle) }
}
