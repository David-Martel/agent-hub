$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$cacheRoot = if ($env:AGENT_HUB_CI_CACHE_ROOT) {
    $env:AGENT_HUB_CI_CACHE_ROOT
} else {
    Join-Path $env:LOCALAPPDATA "agent-hub-ci"
}
$jobNamespace = if ($env:GITHUB_JOB) {
    $env:GITHUB_JOB -replace '[^A-Za-z0-9_.-]', '_'
} else {
    "local"
}
$archNamespace = if ($env:RUNNER_ARCH) { $env:RUNNER_ARCH } else { "unknown" }
$cargoTarget = Join-Path $cacheRoot "target\$jobNamespace-$archNamespace"
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

$pinnedSccache = Join-Path $env:USERPROFILE ".cargo\bin\sccache.exe"
$sccache = if (Test-Path -LiteralPath $pinnedSccache -PathType Leaf) {
    Get-Item -LiteralPath $pinnedSccache
} else {
    Get-Command sccache -ErrorAction SilentlyContinue
}
if ($sccache) {
    $sccachePath = if ($sccache -is [System.IO.FileInfo]) {
        $sccache.FullName
    } else {
        $sccache.Source
    }
    $sccacheVersion = (& $sccachePath --version).Trim()
    if ($sccacheVersion -ne "sccache 0.16.0") {
        Write-Warning "Expected sccache 0.16.0, found $sccacheVersion at $sccachePath"
    }
    & $sccachePath --stop-server 2>$null | Out-Null
    & $sccachePath --start-server | Out-Null
    & $sccachePath --show-stats | Out-Null
    $env:RUSTC_WRAPPER = $sccachePath
    Write-Host "sccache enabled ($sccachePath; cache directory $sccacheDir)"
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
