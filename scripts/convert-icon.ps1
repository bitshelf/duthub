# Build a multi-resolution ICO and previews from a supplied icon.png.
# Usage: powershell -File scripts/convert-icon.ps1 -SourcePath <icon.png path> [-OutDir <output directory>]
param(
    [Parameter(Mandatory=$true)][string]$SourcePath,
    [string]$OutDir = ""
)

Add-Type -AssemblyName System.Drawing

if ($OutDir -eq "") {
    $root = Split-Path $PSScriptRoot -Parent
    $OutDir = Join-Path $root "apps/desktop/assets"
}
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

$src = [System.Drawing.Image]::FromFile($SourcePath)

# Crop a square from the center.
$side = [Math]::Min($src.Width, $src.Height)
$sq = New-Object System.Drawing.Bitmap $side, $side
$g = [System.Drawing.Graphics]::FromImage($sq)
$g.InterpolationMode = 'HighQualityBicubic'
$g.SmoothingMode = 'AntiAlias'
$g.PixelOffsetMode = 'HighQuality'
$offX = [int](($src.Width - $side) / 2)
$offY = [int](($src.Height - $side) / 2)
$g.DrawImage($src, (New-Object System.Drawing.Rectangle 0, 0, $side, $side),
    (New-Object System.Drawing.Rectangle $offX, $offY, $side, $side), 'Pixel') | Out-Null
$g.Dispose() | Out-Null

# Resize for each icon size.
$sizes = @(256, 128, 64, 48, 32, 16)
$pngs = @{}
foreach ($s in $sizes) {
    $img = New-Object System.Drawing.Bitmap $s, $s
    $gg = [System.Drawing.Graphics]::FromImage($img)
    $gg.InterpolationMode = 'HighQualityBicubic'
    $gg.SmoothingMode = 'AntiAlias'
    $gg.PixelOffsetMode = 'HighQuality'
    $gg.DrawImage($sq, (New-Object System.Drawing.Rectangle 0, 0, $s, $s)) | Out-Null
    $gg.Dispose() | Out-Null
    $ms = New-Object System.IO.MemoryStream
    $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png) | Out-Null
    $pngs[$s] = $ms.ToArray()
    $ms.Dispose() | Out-Null
    if ($s -eq 256) {
        $img.Save((Join-Path $OutDir "app-256.png"), [System.Drawing.Imaging.ImageFormat]::Png) | Out-Null
    }
    $img.Dispose() | Out-Null
}
$sq.Dispose() | Out-Null
$src.Dispose() | Out-Null

# Pack the images into the ICO container.
$msIco = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $msIco
$bw.Write([uint16]0) | Out-Null
$bw.Write([uint16]1) | Out-Null
$bw.Write([uint16]$sizes.Count) | Out-Null
$offset = 6 + 16 * $sizes.Count
foreach ($s in $sizes) {
    $data = $pngs[$s]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim) | Out-Null
    $bw.Write([byte]$dim) | Out-Null
    $bw.Write([byte]0) | Out-Null
    $bw.Write([byte]0) | Out-Null
    $bw.Write([uint16]1) | Out-Null
    $bw.Write([uint16]32) | Out-Null
    $bw.Write([uint32]$data.Length) | Out-Null
    $bw.Write([uint32]$offset) | Out-Null
    $offset += $data.Length
}
foreach ($s in $sizes) { $bw.Write($pngs[$s]) | Out-Null }
$bw.Flush() | Out-Null
[System.IO.File]::WriteAllBytes((Join-Path $OutDir "app.ico"), $msIco.ToArray()) | Out-Null
$bw.Dispose() | Out-Null
$msIco.Dispose() | Out-Null

Write-Output "OK -> $OutDir (app.ico + app-256.png)"
