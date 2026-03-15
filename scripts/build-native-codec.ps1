<#
.SYNOPSIS
    Build the Rust/PyO3 native codec extension and install into the package.
.PARAMETER Release
    Build in release mode (optimized). Default is debug.
.PARAMETER Python
    Path to Python interpreter. Defaults to .venv/Scripts/python.exe.
#>
param(
    [switch]$Release,
    [string]$Python
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$rustDir = Join-Path $root 'rust'
$targetDir = Join-Path $root 'src' 'agent_bus_mcp'

# Resolve Python interpreter
if (-not $Python) {
    $Python = Join-Path $root '.venv' 'Scripts' 'python.exe'
}
if (-not (Test-Path $Python)) {
    throw "Python not found at $Python"
}

# Get the correct extension suffix for this Python (e.g. .cp313-win_amd64.pyd)
$extSuffix = & $Python -c "import sysconfig; print(sysconfig.get_config_var('EXT_SUFFIX'))"
Write-Host "Python extension suffix: $extSuffix" -ForegroundColor DarkGray

Push-Location $rustDir
try {
    $mode = if ($Release) { 'release' } else { 'debug' }
    Write-Host "Building native codec ($mode)..." -ForegroundColor Cyan

    # Set PYO3_PYTHON so PyO3 links against the correct Python
    $env:PYO3_PYTHON = $Python
    $env:RUSTC_WRAPPER = ''

    if ($Release) {
        cargo build --release 2>&1
    } else {
        cargo build 2>&1
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }

    # Resolve actual target directory from cargo metadata (may be shared cache)
    $targetJson = cargo metadata --format-version 1 --no-deps 2>$null | ConvertFrom-Json
    $cargoTarget = if ($targetJson.target_directory) { $targetJson.target_directory } else { 'target' }
    $buildDir = Join-Path $cargoTarget $mode

    $dllName = '_codec_native.dll'
    $src = Join-Path $buildDir $dllName
    if (Test-Path $src) {
        $destName = "_codec_native$extSuffix"
        $dest = Join-Path $targetDir $destName
        Copy-Item $src $dest -Force
        Write-Host "Installed: $dllName -> $dest" -ForegroundColor Green

        # Verify it loads
        $meta = & $Python -c "from agent_bus_mcp.codec import codec_metadata; import json; print(json.dumps(codec_metadata()))" 2>&1
        Write-Host "Backend: $meta" -ForegroundColor Cyan
    } else {
        Write-Warning "Native codec build succeeded but $dllName not found in $buildDir."
        Write-Warning "Python fallback will remain active."
    }
} finally {
    Pop-Location
    Remove-Item env:PYO3_PYTHON -ErrorAction SilentlyContinue
}
