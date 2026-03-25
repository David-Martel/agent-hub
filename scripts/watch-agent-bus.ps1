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

function Write-KnockNotice {
    param(
        [string]$Text
    )

    try {
        $prevFg = $Host.UI.RawUI.ForegroundColor
        $prevBg = $Host.UI.RawUI.BackgroundColor
        $Host.UI.RawUI.ForegroundColor = "Black"
        $Host.UI.RawUI.BackgroundColor = "Yellow"
        Write-Host $Text
        $Host.UI.RawUI.ForegroundColor = $prevFg
        $Host.UI.RawUI.BackgroundColor = $prevBg
    }
    catch {
        Write-Host $Text
    }
}

function Test-IsKnockEvent {
    param(
        $Event
    )

    if ($null -eq $Event) {
        return $false
    }

    if ($Event.event -eq "notification" -and $Event.notification.reason -eq "knock") {
        return $true
    }

    if ($Event.event -eq "message" -and $Event.message.topic -eq "knock") {
        return $true
    }

    return $false
}

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
    $arguments += @("--encoding", "json")
}

Write-Host ("Watching agent bus for '{0}' ..." -f $Agent)

& $invokeScript @arguments 2>&1 | ForEach-Object {
    $line = $_.ToString()
    if ([string]::IsNullOrWhiteSpace($line)) {
        return
    }

    try {
        $event = $line | ConvertFrom-Json -ErrorAction Stop
        $isKnock = Test-IsKnockEvent -Event $event
        if ($Notify) {
            try {
                if ($isKnock) {
                    [console]::Beep(1200, 150)
                    [console]::Beep(1600, 180)
                }
                else {
                    [console]::Beep(880, 120)
                    [console]::Beep(1100, 120)
                }
            }
            catch {
                Write-Host "`a"
            }
        }

        if ($Encoding -eq "human") {
            if ($event.event -eq "message") {
                $msg = $event.message
                $prefix = if ($isKnock) { "[KNOCK]" } else { "[MESSAGE]" }
                $text = ("{0} [{1}] {2} -> {3} | {4} | {5} | {6}" -f $prefix, $msg.timestamp_utc, $msg.from, $msg.to, $msg.topic, $msg.priority, $msg.body)
                if ($isKnock) {
                    Write-KnockNotice $text
                }
                else {
                    Write-Host $text
                }
            }
            elseif ($event.event -eq "presence") {
                $presence = $event.presence
                Write-Host ("[{0}] presence {1}={2} session={3}" -f $presence.timestamp_utc, $presence.agent, $presence.status, $presence.session_id)
            }
            elseif ($isKnock) {
                Write-KnockNotice ("[KNOCK] {0}" -f $line)
            }
            else {
                Write-Host $line
            }
        }
        else {
            if ($isKnock) {
                Write-KnockNotice ("[KNOCK] {0}" -f $line)
            }
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
