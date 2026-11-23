<#
.SYNOPSIS
Runs the RBS app in one of several ways (Windows; macOS and Linux: run.sh).

.DESCRIPTION
    tools\dev\run.ps1 [-Release | -UiDev] [-Build] [-NoBrowser] [-TestBrowser]
                      [-FreshConfig | -Config <dir>] [-Log <levels>] [-- app arguments]

Modes: the debug build (default), -Release for the release build, or -UiDev for the
UI live preview (saving a .slint file reloads the windows with no Rust build;
cargo xtask ui-dev; never for measurements).

.PARAMETER Release
Run the release build.
.PARAMETER UiDev
Run the UI live preview instead of a build.
.PARAMETER Build
Build first (tools\dev\build.ps1 with the same profile).
.PARAMETER NoBrowser
With -Build: skip the CEF browser runner.
.PARAMETER TestBrowser
Browser sources use rbs-browser-helper.exe, the synthetic page, instead of CEF
(built with -Build; RBS_BROWSER_HELPER).
.PARAMETER FreshConfig
Start from an empty configuration in a new temporary folder (the first-run page);
your own configuration is not touched.
.PARAMETER Config
Use this folder as the configuration folder (RBS_CONFIG_DIR).
.PARAMETER Log
Diagnostics log levels for this run (RBS_LOG, docs/specs/diagnostics-log.md).
.PARAMETER AppArgs
Everything else is passed to the app.
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$UiDev,
    [switch]$Build,
    [switch]$NoBrowser,
    [switch]$TestBrowser,
    [switch]$FreshConfig,
    [string]$Config,
    [string]$Log,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$AppArgs
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$out = Join-Path $target $(if ($Release) { 'release' } else { 'debug' })
if (-not $AppArgs) { $AppArgs = @() }

if ($FreshConfig) {
    $Config = Join-Path ([IO.Path]::GetTempPath()) "rbs-fresh-config-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
    New-Item -ItemType Directory -Path $Config | Out-Null
    Write-Host "Fresh configuration: $Config"
}
if ($Config) { $env:RBS_CONFIG_DIR = $Config }
if ($Log) { $env:RBS_LOG = $Log }

if ($UiDev) {
    if ($TestBrowser) { Write-Warning '-TestBrowser is not used with -UiDev' }
    Push-Location $root
    try { & cargo xtask ui-dev @AppArgs } finally { Pop-Location }
    exit $LASTEXITCODE
}

if ($Build) {
    $buildArgs = @{ Release = $Release; NoBrowser = $NoBrowser; TestBrowser = $TestBrowser }
    & (Join-Path $PSScriptRoot 'build.ps1') @buildArgs
}

if ($TestBrowser) {
    $helper = Join-Path $out 'rbs-browser-helper.exe'
    if (-not (Test-Path $helper)) { throw "$helper is missing; add -Build" }
    $env:RBS_BROWSER_HELPER = $helper
}

$exe = Join-Path $out 'rbs.exe'
if (-not (Test-Path $exe)) { throw "$exe is missing; add -Build" }
& $exe @AppArgs
exit $LASTEXITCODE
