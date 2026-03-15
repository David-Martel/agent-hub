param(
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$CliArgs
)

$ErrorActionPreference = "Stop"
$toolRoot = Join-Path $HOME ".codex/tools/agent-bus-mcp"
$venvPython = Join-Path $toolRoot ".venv/Scripts/python.exe"

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
}
if (-not $env:AGENT_BUS_DATABASE_URL) {
    $env:AGENT_BUS_DATABASE_URL = "postgresql://postgres:password@localhost:5300/postgres"
}

if (Test-Path $venvPython) {
    & $venvPython -m agent_bus_mcp.cli @CliArgs
    if ($LASTEXITCODE -eq 0) {
        exit 0
    }
    Write-Warning "In-place agent-bus runtime failed; retrying with isolated uv runtime."
}

& uv --directory $toolRoot run --isolated agent-bus-mcp @CliArgs
if ($LASTEXITCODE -ne 0) {
    throw "agent-bus command failed: $($CliArgs -join ' ')"
}
