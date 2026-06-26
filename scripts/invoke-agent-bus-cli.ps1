param(
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$CliArgs
)

$ErrorActionPreference = "Stop"

# Use the Rust binary directly (primary entry point).  This workstation installs
# active AgentHub binaries under ~/bin.  Some historical ~/.local/bin shims were
# zero-byte placeholders, so reject empty files instead of selecting them.
$candidateBins = @(
    (Join-Path $HOME "bin/agent-bus.exe"),
    (Join-Path $HOME ".local/bin/agent-bus.exe"),
    (Join-Path $PSScriptRoot "..\target\release\agent-bus.exe")
)

$rustBin = $null
foreach ($candidate in $candidateBins) {
    $item = Get-Item -LiteralPath $candidate -ErrorAction SilentlyContinue
    if ($item -and $item.Length -gt 0) {
        $rustBin = $item.FullName
        break
    }
}
if (-not $rustBin) {
    throw "agent-bus binary not found in: $($candidateBins -join ', '). Build with: cargo build --release"
}

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://127.0.0.1:6380/0"
}
if (-not $env:AGENT_BUS_DATABASE_URL) {
    $env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@127.0.0.1:5300/redis_backend"
}
if (-not $env:AGENT_BUS_SERVER_HOST) {
    $env:AGENT_BUS_SERVER_HOST = "localhost"
}
if (-not $env:AGENT_BUS_SERVER_URL -or $env:AGENT_BUS_SERVER_URL -eq "http://192.168.50.79:8400") {
    $env:AGENT_BUS_SERVER_URL = "http://localhost:8400"
}

& $rustBin @CliArgs
if ($LASTEXITCODE -ne 0) {
    throw "agent-bus command failed (exit $LASTEXITCODE): $($CliArgs -join ' ')"
}
