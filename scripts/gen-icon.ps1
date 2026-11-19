# duthub 应用图标生成器（线性版，避免 PS 函数输出管道坑）
# 设计：青色渐变圆角方（品牌色）+ 深色 UART 波形（串口信号）+ 亮色终端光标块
param([string]$OutDir = "")

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

if ($OutDir -eq "") {
    $root = Split-Path $PSScriptRoot -Parent
    $OutDir = Join-Path $root "apps/desktop/assets"
}
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

# 256 主图（GraphicsPath 直接构建）
$S = 256
$bmp = New-Object System.Drawing.Bitmap $S, $S
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = 'AntiAlias'
$g.PixelOffsetMode = 'HighQuality'

# 1) 品牌青色渐变圆角方
$bgPath = New-Object System.Drawing.Drawing2D.GraphicsPath
foreach ($seg in @(
    @{x=8;   y=8;   d=112; a=180; s=90},
    @{x=144; y=8;   d=112; a=270; s=90},
    @{x=144; y=144; d=112; a=0;   s=90},
    @{x=8;   y=144; d=112; a=90;  s=90}
)) {
    $bgPath.AddArc($seg.x, $seg.y, $seg.d, $seg.d, $seg.a, $seg.s) | Out-Null
}
$bgPath.CloseFigure() | Out-Null
$brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush (
    (New-Object System.Drawing.Point 0, 0),
    (New-Object System.Drawing.Point 256, 256),
    [System.Drawing.Color]::FromArgb(255, 90, 220, 198),
    [System.Drawing.Color]::FromArgb(255, 36, 146, 128)
)
$g.FillPath($brush, $bgPath) | Out-Null

# 2) 内侧深色细描边
$penBg = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(255, 7, 16, 24)), 10
$g.DrawPath($penBg, $bgPath) | Out-Null
$penBg.Dispose() | Out-Null

# 3) UART 波形（0101 帧样式）
$ink = [System.Drawing.Color]::FromArgb(255, 7, 16, 24)
$wf = New-Object System.Drawing.Pen ($ink), 22
$wf.LineJoin = 'Round'; $wf.StartCap = 'Round'; $wf.EndCap = 'Round'
$pts = @(
    (New-Object System.Drawing.Point 34, 104), (New-Object System.Drawing.Point 62, 104),
    (New-Object System.Drawing.Point 62, 152), (New-Object System.Drawing.Point 94, 152),
    (New-Object System.Drawing.Point 94, 104), (New-Object System.Drawing.Point 126, 104),
    (New-Object System.Drawing.Point 126, 152), (New-Object System.Drawing.Point 158, 152),
    (New-Object System.Drawing.Point 158, 104), (New-Object System.Drawing.Point 178, 104)
)
$g.DrawLines($wf, $pts) | Out-Null
$wf.Dispose() | Out-Null

# 4) 终端光标块（亮色）
$cur = New-Object System.Drawing.Drawing2D.GraphicsPath
foreach ($seg in @(
    @{x=196; y=86;  d=20; a=180; s=90},
    @{x=216; y=86;  d=20; a=270; s=90},
    @{x=216; y=106; d=20; a=0;   s=90},
    @{x=196; y=106; d=20; a=90;  s=90}
)) {
    $cur.AddArc($seg.x, $seg.y, $seg.d, $seg.d, $seg.a, $seg.s) | Out-Null
}
$cur.CloseFigure() | Out-Null
$curBrush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 235, 255, 250))
$g.FillPath($curBrush, $cur) | Out-Null
$curBrush.Dispose() | Out-Null
$g.Dispose() | Out-Null

$bmp.Save((Join-Path $OutDir "app-256.png"), [System.Drawing.Imaging.ImageFormat]::Png) | Out-Null

# 多尺寸
$sizes = @(256, 128, 64, 48, 32, 16)
$pngs = @{}
foreach ($s in $sizes) {
    if ($s -eq 256) { $img = $bmp }
    else {
        $img = New-Object System.Drawing.Bitmap $s, $s
        $gg = [System.Drawing.Graphics]::FromImage($img)
        $gg.InterpolationMode = 'HighQualityBicubic'
        $gg.SmoothingMode = 'AntiAlias'
        $gg.PixelOffsetMode = 'HighQuality'
        $gg.DrawImage($bmp, (New-Object System.Drawing.Rectangle 0, 0, $s, $s)) | Out-Null
        $gg.Dispose() | Out-Null
    }
    $ms = New-Object System.IO.MemoryStream
    $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png) | Out-Null
    $pngs[$s] = $ms.ToArray()
    $ms.Dispose() | Out-Null
    if ($s -ne 256) { $img.Dispose() | Out-Null }
}
$bmp.Dispose() | Out-Null

# ICO 容器
$msIco = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $msIco
$bw.Write([uint16]0) | Out-Null; $bw.Write([uint16]1) | Out-Null; $bw.Write([uint16]$sizes.Count) | Out-Null
$offset = 6 + 16 * $sizes.Count
foreach ($s in $sizes) {
    $data = $pngs[$s]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim) | Out-Null; $bw.Write([byte]$dim) | Out-Null
    $bw.Write([byte]0) | Out-Null; $bw.Write([byte]0) | Out-Null
    $bw.Write([uint16]1) | Out-Null; $bw.Write([uint16]32) | Out-Null
    $bw.Write([uint32]$data.Length) | Out-Null
    $bw.Write([uint32]$offset) | Out-Null
    $offset += $data.Length
}
foreach ($s in $sizes) { $bw.Write($pngs[$s]) | Out-Null }
$bw.Flush() | Out-Null
[System.IO.File]::WriteAllBytes((Join-Path $OutDir "app.ico"), $msIco.ToArray()) | Out-Null
$bw.Dispose() | Out-Null; $msIco.Dispose() | Out-Null

Write-Output "OK -> $OutDir (app.ico, $(($pngs[256]).Length) bytes 256px png)"
