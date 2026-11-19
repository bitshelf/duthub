# 裁剪放大截图区域便于读图取证
# 用法: powershell -File scripts/ui-crop.ps1 -SrcFile <png> -X <x> -Y <y> -W <w> -H <h> -OutFile <png> [-Zoom 2]
param(
    [string]$SrcFile = "target/ui-verify/00-initial.png",
    [int]$X = 0, [int]$Y = 0, [int]$W = 400, [int]$H = 60,
    [string]$OutFile = "target/ui-verify/crop.png",
    [int]$Zoom = 2
)
Add-Type -AssemblyName System.Drawing
$root = Split-Path $PSScriptRoot -Parent
$inPath = Join-Path $root $SrcFile
$outPath = Join-Path $root $OutFile
$bmpIn = New-Object -TypeName System.Drawing.Bitmap -ArgumentList $inPath
$rect = New-Object -TypeName System.Drawing.Rectangle -ArgumentList $X, $Y, $W, $H
$bmpCrop = $bmpIn.Clone($rect, $bmpIn.PixelFormat)
$zw = [int]($W * $Zoom); $zh = [int]($H * $Zoom)
$bmpBig = New-Object -TypeName System.Drawing.Bitmap -ArgumentList $bmpCrop, $zw, $zh
$dir = Split-Path $outPath -Parent
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmpBig.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)
$bmpIn.Dispose(); $bmpCrop.Dispose(); $bmpBig.Dispose()
Write-Output "SAVED $outPath ($zw x $zh)"
