param(
    [Parameter(Mandatory = $true)][string]$Agent,
    [int]$History = 0,
    [ValidateSet("json", "compact", "human")]
    [string]$Encoding = "compact",
    [switch]$ExcludeBroadcast,
    [switch]$Notify
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$invokeScript = Join-Path $scriptRoot "invoke-agent-bus-cli.ps1"
$arguments = @(
    "watch",
    "--agent", $Agent,
    "--history", $History
)
if ($ExcludeBroadcast) { $arguments += "--exclude-broadcast" }
if ($Encoding -ne "json") {
    $arguments += @("--encoding", $Encoding)
}
else {
    $arguments += "--json"
}

Write-Host ("Watching agent bus for '{0}' ..." -f $Agent)

& $invokeScript @arguments 2>&1 | ForEach-Object {
    $line = $_.ToString()
    if ([string]::IsNullOrWhiteSpace($line)) {
        return
    }

    try {
        $event = $line | ConvertFrom-Json -ErrorAction Stop
        if ($Notify) {
            try {
                [console]::Beep(880, 120)
                [console]::Beep(1100, 120)
            }
            catch {
                Write-Host "`a"
            }
        }

        if ($Encoding -eq "human") {
            if ($event.event -eq "message") {
                $msg = $event.message
                Write-Host ("[{0}] {1} -> {2} | {3} | {4} | {5}" -f $msg.timestamp_utc, $msg.from, $msg.to, $msg.topic, $msg.priority, $msg.body)
            }
            elseif ($event.event -eq "presence") {
                $presence = $event.presence
                Write-Host ("[{0}] presence {1}={2} session={3}" -f $presence.timestamp_utc, $presence.agent, $presence.status, $presence.session_id)
            }
            else {
                Write-Host $line
            }
        }
        else {
            if ($Encoding -eq "json") {
                Write-Output ($event | ConvertTo-Json -Depth 16)
            }
            else {
                Write-Output ($event | ConvertTo-Json -Depth 16 -Compress)
            }
        }
    }
    catch {
        Write-Host $line
    }
}

if ($LASTEXITCODE -ne 0) {
    throw "watch-agent-bus failed"
}
