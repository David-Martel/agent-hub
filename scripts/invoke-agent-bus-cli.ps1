param(
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$CliArgs
)

$ErrorActionPreference = "Stop"

# Use the Rust binary directly (primary entry point)
$rustBin = Join-Path $HOME "bin/agent-bus.exe"
$repoRustBin = Join-Path $PSScriptRoot "..\rust-cli\target\release\agent-bus.exe"

if (-not (Test-Path $rustBin)) {
    if (Test-Path $repoRustBin) {
        $rustBin = (Resolve-Path $repoRustBin).Path
    }
    else {
        throw "agent-bus binary not found at $rustBin or $repoRustBin. Build with: cargo build --release in rust-cli/"
    }
}

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
}
if (-not $env:AGENT_BUS_DATABASE_URL) {
    $env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@localhost:5300/redis_backend"
}
if (-not $env:AGENT_BUS_SERVER_HOST) {
    $env:AGENT_BUS_SERVER_HOST = "localhost"
}

& $rustBin @CliArgs
if ($LASTEXITCODE -ne 0) {
    throw "agent-bus command failed (exit $LASTEXITCODE): $($CliArgs -join ' ')"
}
