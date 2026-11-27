<#
.SYNOPSIS
    Builds Riptide for release and packs it into a zip that runs on a machine with nothing
    else installed.

.DESCRIPTION
    Builds riptide-launcher.exe and riptide.exe in release mode with the C runtime linked
    in, so the package needs no Visual C++ redistributable and no Rust toolchain. Checks
    that neither program imports a C runtime DLL, puts both in one folder with a short
    README, and zips the folder.

    The package is written to local\dist, which is never committed.

    The build needs what docs\setup.md lists. It uses its own build folder, target\package,
    so it does not disturb everyday builds.
    Works in Windows PowerShell 5.1 and PowerShell 7.

.PARAMETER Target7800X3D
    Build for the Ryzen 7 7800X3D (Zen 4, AVX-512). The programs in that package crash on a
    processor without Zen 4's instructions.

.PARAMETER OutputFolder
    Where the package folder and the zip go. Default: local\dist in this repository.

.EXAMPLE
    .\scripts\package.ps1
    Makes local\dist\riptide-<version>-win-x64.zip, which runs on any 64-bit Windows 11 PC.

.EXAMPLE
    .\scripts\package.ps1 -Target7800X3D
    Makes local\dist\riptide-<version>-win-x64-7800x3d.zip for the target machine.
#>
[CmdletBinding()]
param(
    [switch]$Target7800X3D,
    [string]$OutputFolder
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $OutputFolder) {
    $OutputFolder = Join-Path $repository 'local\dist'
}
$OutputFolder = [System.IO.Path]::GetFullPath($OutputFolder)

# The version is the workspace's, from Cargo.toml.
$manifest = Get-Content (Join-Path $repository 'Cargo.toml') -Raw
if ($manifest -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    throw 'No version found under [workspace.package] in Cargo.toml.'
}
$version = $Matches[1]

$name = "riptide-$version-win-x64"
# The C runtime is linked into the programs, so the package needs no redistributable.
$flags = '-C target-feature=+crt-static'
if ($Target7800X3D) {
    $name += '-7800x3d'
    $flags += ' -C target-cpu=znver4'
}

$buildFolder = Join-Path $repository 'target\package'
if ($Target7800X3D) {
    $buildFolder += '-7800x3d'
}

Write-Host "Building $name ..."
$previousFlags = $env:RUSTFLAGS
$env:RUSTFLAGS = $flags
try {
    Push-Location $repository
    try {
        & cargo build --release --locked -p riptide-launcher -p riptide --target-dir $buildFolder
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}
finally {
    $env:RUSTFLAGS = $previousFlags
}

$built = Join-Path $buildFolder 'release'
$programs = @('riptide-launcher.exe', 'riptide.exe')

$stage = Join-Path $OutputFolder $name
if (Test-Path $stage) {
    Remove-Item $stage -Recurse -Force
}
[void](New-Item -ItemType Directory -Path $stage -Force)
foreach ($program in $programs) {
    Copy-Item (Join-Path $built $program) $stage
}

# Riptide's own census lists what a program imports. Neither program may import a C runtime
# DLL, or the package would not start on a machine without the redistributable.
$census = Join-Path $stage 'riptide.exe'
foreach ($program in $programs) {
    $imports = & $census census (Join-Path $stage $program)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not read the imports of $program."
    }
    $runtime = @($imports | Where-Object { $_ -match 'vcruntime|msvcp|ucrtbase|api-ms-win-crt' })
    if ($runtime.Count -gt 0) {
        throw "$program imports a C runtime DLL and would need the redistributable: $($runtime[0])"
    }
}

$cpu = 'Any 64-bit processor.'
if ($Target7800X3D) {
    $cpu = 'A Ryzen 7 7800X3D or another Zen 4 processor. On any other processor these programs crash.'
}
$readme = @"
Riptide $version

Start riptide-launcher.exe. Keep riptide.exe in the same folder: the launcher's Play
starts it.

Needs
  Windows 11, 64-bit.
  $cpu
  Nothing else: no installer, no redistributable.

Riptide is at the design stage. The launcher works; no title runs yet.

Where Riptide keeps things
  Settings    %APPDATA%\Riptide\riptide.toml
  Logs        %LOCALAPPDATA%\Riptide\logs

To remove Riptide, delete this folder and those two.

Source and license: https://github.com/m-de-graaff/Riptide
"@
# Windows line endings, so Notepad on any Windows shows it right.
$readme = ($readme -replace "`r?`n", "`r`n") + "`r`n"
[System.IO.File]::WriteAllText((Join-Path $stage 'README.txt'), $readme, (New-Object System.Text.UTF8Encoding($false)))

$zip = Join-Path $OutputFolder "$name.zip"
if (Test-Path $zip) {
    Remove-Item $zip -Force
}
Compress-Archive -Path $stage -DestinationPath $zip

Write-Host ''
foreach ($program in $programs) {
    $size = (Get-Item (Join-Path $stage $program)).Length / 1MB
    Write-Host ('  {0,-22} {1,6:N1} MiB' -f $program, $size)
}
Write-Host ('  {0,-22} {1,6:N1} MiB' -f 'zip', ((Get-Item $zip).Length / 1MB))
Write-Host ''
Write-Host "Package: $zip"
