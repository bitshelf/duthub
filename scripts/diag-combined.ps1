# combined: enum backstop + windowfrompoint in one process
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class ZC {
    public struct WinInfo { public IntPtr Hwnd; public bool Vis; public uint Pid; public int L, T, R, B; public string Title; public int ExStyle; }
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder t, int c);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int idx);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder t, int c);
    public struct RECT { public int L, T, R, B; }
    public struct POINT { public int X, Y; }
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
    public static string PointInfo(int x, int y) {
        POINT p; p.X = x; p.Y = y;
        IntPtr h = WindowFromPoint(p);
        uint pid; GetWindowThreadProcessId(h, out pid);
        var sb = new StringBuilder(512); GetWindowText(h, sb, 512);
        var cn = new StringBuilder(256); GetClassName(h, cn, 256);
        RECT r; GetWindowRect(h, out r);
        return "WFP hwnd=" + h + " pid=" + pid + " class=[" + cn + "] title=[" + sb + "] vis=" + IsWindowVisible(h) + " ex=0x" + GetWindowLong(h, -20).ToString("X") + " rect=(" + r.L + "," + r.T + ")-(" + r.R + "," + r.B + ")";
    }
}
"@
$wins = [ZC]::List()
$found = 0
foreach ($w in $wins) {
    if ($w.Title -match "Backstop") {
        $proc = Get-Process -Id $w.Pid -ErrorAction SilentlyContinue
        Write-Output ('ENUM-BACKSTOP hwnd={0} vis={1} ex=0x{2:X} rect=({3},{4})-({5},{6}) proc={7}' -f $w.Hwnd, $w.Vis, $w.ExStyle, $w.L, $w.T, $w.R, $w.B, $proc.ProcessName)
        $found++
    }
}
Write-Output ("enum matches: " + $found)
Write-Output ([ZC]::PointInfo(425, 150))
Write-Output ([ZC]::PointInfo(800, 400))
