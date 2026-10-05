param(
    [string]$CliPath = (Join-Path $HOME "bin/agent-bus.exe"),
    [string]$HttpBinaryPath = (Join-Path $HOME "bin/agent-bus-http.exe"),
    [int]$HttpPort = 8410,
    [switch]$SkipCli,
    [switch]$SkipHttp,
    [switch]$SkipForcedDegraded,
    [switch]$RequirePostgres,
    [string]$HttpAuthToken,
    [string]$RedisUrl = $env:AGENT_BUS_REDIS_URL,
    [string]$DatabaseUrl = $env:AGENT_BUS_DATABASE_URL
)

$ErrorActionPreference = "Stop"
$cliSmokeScript = Join-Path $PSScriptRoot "test-agent-bus-cli-smoke.ps1"
$httpSmokeScript = Join-Path $PSScriptRoot "test-agent-bus-http-smoke.ps1"

function Assert-DisposableBackend {
    param([string]$Value, [string]$Name, [string[]]$Schemes)

    $target = $null
    if ([string]::IsNullOrWhiteSpace($Value) -or
        -not [uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$target) -or
        $target.Scheme -notin $Schemes -or
        $target.Host.Trim('[', ']') -notin @('localhost', '127.0.0.1', '::1') -or
        $target.Port -lt 1 -or $target.Port -in @(6380, 5300, 8400, 18400) -or
        $target.Query -or $target.Fragment) {
        throw "$Name must explicitly identify a disposable loopback backend on a non-live port"
    }
}
Assert-DisposableBackend -Value $RedisUrl -Name 'RedisUrl' -Schemes @('redis', 'rediss')
Assert-DisposableBackend -Value $DatabaseUrl -Name 'DatabaseUrl' -Schemes @('postgresql', 'postgres')
if ($HttpPort -lt 1024 -or $HttpPort -gt 65534 -or
    $HttpPort -in @(6380, 5300, 8400, 18400) -or
    (-not $SkipForcedDegraded -and ($HttpPort + 1) -in @(6380, 5300, 8400, 18400))) {
    throw 'HttpPort must identify a non-live unprivileged port for the disposable smoke server'
}

$environmentKeys = @('AGENT_BUS_CONFIG', 'AGENT_BUS_REDIS_URL', 'AGENT_BUS_DATABASE_URL',
    'AGENT_BUS_SERVER_HOST', 'AGENT_BUS_SERVER_URL', 'AGENT_BUS_SERVER_URLS',
    'AGENT_BUS_SERVER_CANDIDATES', 'AGENT_BUS_AUTH_TOKEN', 'AGENT_BUS_STARTUP_ENABLED',
    'AGENT_BUS_HUB_CACHE_TTL_SECONDS')
$originalEnvironment = @{}
foreach ($key in $environmentKeys) {
    $originalEnvironment[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
}
$directConfig = [IO.Path]::GetTempFileName()
try {
    [IO.File]::WriteAllText($directConfig, '{}')
    $env:AGENT_BUS_CONFIG = $directConfig
    $env:AGENT_BUS_REDIS_URL = $RedisUrl
    $env:AGENT_BUS_DATABASE_URL = $DatabaseUrl
    $env:AGENT_BUS_SERVER_HOST = 'localhost'
    $env:AGENT_BUS_STARTUP_ENABLED = 'false'
    $env:AGENT_BUS_HUB_CACHE_TTL_SECONDS = '0'
    foreach ($key in @('AGENT_BUS_SERVER_URL', 'AGENT_BUS_SERVER_URLS', 'AGENT_BUS_SERVER_CANDIDATES')) {
        Remove-Item -LiteralPath ('Env:' + $key) -ErrorAction SilentlyContinue
    }
    if ([string]::IsNullOrWhiteSpace($HttpAuthToken)) { $HttpAuthToken = [guid]::NewGuid().ToString('N') }
    $env:AGENT_BUS_AUTH_TOKEN = $HttpAuthToken

    function Write-SummaryLine {
        param([string]$Line)

        Write-Information $Line -InformationAction Continue
        if ($env:GITHUB_STEP_SUMMARY) {
            Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value $Line
        }
    }

    function Invoke-JsonHealth {
        param([string]$CommandPath)

        $json = & $CommandPath "health" "--encoding" "json"
        $healthExit = $LASTEXITCODE
        $health = $null
        try { $health = $json | ConvertFrom-Json -AsHashtable -ErrorAction Stop }
        catch { $health = $null }
        $validJson = $health -is [Collections.IDictionary]
        $validSchema = $validJson -and $health['ok'] -is [bool] -and
            $health['storage_ready'] -is [bool] -and $health.Contains('database_ok') -and
            ($null -eq $health['database_ok'] -or $health['database_ok'] -is [bool])
        if ($healthExit -ne 0 -or -not $validSchema) {
            # Health can contain endpoint URLs, credentials, and backend error
            # text. Preserve only typed booleans and a fixed backend mode.
            $values = @{}
            foreach ($field in @('ok', 'database_ok', 'storage_ready')) {
                $values[$field] = if ($validJson -and $health[$field] -is [bool]) { $health[$field].ToString().ToLowerInvariant() } else { 'unknown' }
            }
            $mode = 'unknown'
            if ($validJson -and $health['backend'] -is [Collections.IDictionary] -and
                $health['backend']['mode'] -in @('local', 'remote', 'offline')) { $mode = $health['backend']['mode'] }
            $jsonState = if ($validJson) { 'valid' } else { 'invalid' }
            throw "CLI health failed (exit=$healthExit; ok=$($values.ok); database_ok=$($values.database_ok); storage_ready=$($values.storage_ready); backend=$mode; json=$jsonState)."
        }

        return [pscustomobject]$health
    }

    function Invoke-WithDatabaseUrl {
        param(
            [string]$DatabaseUrl,
            [scriptblock]$Script
        )

        $originalDatabaseUrl = $env:AGENT_BUS_DATABASE_URL
        $env:AGENT_BUS_DATABASE_URL = $DatabaseUrl
        try {
            & $Script
        }
        finally {
            $env:AGENT_BUS_DATABASE_URL = $originalDatabaseUrl
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
}
finally {
    foreach ($key in $environmentKeys) {
        if ($null -eq $originalEnvironment[$key]) {
            Remove-Item -LiteralPath ('Env:' + $key) -ErrorAction SilentlyContinue
        }
        else {
            [Environment]::SetEnvironmentVariable($key, $originalEnvironment[$key], 'Process')
        }
    }
    Remove-Item -LiteralPath $directConfig -Force -ErrorAction Stop
}
