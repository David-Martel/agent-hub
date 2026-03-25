param(
    [string]$CliPath = (Join-Path $HOME "bin/agent-bus.exe"),
    [ValidateSet("Any", "Healthy", "Degraded")]
    [string]$DatabaseMode = "Any",
    [string]$Agent = $null,
    [int]$TimeoutMilliseconds = 600
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $CliPath)) {
    throw "agent-bus CLI not found at $CliPath"
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

function Invoke-AgentBusJson {
    param([string[]]$CommandArgs)

    $json = & $CliPath @CommandArgs
    if ($LASTEXITCODE -ne 0) {
        throw "agent-bus command failed: $($CommandArgs -join ' ')"
    }

    return $json | ConvertFrom-Json
}

if (-not $Agent -or [string]::IsNullOrWhiteSpace($Agent)) {
    $Agent = "cli-smoke-{0}" -f ([guid]::NewGuid().ToString("N").Substring(0, 8))
}

$Sender = "$Agent-sender"
$ProbeId = [guid]::NewGuid().ToString("N")
$ProbeBody = "CLI smoke probe $ProbeId"
$Health = Invoke-AgentBusJson -CommandArgs @("health", "--encoding", "json")

if (-not $Health.ok) {
    throw "CLI health check failed. Redis is required for smoke tests."
}

switch ($DatabaseMode) {
    "Healthy" {
        if (-not $Health.database_ok) {
            throw "Expected PostgreSQL to be healthy, but database_ok=false"
        }
    }
    "Degraded" {
        if ($Health.database_ok) {
            throw "Expected PostgreSQL degraded mode, but database_ok=true"
        }
    }
}

& $CliPath "presence" "--agent" $Agent "--status" "online" "--capability" "smoke" "--ttl-seconds" "120" | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "presence update failed for $Agent"
}

& $CliPath "send" "--from-agent" $Sender "--to-agent" $Agent "--topic" "cli-smoke" "--body" $ProbeBody "--tag" "repo:agent-bus" "--tag" "type:smoke" "--tag" "transport:cli" "--schema" "status" | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "send failed for CLI smoke probe"
}

Start-Sleep -Milliseconds $TimeoutMilliseconds
$Messages = Invoke-AgentBusJson -CommandArgs @("read", "--agent", $Agent, "--since-minutes", "10", "--limit", "20", "--encoding", "json")
$Match = $Messages | Where-Object { $_.body -eq $ProbeBody } | Select-Object -First 1

if (-not $Match) {
    throw "CLI smoke probe was not returned by read --agent $Agent"
}

Write-Host "CLI smoke ok: sender=$Sender recipient=$Agent database_ok=$($Health.database_ok)"
