param(
    [switch]$SkipHealth
)

$ErrorActionPreference = "Stop"
$toolRoot = Join-Path $HOME ".codex/tools/agent-bus-mcp"
$useIsolated = $false

Write-Host "Syncing uv environment..."
& uv --directory $toolRoot sync --frozen --group dev
if ($LASTEXITCODE -ne 0) {
    Write-Warning "uv sync hit a Windows file lock. Falling back to an isolated uv run for validation."
    $useIsolated = $true
}

Write-Host "Running pytest validation..."
if ($useIsolated) {
    & uv --directory $toolRoot run --isolated --group dev pytest --cov=agent_bus_mcp --cov-report=term-missing
}
else {
    & uv --directory $toolRoot run pytest --cov=agent_bus_mcp --cov-report=term-missing
}
if ($LASTEXITCODE -ne 0) {
    if (-not $useIsolated) {
        Write-Warning "In-place pytest validation failed. Retrying in an isolated uv environment."
        & uv --directory $toolRoot run --isolated --group dev pytest --cov=agent_bus_mcp --cov-report=term-missing
    }
    if ($LASTEXITCODE -ne 0) {
        throw "Pytest validation failed."
    }
}

if (-not $SkipHealth) {
    Write-Host "Running live health validation..."
    if ($useIsolated) {
        $code = @'
from agent_bus_mcp.bus import AgentBus
import json
bus = AgentBus.from_env()
try:
    print(json.dumps(bus.initialize(announce_startup=True), indent=2))
finally:
    bus.close()
'@
        & uv --directory $toolRoot run --isolated python -c $code
    }
    else {
        & uv --directory $toolRoot run agent-bus-mcp health
    }
    if ($LASTEXITCODE -ne 0) {
        if (-not $useIsolated) {
            Write-Warning "In-place health validation failed. Retrying in an isolated uv environment."
            $code = @'
from agent_bus_mcp.bus import AgentBus
import json
bus = AgentBus.from_env()
try:
    print(json.dumps(bus.initialize(announce_startup=True), indent=2))
finally:
    bus.close()
'@
            & uv --directory $toolRoot run --isolated python -c $code
        }
        if ($LASTEXITCODE -ne 0) {
            throw "Health validation failed."
        }
    }
}
