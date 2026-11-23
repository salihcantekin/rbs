<#
.SYNOPSIS
Makes the download zip of RBS for Windows. The same script runs on a developer
machine and in CI (macOS and Linux: package.sh).

.DESCRIPTION
    tools\package\package.ps1 [-NoBuild] [-Label <text>] [-Out <dir>]

Builds the release app with tools\dev\build.ps1 -Release (rbs.exe, rbs-mcp.exe and the
CEF browser runner), collects exactly the files a user needs into
<out>\RBS-<version>-<label>\ and zips it to <out>\RBS-<version>-<label>.zip, with a
.sha256 file beside it. The browser engine (CEF runtime) is not included: the app
downloads it in Settings > Add-ons (ADR-0145). The virtual camera is not included
either: it needs a signed installation under Program Files (ADR-0066). Prints the zip
path on its last line. Honours CARGO_TARGET_DIR and CARGO_BUILD_JOBS.

.PARAMETER NoBuild
Package what is already built in target\release.
.PARAMETER Label
Name part after the version (default: windows-x64).
.PARAMETER Out
Where the zip goes (default: dist\ in the repository).
#>
[CmdletBinding()]
param(
    [switch]$NoBuild,
    [string]$Label = 'windows-x64',
    [string]$Out
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
if (-not $Out) { $Out = Join-Path $root 'dist' }
$cargoToml = Get-Content (Join-Path $root 'Cargo.toml')
$version = ($cargoToml | Where-Object { $_ -match '^version = "(.*)"' } | Select-Object -First 1) -replace '^version = "(.*)".*$', '$1'
if (-not $version) { throw 'no version in Cargo.toml' }
$name = "RBS-$version-$Label"
$bin = Join-Path $target 'release'

if (-not $NoBuild) {
    & (Join-Path $root 'tools\dev\build.ps1') -Release
    if ($LASTEXITCODE -ne 0) { throw "build.ps1 failed ($LASTEXITCODE)" }
}

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = (Resolve-Path $Out).Path
$stage = Join-Path $Out $name
$zip = Join-Path $Out "$name.zip"
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $stage, $zip, "$zip.sha256"
New-Item -ItemType Directory -Force -Path $stage | Out-Null

foreach ($file in 'rbs.exe', 'rbs-mcp.exe') {
    $path = Join-Path $bin $file
    if (-not (Test-Path $path)) { throw "missing $path (build first, or drop -NoBuild)" }
    Copy-Item $path $stage
}
# The browser runner is optional: without it browser sources are unavailable.
$runner = Join-Path $bin 'rbs-browser-cef.exe'
if (Test-Path $runner) { Copy-Item $runner $stage }

Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE')
$readme = (Get-Content -Raw (Join-Path $root 'tools\package\README-user.txt')).
    Replace('@RUN@', 'Double-click rbs.exe in this folder. If Windows SmartScreen warns about an unknown publisher, choose More info, then Run anyway.').
    Replace('@DATA@', '%APPDATA%\RBS')
Set-Content -Path (Join-Path $stage 'README.txt') -Value $readme
Set-Content -Path (Join-Path $stage 'VERSION.txt') -Value $version

Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
Set-Content -Path "$zip.sha256" -Value "$hash  $name.zip"
Remove-Item -Recurse -Force $stage
Write-Output $zip
