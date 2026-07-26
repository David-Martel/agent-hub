param(
    [string]$CliPath = (Join-Path $HOME "bin/agent-bus.exe"),
    [string]$HttpBinaryPath = (Join-Path $HOME "bin/agent-bus-http.exe"),
    [int]$HttpPort = 8410,
    [switch]$SkipCli,
    [switch]$SkipHttp,
    [switch]$SkipForcedDegraded,
    [switch]$RequirePostgres,
    [string]$HttpAuthToken = $env:AGENT_BUS_AUTH_TOKEN
)

$ErrorActionPreference = "Stop"
$cliSmokeScript = Join-Path $PSScriptRoot "test-agent-bus-cli-smoke.ps1"
$httpSmokeScript = Join-Path $PSScriptRoot "test-agent-bus-http-smoke.ps1"

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://127.0.0.1:6380/0"
}
if (-not $env:AGENT_BUS_DATABASE_URL) {
    $env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@127.0.0.1:5300/redis_backend"
}
if (-not $env:AGENT_BUS_SERVER_HOST) {
    $env:AGENT_BUS_SERVER_HOST = "localhost"
}
if ([string]::IsNullOrWhiteSpace($HttpAuthToken)) {
    $clientConfigPath = Join-Path $HOME ".config/agent-bus/config.json"
    if (Test-Path $clientConfigPath) {
        try {
            $clientConfig = Get-Content -LiteralPath $clientConfigPath -Raw |
                ConvertFrom-Json -Depth 100
            $HttpAuthToken = [string]$clientConfig.auth_token
        }
        catch {
            Write-Verbose "Agent-bus client config could not be parsed for HTTP smoke authentication."
        }
    }
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

    $originalDatabaseUrl = $env:AGENT_BUS_DATABASE_URL
    $originalServerUrl = $env:AGENT_BUS_SERVER_URL
    $originalConfig = $env:AGENT_BUS_CONFIG
    $directConfig = [System.IO.Path]::GetTempFileName()
    [System.IO.File]::WriteAllText($directConfig, "{}")
    $env:AGENT_BUS_DATABASE_URL = $DatabaseUrl
    $env:AGENT_BUS_CONFIG = $directConfig
    Remove-Item Env:AGENT_BUS_SERVER_URL -ErrorAction SilentlyContinue
    try {
        & $Script
    }
    finally {
        $env:AGENT_BUS_DATABASE_URL = $originalDatabaseUrl
        if ($null -eq $originalServerUrl) {
            Remove-Item Env:AGENT_BUS_SERVER_URL -ErrorAction SilentlyContinue
        }
        else {
            $env:AGENT_BUS_SERVER_URL = $originalServerUrl
        }
        if ($null -eq $originalConfig) {
            Remove-Item Env:AGENT_BUS_CONFIG -ErrorAction SilentlyContinue
        }
        else {
            $env:AGENT_BUS_CONFIG = $originalConfig
        }
        Remove-Item -LiteralPath $directConfig -Force -ErrorAction SilentlyContinue
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
    & $httpSmokeScript -BinaryPath $HttpBinaryPath -BaseUrl "http://localhost:$HttpPort" -Port $HttpPort -DatabaseMode $steadyState -AuthToken $HttpAuthToken
}

if (-not $SkipForcedDegraded) {
    # Use an explicit loopback address so the forced outage is deterministic
    # and does not multiply connection retries across IPv4/IPv6 candidates.
    $forcedDatabaseUrl = "postgresql://postgres@127.0.0.1:1/redis_backend"
    Write-SummaryLine "- Forced degraded PostgreSQL smoke: enabled"

    if (-not $SkipCli) {
        Invoke-WithDatabaseUrl -DatabaseUrl $forcedDatabaseUrl -Script {
            & $cliSmokeScript -CliPath $CliPath -DatabaseMode "Degraded"
        }
    }

    if (-not $SkipHttp) {
        Invoke-WithDatabaseUrl -DatabaseUrl $forcedDatabaseUrl -Script {
            & $httpSmokeScript `
                -BinaryPath $HttpBinaryPath `
                -BaseUrl "http://localhost:$($HttpPort + 1)" `
                -Port ($HttpPort + 1) `
                -DatabaseMode "Degraded" `
                -StartupTimeoutSeconds 90 `
                -AuthToken $HttpAuthToken
        }
    }
}
else {
    Write-SummaryLine "- Forced degraded PostgreSQL smoke: skipped"
}

Write-SummaryLine "- Functional smoke result: success"
