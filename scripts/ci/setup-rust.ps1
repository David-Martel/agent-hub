$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$cacheRoot = if ($env:AGENT_HUB_CI_CACHE_ROOT) {
    $env:AGENT_HUB_CI_CACHE_ROOT
} else {
    Join-Path $env:LOCALAPPDATA "agent-hub-ci"
}
$cargoTarget = Join-Path $cacheRoot "target"
$sccacheDir = Join-Path $cacheRoot "sccache"

New-Item -ItemType Directory -Force -Path $cargoTarget, $sccacheDir | Out-Null

$cargoPath = (& rustup which cargo).Trim()
$rustcPath = (& rustup which rustc).Trim()
$toolchainBin = Split-Path -Parent $cargoPath
$env:PATH = "$toolchainBin;$($env:PATH)"

$env:CARGO_TARGET_DIR = $cargoTarget
$env:CARGO_INCREMENTAL = "0"
$env:SCCACHE_DIR = $sccacheDir
$env:SCCACHE_SERVER_PORT = "4228"

$sccache = Get-Command sccache -ErrorAction SilentlyContinue
if ($sccache) {
    & $sccache.Source --stop-server 2>$null | Out-Null
    & $sccache.Source --start-server | Out-Null
    & $sccache.Source --show-stats | Out-Null
    $env:RUSTC_WRAPPER = $sccache.Source
    Write-Host "sccache enabled ($($sccache.Source); cache directory $sccacheDir)"
} else {
    Remove-Item Env:RUSTC_WRAPPER -ErrorAction SilentlyContinue
    Write-Warning "sccache is unavailable; continuing with persistent Cargo outputs"
}

if ($env:GITHUB_ENV) {
    @(
        "CARGO_TARGET_DIR=$cargoTarget"
        "CARGO_INCREMENTAL=0"
        "SCCACHE_DIR=$sccacheDir"
        "SCCACHE_SERVER_PORT=4228"
    ) | Add-Content -Path $env:GITHUB_ENV -Encoding utf8
    if ($env:RUSTC_WRAPPER) {
        "RUSTC_WRAPPER=$($env:RUSTC_WRAPPER)" |
            Add-Content -Path $env:GITHUB_ENV -Encoding utf8
    }
}
if ($env:GITHUB_PATH) {
    $toolchainBin | Add-Content -Path $env:GITHUB_PATH -Encoding utf8
}

& $cargoPath --version
& $rustcPath --version
