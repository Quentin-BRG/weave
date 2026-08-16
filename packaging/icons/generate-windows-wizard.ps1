# Derive the Inno Setup wizard artwork from the canonical Weave icon.
#
#     pwsh packaging/icons/generate-windows-wizard.ps1
#
# Inno Setup only reads BMP for wizard images, so the two files it needs are
# composited here from `assets/icons/linux/hicolor/<size>/apps/weave.png` — the
# same rasterisations of `docs/assets/weave-icon.svg` as every other platform
# icon. Nothing is redrawn: the mark is scaled and centred on the brand
# background, and the result is committed so the release build needs no image
# tooling.
#
# Re-run this after regenerating the PNGs from the SVG; the output is
# deterministic for a given input.
[CmdletBinding()]
param(
    [string]$Repo
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

if (-not $Repo) {
    # $PSScriptRoot is not reliably populated in a param() default under
    # Windows PowerShell 5.1, so resolve the repository root here instead.
    $Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
}

# Brand background, the same #181A1D the icon itself uses.
$background = [System.Drawing.Color]::FromArgb(255, 24, 26, 29)

function New-WizardImage {
    param(
        [int]$Width,
        [int]$Height,
        [int]$MarkSize,
        [string]$SourcePng,
        [string]$Destination
    )

    $source = [System.Drawing.Image]::FromFile($SourcePng)
    try {
        $bitmap = New-Object System.Drawing.Bitmap($Width, $Height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
        try {
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            try {
                $graphics.Clear($background)
                $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                $x = [int](($Width - $MarkSize) / 2)
                $y = [int](($Height - $MarkSize) / 2)
                $graphics.DrawImage($source, $x, $y, $MarkSize, $MarkSize)
            } finally {
                $graphics.Dispose()
            }
            $bitmap.Save($Destination, [System.Drawing.Imaging.ImageFormat]::Bmp)
        } finally {
            $bitmap.Dispose()
        }
    } finally {
        $source.Dispose()
    }
    Write-Output "generated $Destination"
}

$outDir = Join-Path $Repo 'assets\icons\windows'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

# WizardImageFile: the tall panel on the welcome and finished pages.
New-WizardImage -Width 164 -Height 314 -MarkSize 112 `
    -SourcePng (Join-Path $Repo 'assets\icons\linux\hicolor\128x128\apps\weave.png') `
    -Destination (Join-Path $outDir 'wizard-large.bmp')

# WizardSmallImageFile: the badge in the top-right of every other page.
New-WizardImage -Width 55 -Height 55 -MarkSize 44 `
    -SourcePng (Join-Path $Repo 'assets\icons\linux\hicolor\64x64\apps\weave.png') `
    -Destination (Join-Path $outDir 'wizard-small.bmp')
