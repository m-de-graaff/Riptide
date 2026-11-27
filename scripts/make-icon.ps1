<#
.SYNOPSIS
    Draws Riptide's icon and writes it as the launcher's assets.

.DESCRIPTION
    The icon is drawn here, from code, so it is ours and can be redrawn: a dark rounded
    square in the launcher's colours with two waves across it.

    Writes two files into crates\riptide-launcher\assets:
      riptide.ico     sizes 16, 24, 32, 48, 64 and 256, built into the launcher's .exe
      icon-64.rgba    64 x 64 raw RGBA, shown in the window's title bar and the taskbar

    Run it again only when the drawing changes, and commit both files.
    Works in Windows PowerShell 5.1 and PowerShell 7.

.EXAMPLE
    .\scripts\make-icon.ps1
#>
[CmdletBinding()]
param(
    # Where the two files go. Default: crates\riptide-launcher\assets in this repository.
    [string]$OutputFolder
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# Not as the parameter's default: Windows PowerShell 5.1 has no $PSScriptRoot there yet.
if (-not $OutputFolder) {
    $OutputFolder = Join-Path $PSScriptRoot '..\crates\riptide-launcher\assets'
}
Add-Type -AssemblyName System.Drawing

# The launcher's colours (crates\riptide-launcher\src\theme.rs).
$surface = [System.Drawing.Color]::FromArgb(255, 17, 17, 17)
$border = [System.Drawing.Color]::FromArgb(255, 64, 64, 64)
$text = [System.Drawing.Color]::FromArgb(255, 237, 237, 237)
$accent = [System.Drawing.Color]::FromArgb(255, 62, 207, 142)

function New-IconBitmap {
    param([int]$Size)

    $bitmap = New-Object System.Drawing.Bitmap($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $graphics.Clear([System.Drawing.Color]::Transparent)

        # Everything is laid out on a 64-unit square and scaled to the size asked for.
        $unit = $Size / 64.0

        # The rounded square.
        $inset = 2.0 * $unit
        $side = $Size - 2 * $inset
        $radius = 13.0 * $unit
        $diameter = 2 * $radius
        $square = New-Object System.Drawing.Drawing2D.GraphicsPath
        $square.AddArc($inset, $inset, $diameter, $diameter, 180, 90)
        $square.AddArc($inset + $side - $diameter, $inset, $diameter, $diameter, 270, 90)
        $square.AddArc($inset + $side - $diameter, $inset + $side - $diameter, $diameter, $diameter, 0, 90)
        $square.AddArc($inset, $inset + $side - $diameter, $diameter, $diameter, 90, 90)
        $square.CloseFigure()
        $fill = New-Object System.Drawing.SolidBrush($surface)
        $graphics.FillPath($fill, $square)
        if ($Size -ge 32) {
            $edge = New-Object System.Drawing.Pen($border, [single][Math]::Max(1.0, 1.5 * $unit))
            $graphics.DrawPath($edge, $square)
            $edge.Dispose()
        }
        $fill.Dispose()
        $square.Dispose()

        # Two waves: the upper one in the text colour, the lower one in the accent.
        foreach ($wave in @(@{ Y = 26.0; Color = $text }, @{ Y = 40.0; Color = $accent })) {
            $pen = New-Object System.Drawing.Pen($wave.Color, [single][Math]::Max(1.6, 5.0 * $unit))
            $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
            $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
            $y = $wave.Y * $unit
            $swing = 9.0 * $unit
            $points = @(
                (New-Object System.Drawing.PointF([single](13 * $unit), [single]$y)),
                (New-Object System.Drawing.PointF([single](22 * $unit), [single]($y - $swing))),
                (New-Object System.Drawing.PointF([single](26 * $unit), [single]($y - $swing))),
                (New-Object System.Drawing.PointF([single](32 * $unit), [single]$y)),
                (New-Object System.Drawing.PointF([single](38 * $unit), [single]($y + $swing))),
                (New-Object System.Drawing.PointF([single](42 * $unit), [single]($y + $swing))),
                (New-Object System.Drawing.PointF([single](51 * $unit), [single]$y))
            )
            $graphics.DrawBeziers($pen, [System.Drawing.PointF[]]$points)
            $pen.Dispose()
        }
    }
    finally {
        $graphics.Dispose()
    }
    $bitmap
}

function Get-PngBytes {
    param([System.Drawing.Bitmap]$Bitmap)
    $stream = New-Object System.IO.MemoryStream
    try {
        $Bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
        , $stream.ToArray()
    }
    finally {
        $stream.Dispose()
    }
}

function Get-RgbaBytes {
    param([System.Drawing.Bitmap]$Bitmap)
    $bytes = New-Object byte[] ($Bitmap.Width * $Bitmap.Height * 4)
    $at = 0
    for ($y = 0; $y -lt $Bitmap.Height; $y++) {
        for ($x = 0; $x -lt $Bitmap.Width; $x++) {
            $pixel = $Bitmap.GetPixel($x, $y)
            $bytes[$at] = $pixel.R
            $bytes[$at + 1] = $pixel.G
            $bytes[$at + 2] = $pixel.B
            $bytes[$at + 3] = $pixel.A
            $at += 4
        }
    }
    , $bytes
}

$folder = [System.IO.Path]::GetFullPath($OutputFolder)
[void][System.IO.Directory]::CreateDirectory($folder)

# An .ico file is a small directory followed by the pictures, here each a PNG.
$sizes = @(16, 24, 32, 48, 64, 256)
$pictures = @()
foreach ($size in $sizes) {
    $bitmap = New-IconBitmap -Size $size
    $pictures += , (Get-PngBytes -Bitmap $bitmap)
    if ($size -eq 64) {
        $rgbaPath = Join-Path $folder 'icon-64.rgba'
        [System.IO.File]::WriteAllBytes($rgbaPath, (Get-RgbaBytes -Bitmap $bitmap))
    }
    $bitmap.Dispose()
}

$icoPath = Join-Path $folder 'riptide.ico'
$stream = New-Object System.IO.MemoryStream
$writer = New-Object System.IO.BinaryWriter($stream)
try {
    $writer.Write([uint16]0)              # reserved
    $writer.Write([uint16]1)              # 1 = icon
    $writer.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $size = $sizes[$i]
        # A size of 256 is written as 0.
        $writer.Write([byte]($size % 256))
        $writer.Write([byte]($size % 256))
        $writer.Write([byte]0)            # no palette
        $writer.Write([byte]0)            # reserved
        $writer.Write([uint16]1)          # colour planes
        $writer.Write([uint16]32)         # bits per pixel
        $writer.Write([uint32]$pictures[$i].Length)
        $writer.Write([uint32]$offset)
        $offset += $pictures[$i].Length
    }
    foreach ($picture in $pictures) {
        $writer.Write([byte[]]$picture)
    }
    $writer.Flush()
    [System.IO.File]::WriteAllBytes($icoPath, $stream.ToArray())
}
finally {
    $writer.Dispose()
    $stream.Dispose()
}

Write-Host "Wrote $icoPath ($((Get-Item $icoPath).Length) bytes, sizes $($sizes -join ', '))"
Write-Host "Wrote $rgbaPath ($((Get-Item $rgbaPath).Length) bytes)"
