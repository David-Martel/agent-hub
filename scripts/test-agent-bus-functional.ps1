param(
    [string]$CliPath = (Join-Path $HOME "bin/agent-bus.exe"),
    [string]$HttpBinaryPath = (Join-Path $HOME "bin/agent-bus-http.exe"),
    [int]$HttpPort = 8410,
    [switch]$SkipCli,
    [switch]$SkipHttp,
    [switch]$SkipForcedDegraded,
    [switch]$RequirePostgres
)

$ErrorActionPreference = "Stop"
$cliSmokeScript = Join-Path $PSScriptRoot "test-agent-bus-cli-smoke.ps1"
$httpSmokeScript = Join-Path $PSScriptRoot "test-agent-bus-http-smoke.ps1"

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
}
if (-not $env:AGENT_BUS_DATABASE_URL) {
    $env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@localhost:5300/redis_backend"
}
if (-not $env:AGENT_BUS_SERVER_HOST) {
    $env:AGENT_BUS_SERVER_HOST = "localhost"
}

function Write-SummaryLine {
    param([string]$Line)

    Write-Host $Line
    if ($env:GITHUB_STEP_SUMMARY) {
        Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value $Line
    }
}

function Invoke-JsonHealth {
    param([string]$CommandPath)

    $json = & $CommandPath "health" "--encoding" "json"
    if ($LASTEXITCODE -ne 0) {
        throw "health failed for $CommandPath"
    }

    return $json | ConvertFrom-Json
}

function Invoke-WithDatabaseUrl {
    param(
        [string]$DatabaseUrl,
        [scriptblock]$Script
    )

    $original = $env:AGENT_BUS_DATABASE_URL
    $env:AGENT_BUS_DATABASE_URL = $DatabaseUrl
    try {
        & $Script
    }
    finally {
        $env:AGENT_BUS_DATABASE_URL = $original
    }
}

if (-not (Test-Path $CliPath)) {
    throw "agent-bus CLI not found at $CliPath"
}
if (-not (Test-Path $HttpBinaryPath)) {
    throw "agent-bus HTTP binary not found at $HttpBinaryPath"
}

$Health = Invoke-JsonHealth -CommandPath $CliPath
if (-not $Health.ok) {
    throw "Redis is required for functional smoke tests. health.ok=false for $CliPath"
}
if ($RequirePostgres -and -not $Health.database_ok) {
    throw "PostgreSQL is required for this run but database_ok=false"
}

$steadyState = if ($Health.database_ok) { "Healthy" } else { "Degraded" }
Write-SummaryLine "### Agent Bus Functional Smoke"
Write-SummaryLine "- Redis available: True"
Write-SummaryLine "- PostgreSQL available: $($Health.database_ok)"
Write-SummaryLine "- Normal database mode: $steadyState"
Write-SummaryLine "- CLI binary: $CliPath"
Write-SummaryLine "- HTTP binary: $HttpBinaryPath"

if (-not $SkipCli) {
    & $cliSmokeScript -CliPath $CliPath -DatabaseMode $steadyState
}

if (-not $SkipHttp) {
    & $httpSmokeScript -BinaryPath $HttpBinaryPath -BaseUrl "http://localhost:$HttpPort" -Port $HttpPort -DatabaseMode $steadyState
}

if (-not $SkipForcedDegraded) {
    $forcedDatabaseUrl = "postgresql://postgres@localhost:1/redis_backend"
    Write-SummaryLine "- Forced degraded PostgreSQL smoke: enabled"

    if (-not $SkipCli) {
        Invoke-WithDatabaseUrl -DatabaseUrl $forcedDatabaseUrl -Script {
            & $cliSmokeScript -CliPath $CliPath -DatabaseMode "Degraded"
        }
    }

    if (-not $SkipHttp) {
        Invoke-WithDatabaseUrl -DatabaseUrl $forcedDatabaseUrl -Script {
            & $httpSmokeScript -BinaryPath $HttpBinaryPath -BaseUrl "http://localhost:$($HttpPort + 1)" -Port ($HttpPort + 1) -DatabaseMode "Degraded"
        }
    }
}
else {
    Write-SummaryLine "- Forced degraded PostgreSQL smoke: skipped"
}

Write-SummaryLine "- Functional smoke result: success"
