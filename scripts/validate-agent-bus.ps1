param(
    [switch]$SkipBuild,
    [switch]$SkipTests,
    [switch]$SkipHealth
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$rustCliRoot = Join-Path $repoRoot "rust-cli"
$invokeScript = Join-Path $PSScriptRoot "invoke-agent-bus-cli.ps1"

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
}
if (-not $env:AGENT_BUS_DATABASE_URL) {
    $env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@localhost:5432/redis_backend"
}
if (-not $env:AGENT_BUS_SERVER_HOST) {
    $env:AGENT_BUS_SERVER_HOST = "localhost"
}

if (-not $SkipBuild) {
    Write-Host "Building native agent-bus binary..."
    Push-Location $rustCliRoot
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release failed."
        }
    }
    finally {
        Pop-Location
    }
}

if (-not $SkipTests) {
    Write-Host "Running native test suite..."
    Push-Location $rustCliRoot
    try {
        & cargo test
        if ($LASTEXITCODE -ne 0) {
            throw "cargo test failed."
        }
    }
    finally {
        Pop-Location
    }
}

if (-not $SkipHealth) {
    Write-Host "Running live Redis/PostgreSQL health check..."
    & $invokeScript "health" "--encoding" "json"
    if ($LASTEXITCODE -ne 0) {
        throw "agent-bus health validation failed."
    }
}
