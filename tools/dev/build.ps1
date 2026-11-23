<#
.SYNOPSIS
Builds the RBS app and the programs it starts from its own folder, and puts them
together so the app finds them (Windows; macOS and Linux: build.sh).

.DESCRIPTION
    tools\dev\build.ps1 [-Release] [-NoBrowser] [-BrowserOnly] [-TestBrowser]

What ends up next to rbs.exe in target\debug or target\release
(rbs-app browser_tree::resolve_runner, ai_tools, rbs-vcam-mf):
    rbs-mcp.exe                AI tools server
    rbs_vcam_mf_source.dll     virtual camera source
    rbs-browser-cef.exe        CEF browser runner

The browser engine (CEF runtime) is not built here: the app downloads it in
Settings > Add-ons (ADR-0145). The runner build needs cmake, ninja and the Visual
Studio C++ tools (rc.exe, mt.exe); when rc.exe is not on PATH this script loads the
vcvars64 environment of the newest Visual Studio with C++ tools (vswhere). The first
runner build downloads CEF once for its headers and import library.
Honours CARGO_TARGET_DIR and CARGO_BUILD_JOBS.

.PARAMETER Release
Optimised build (target\release); default is debug (target\debug).
.PARAMETER NoBrowser
Skip the CEF browser runner (browser sources stay Unavailable).
.PARAMETER BrowserOnly
Build only the CEF browser runner.
.PARAMETER TestBrowser
Also build rbs-browser-helper.exe, the synthetic page used without CEF.
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$NoBrowser,
    [switch]$BrowserOnly,
    [switch]$TestBrowser
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$profileName = if ($Release) { 'release' } else { 'debug' }
$releaseArgs = if ($Release) { @('--release') } else { @() }
$out = Join-Path $target $profileName

function Step([string]$text) { Write-Host "`n== $text" }

function Invoke-Cargo([string[]]$cargoArgs) {
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo $($cargoArgs -join ' ') failed ($LASTEXITCODE)" }
}

# Loads the vcvars64 environment into this process, once, when rc.exe is missing.
function Import-VcVars {
    if (Get-Command rc.exe -ErrorAction SilentlyContinue) { return }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { throw 'vswhere.exe not found: install Visual Studio with the C++ tools' }
    $bat = & $vswhere -latest -prerelease -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -find 'VC\Auxiliary\Build\vcvars64.bat' | Select-Object -First 1
    if (-not $bat) { throw 'vcvars64.bat not found: install the Visual Studio C++ x64 tools' }
    Write-Host "Loading $bat"
    $lines = & cmd.exe /d /s /c "call `"$bat`" >nul && set"
    if ($LASTEXITCODE -ne 0) { throw "vcvars64.bat failed ($LASTEXITCODE)" }
    foreach ($line in $lines) {
        $i = $line.IndexOf('=')
        if ($i -gt 0) { [Environment]::SetEnvironmentVariable($line.Substring(0, $i), $line.Substring($i + 1), 'Process') }
    }
}

Push-Location $root
try {
    if (-not $BrowserOnly) {
        Step "Building rbs, rbs-mcp and the virtual camera ($profileName)"
        $packages = @('-p', 'rbs-app', '-p', 'rbs-mcp', '-p', 'rbs-vcam-mf-source')
        if ($TestBrowser) { $packages += @('-p', 'rbs-browser-runner') }
        Invoke-Cargo (@('build') + $releaseArgs + $packages)
    }
    if (-not $NoBrowser) {
        Step "Building the CEF browser runner ($profileName)"
        Import-VcVars
        # Its own workspace and lock file (ADR-0132 decision 1); the shared target
        # directory puts rbs-browser-cef.exe beside rbs.exe.
        $manifest = Join-Path $root 'crates\rbs-browser-cef\Cargo.toml'
        Invoke-Cargo (@('build') + $releaseArgs + @('--manifest-path', $manifest, '--target-dir', $target))
    }
}
finally {
    Pop-Location
}

Write-Host "`nReady: $(Join-Path $out 'rbs.exe')"
if ($NoBrowser -and -not $BrowserOnly) {
    Write-Host 'No browser runner built: browser sources show helper_missing.'
}
