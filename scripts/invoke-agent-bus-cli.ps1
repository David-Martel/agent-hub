param(
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$CliArgs
)

$ErrorActionPreference = "Stop"

# Use the Rust binary directly (primary entry point)
$rustBin = Join-Path $HOME "bin/agent-bus.exe"

if (-not (Test-Path $rustBin)) {
    throw "agent-bus binary not found at $rustBin. Build with: cargo build --release in rust-cli/"
}

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
}

& $rustBin @CliArgs
if ($LASTEXITCODE -ne 0) {
    throw "agent-bus command failed (exit $LASTEXITCODE): $($CliArgs -join ' ')"
}
