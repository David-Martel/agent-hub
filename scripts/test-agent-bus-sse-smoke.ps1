param(
    [string]$BaseUrl = "http://localhost:8400",
    [int]$TimeoutSeconds = 10,
    [string]$Agent = $null,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

if (-not $Agent -or [string]::IsNullOrWhiteSpace($Agent)) {
    $Agent = "sse-smoke-{0}" -f ([guid]::NewGuid().ToString("N").Substring(0, 8))
}

function New-TempFile {
    param([string]$Prefix, [string]$Extension)

    $name = "{0}-{1}{2}" -f $Prefix, ([guid]::NewGuid().ToString("N").Substring(0, 8)), $Extension
    return Join-Path $env:TEMP $name
}

function Get-AgentBusAuthToken {
    if (-not [string]::IsNullOrWhiteSpace($env:AGENT_BUS_AUTH_TOKEN)) {
        return $env:AGENT_BUS_AUTH_TOKEN
    }

    $configPath = Join-Path $HOME ".config/agent-bus/config.json"
    if (-not (Test-Path $configPath)) {
        return $null
    }

    try {
        $config = Get-Content -Path $configPath -Raw | ConvertFrom-Json
        if (-not [string]::IsNullOrWhiteSpace([string]$config.auth_token)) {
            return [string]$config.auth_token
        }
    }
    catch {
        Write-Warning "Could not read $configPath for AgentHub auth token. Continuing without token."
        Write-Warning $_.Exception.Message
    }
    return $null
}

$probeId = [guid]::NewGuid().ToString("N")
$probeBody = "SSE smoke probe $probeId"
$probeTopic = "sse-smoke"
$eventUrl = "{0}/events/{1}" -f $BaseUrl.TrimEnd("/"), $Agent
$messageUrl = "{0}/messages" -f $BaseUrl.TrimEnd("/")
$headersFile = New-TempFile -Prefix "agent-bus-sse-headers" -Extension ".txt"
$bodyFile = New-TempFile -Prefix "agent-bus-sse-body" -Extension ".txt"
$stderrFile = New-TempFile -Prefix "agent-bus-sse-stderr" -Extension ".txt"
$authToken = Get-AgentBusAuthToken

$curl = Get-Command curl.exe -ErrorAction SilentlyContinue
if (-not $curl) {
    $curl = Get-Command curl -ErrorAction SilentlyContinue
}
$curlPath = if ($curl) { $curl.Source } else { "<curl not found>" }

$args = @(
    "--silent"
    "--show-error"
    "--no-buffer"
    "--max-time", $TimeoutSeconds
    "--include"
    "--output", $bodyFile
    "--dump-header", $headersFile
    $eventUrl
)
if (-not [string]::IsNullOrWhiteSpace($authToken)) {
    $args = @("--header", "`"Authorization: Bearer $authToken`"") + $args
}

if ($DryRun) {
    $displayArgs = $args
    if (-not [string]::IsNullOrWhiteSpace($authToken)) {
        $displayArgs = $displayArgs | ForEach-Object { ([string]$_).Replace($authToken, "***") }
    }
    Write-Host "[DRY-RUN] SSE smoke plan:" -ForegroundColor Cyan
    Write-Host "  - Open SSE subscription: $curlPath $($displayArgs -join ' ')"
    Write-Host "  - POST probe message to: $messageUrl"
    Write-Host "  - Probe recipient: $Agent"
    Write-Host "  - Probe topic: $probeTopic"
    Write-Host "  - Temp files:"
    Write-Host "      $headersFile"
    Write-Host "      $bodyFile"
    Write-Host "      $stderrFile"
    Write-Host "  No curl process was started and no HTTP request was sent."
    exit 0
}

if (-not $curl) {
    throw "curl/curl.exe was not found on PATH"
}

Set-Content -Path $headersFile -Value "" -NoNewline
Set-Content -Path $bodyFile -Value "" -NoNewline
Set-Content -Path $stderrFile -Value "" -NoNewline

Write-Host "Opening SSE subscription for $Agent ..."
$proc = Start-Process -FilePath $curl.Source -ArgumentList $args -PassThru -NoNewWindow -RedirectStandardError $stderrFile

try {
    Start-Sleep -Milliseconds 600

    $payload = @{
        sender = "deploy-smoke"
        recipient = $Agent
        topic = $probeTopic
        body = $probeBody
    } | ConvertTo-Json -Compress

    $requestHeaders = @{}
    if (-not [string]::IsNullOrWhiteSpace($authToken)) {
        $requestHeaders["Authorization"] = "Bearer $authToken"
    }

    Invoke-RestMethod -Method Post -Uri $messageUrl -Headers $requestHeaders -ContentType "application/json" -Body $payload | Out-Null

    Start-Sleep -Seconds ([Math]::Max(2, [Math]::Min($TimeoutSeconds, 5)))

    if ($proc -and -not $proc.HasExited) {
        try {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        }
        catch {
        }
    }

    $headers = Get-Content -Path $headersFile -Raw
    if ($headers -notmatch "Content-Type:\s*text/event-stream") {
        throw "SSE response did not advertise text/event-stream. Headers: $headers"
    }

    $body = Get-Content -Path $bodyFile -Raw
    if ($body -notmatch [regex]::Escape($probeBody)) {
        throw "SSE response body did not contain probe payload"
    }

    Write-Host "SSE smoke ok: received '$probeBody' on $eventUrl"
}
finally {
    if ($proc -and -not $proc.HasExited) {
        try {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        }
        catch {
        }
    }
}
