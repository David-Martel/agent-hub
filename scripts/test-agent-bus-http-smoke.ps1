param(
    [string]$BinaryPath = (Join-Path $HOME "bin/agent-bus-http.exe"),
    [string]$BaseUrl = "http://localhost:8410",
    [int]$Port = 8410,
    [ValidateSet("Any", "Healthy", "Degraded")]
    [string]$DatabaseMode = "Any",
    [int]$StartupTimeoutSeconds = 30,
    [int]$TimeoutSeconds = 10
)

$ErrorActionPreference = "Stop"
$sseSmokeScript = Join-Path $PSScriptRoot "test-agent-bus-sse-smoke.ps1"

if (-not (Test-Path $sseSmokeScript)) {
    throw "SSE smoke script not found at $sseSmokeScript"
}

if (-not (Test-Path $BinaryPath)) {
    throw "HTTP binary not found at $BinaryPath"
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

function New-TempFile {
    param([string]$Prefix, [string]$Extension)

    $name = "{0}-{1}{2}" -f $Prefix, ([guid]::NewGuid().ToString("N").Substring(0, 8)), $Extension
    return Join-Path $env:TEMP $name
}

function Wait-ForHealth {
    param(
        [string]$Url,
        [System.Diagnostics.Process]$Process,
        [int]$TimeoutSeconds,
        [string]$StdoutPath,
        [string]$StderrPath
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if ($Process.HasExited) {
            $stdout = if (Test-Path $StdoutPath) { Get-Content -Path $StdoutPath -Raw } else { "" }
            $stderr = if (Test-Path $StderrPath) { Get-Content -Path $StderrPath -Raw } else { "" }
            throw "HTTP server exited before health check succeeded.`nstdout:`n$stdout`nstderr:`n$stderr"
        }

        try {
            return Invoke-RestMethod -Uri "$Url/health" -Method Get -TimeoutSec 15
        }
        catch {
            Start-Sleep -Milliseconds 400
        }
    } while ((Get-Date) -lt $deadline)

    $stdout = if (Test-Path $StdoutPath) { Get-Content -Path $StdoutPath -Raw } else { "" }
    $stderr = if (Test-Path $StderrPath) { Get-Content -Path $StderrPath -Raw } else { "" }
    throw "Timed out waiting for HTTP health at $Url/health.`nstdout:`n$stdout`nstderr:`n$stderr"
}

$Agent = "http-smoke-{0}" -f ([guid]::NewGuid().ToString("N").Substring(0, 8))
$Sender = "$Agent-sender"
$ProbeId = [guid]::NewGuid().ToString("N")
$ProbeBody = "HTTP smoke probe $ProbeId"
$StdoutPath = New-TempFile -Prefix "agent-bus-http-stdout" -Extension ".log"
$StderrPath = New-TempFile -Prefix "agent-bus-http-stderr" -Extension ".log"

$Process = Start-Process `
    -FilePath $BinaryPath `
    -ArgumentList @("serve", "--transport", "http", "--port", $Port.ToString()) `
    -PassThru `
    -WindowStyle Hidden `
    -RedirectStandardOutput $StdoutPath `
    -RedirectStandardError $StderrPath `
    -Environment @{
        AGENT_BUS_REDIS_URL = $env:AGENT_BUS_REDIS_URL
        AGENT_BUS_DATABASE_URL = $env:AGENT_BUS_DATABASE_URL
        AGENT_BUS_SERVER_HOST = $env:AGENT_BUS_SERVER_HOST
        RUST_LOG = "error"
    }

try {
    $Health = Wait-ForHealth -Url $BaseUrl.TrimEnd("/") -Process $Process -TimeoutSeconds $StartupTimeoutSeconds -StdoutPath $StdoutPath -StderrPath $StderrPath

    if (-not $Health.ok) {
        throw "HTTP health check failed. Redis is required for HTTP smoke tests."
    }

    switch ($DatabaseMode) {
        "Healthy" {
            if (-not $Health.database_ok) {
                throw "Expected PostgreSQL healthy mode, but database_ok=false"
            }
        }
        "Degraded" {
            if ($Health.database_ok) {
                throw "Expected PostgreSQL degraded mode, but database_ok=true"
            }
        }
    }

    $MessageUrl = "{0}/messages" -f $BaseUrl.TrimEnd("/")
    $ReadUrl = "{0}/messages?agent={1}&since=10&limit=20" -f $BaseUrl.TrimEnd("/"), $Agent
    $NotificationsUrl = "{0}/notifications/{1}?history=20" -f $BaseUrl.TrimEnd("/"), $Agent
    $Payload = @{
        sender = $Sender
        recipient = $Agent
        topic = "http-smoke"
        body = $ProbeBody
        tags = @("repo:agent-bus", "type:smoke", "transport:http")
        schema = "status"
    } | ConvertTo-Json -Compress

    Invoke-RestMethod -Method Post -Uri $MessageUrl -ContentType "application/json" -Body $Payload | Out-Null
    Start-Sleep -Milliseconds 600

    $Messages = Invoke-RestMethod -Method Get -Uri $ReadUrl -TimeoutSec 15
    $MessageMatch = $Messages | Where-Object { $_.body -eq $ProbeBody } | Select-Object -First 1
    if (-not $MessageMatch) {
        throw "HTTP read path did not return the smoke probe for $Agent"
    }

    $Notifications = Invoke-RestMethod -Method Get -Uri $NotificationsUrl -TimeoutSec 15
    $NotificationMatch = $Notifications | Where-Object { $_.message.body -eq $ProbeBody -or $_.body -eq $ProbeBody } | Select-Object -First 1
    if (-not $NotificationMatch) {
        throw "HTTP replay path did not return the smoke probe for $Agent"
    }

    & $sseSmokeScript -BaseUrl $BaseUrl -TimeoutSeconds $TimeoutSeconds -Agent $Agent
    if ($LASTEXITCODE -ne 0) {
        throw "SSE smoke sub-test failed for $BaseUrl"
    }

    Write-Host "HTTP smoke ok: base_url=$BaseUrl agent=$Agent database_ok=$($Health.database_ok)"
}
finally {
    if ($Process -and -not $Process.HasExited) {
        try {
            Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
        }
        catch {
        }
    }
}
