$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path (Split-Path -Parent $PSScriptRoot) "rust-build-common.ps1")
. (Join-Path $PSScriptRoot "cache-preflight.ps1")
$cachePolicy = Get-Content -LiteralPath (Join-Path $PSScriptRoot "windows-cache-policy.json") -Raw | ConvertFrom-Json

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

# Install the toolchain pinned by rust-toolchain.toml explicitly (see setup-rust.sh), then
# the components the fmt/clippy steps need, before resolving cargo/rustc through it.
& rustup toolchain install --profile minimal --no-self-update
if ($LASTEXITCODE -ne 0) { throw "rustup toolchain install failed ($LASTEXITCODE)" }
& rustup component add rustfmt clippy
if ($LASTEXITCODE -ne 0) { throw "rustup component add failed ($LASTEXITCODE)" }

$cargoPath = (& rustup which cargo).Trim()
$rustcPath = (& rustup which rustc).Trim()
$toolchainBin = Split-Path -Parent $cargoPath
$env:PATH = "$toolchainBin;$($env:PATH)"

$env:CARGO_TARGET_DIR = $cargoTarget
$env:CARGO_INCREMENTAL = "0"
$env:SCCACHE_DIR = $sccacheDir
$env:SCCACHE_SERVER_PORT = [string]$cachePolicy.server_port

# Never carry a previous runner/job wrapper into discovery or a failed health probe.
Remove-Item Env:RUSTC_WRAPPER -ErrorAction SilentlyContinue

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
    $sccacheVersion = (& $sccachePath --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $sccacheVersion -cne "sccache $($cachePolicy.version)") {
        throw "Expected sccache $($cachePolicy.version), found '$sccacheVersion' at $sccachePath"
    }
    $cacheProof = Invoke-AgentBusCachePreflight -SccachePath $sccachePath -RustcPath $rustcPath `
        -TargetDirectory $cargoTarget -Policy $cachePolicy
    $env:RUSTC_WRAPPER = $sccachePath
    Write-Host "sccache qualified ($sccachePath; port $($cacheProof.server_port); real Rust cache hit)"
} else {
    throw 'Required Windows sccache executable is unavailable; refusing uncached CI fallback'
}

if ($env:GITHUB_ENV) {
    @(
        "CARGO_TARGET_DIR=$cargoTarget"
        "CARGO_INCREMENTAL=0"
        "SCCACHE_DIR=$sccacheDir"
        "SCCACHE_SERVER_PORT=$($cachePolicy.server_port)"
    ) | Add-Content -Path $env:GITHUB_ENV -Encoding utf8
    # An empty entry also clears a stale wrapper inherited by subsequent steps.
    "RUSTC_WRAPPER=$($env:RUSTC_WRAPPER)" |
        Add-Content -Path $env:GITHUB_ENV -Encoding utf8
}
if ($env:GITHUB_PATH) {
    $toolchainBin | Add-Content -Path $env:GITHUB_PATH -Encoding utf8
}

& $cargoPath --version
& $rustcPath --version
