param(
    [string]$BaseUrl = "http://localhost:8400",
    [int]$TimeoutSeconds = 10,
    [string]$Agent = $null
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

$probeId = [guid]::NewGuid().ToString("N")
$probeBody = "SSE smoke probe $probeId"
$probeTopic = "sse-smoke"
$eventUrl = "{0}/events/{1}" -f $BaseUrl.TrimEnd("/"), $Agent
$messageUrl = "{0}/messages" -f $BaseUrl.TrimEnd("/")
$headersFile = New-TempFile -Prefix "agent-bus-sse-headers" -Extension ".txt"
$bodyFile = New-TempFile -Prefix "agent-bus-sse-body" -Extension ".txt"
$stderrFile = New-TempFile -Prefix "agent-bus-sse-stderr" -Extension ".txt"

Set-Content -Path $headersFile -Value "" -NoNewline
Set-Content -Path $bodyFile -Value "" -NoNewline
Set-Content -Path $stderrFile -Value "" -NoNewline

$curl = Get-Command curl.exe -ErrorAction Stop
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

    Invoke-RestMethod -Method Post -Uri $messageUrl -ContentType "application/json" -Body $payload | Out-Null

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
