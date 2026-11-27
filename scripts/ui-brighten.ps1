# Brighten a screenshot to make dark UI elements easier to inspect.
# Usage: powershell -File scripts/ui-brighten.ps1 -SrcFile <png> -OutFile <png> [-Factor 2.5]
param(
    [string]$SrcFile = "target/ui-verify/in.png",
    [string]$OutFile = "target/ui-verify/out.png",
    [double]$Factor = 2.5
)
Add-Type -AssemblyName System.Drawing
$root = Split-Path $PSScriptRoot -Parent
$inPath = Join-Path $root $SrcFile
$outPath = Join-Path $root $OutFile
$bmpIn = New-Object -TypeName System.Drawing.Bitmap -ArgumentList $inPath
$bmpOut = New-Object -TypeName System.Drawing.Bitmap -ArgumentList $bmpIn.Width, $bmpIn.Height
for ($y = 0; $y -lt $bmpIn.Height; $y++) {
    for ($x = 0; $x -lt $bmpIn.Width; $x++) {
        $c = $bmpIn.GetPixel($x, $y)
        $r = [Math]::Min(255, [int]([Math]::Pow($c.R / 255.0, 0.45) * 255))
        $g = [Math]::Min(255, [int]([Math]::Pow($c.G / 255.0, 0.45) * 255))
        $b = [Math]::Min(255, [int]([Math]::Pow($c.B / 255.0, 0.45) * 255))
        $bmpOut.SetPixel($x, $y, [System.Drawing.Color]::FromArgb($c.A, $r, $g, $b))
    }
}
$dir = Split-Path $outPath -Parent
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmpOut.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)
$bmpIn.Dispose(); $bmpOut.Dispose()
Write-Output "SAVED $outPath"
