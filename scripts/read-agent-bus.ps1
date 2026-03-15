param(
    [string]$Agent = "",
    [string]$From = "",
[int]$SinceMinutes = 1440,
[int]$Limit = 50,
[ValidateSet("json", "compact", "human")]
[string]$Encoding = "compact",
[switch]$ExcludeBroadcast,
[switch]$Json
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$invokeScript = Join-Path $scriptRoot "invoke-agent-bus-cli.ps1"

if ($Json) {
    $Encoding = "compact"
}

$cliEncoding = if ($Encoding -eq "human") { "compact" } else { $Encoding }

$arguments = @(
    "read",
    "--since-minutes", $SinceMinutes,
    "--limit", $Limit,
    "--encoding", $cliEncoding
)
if ($Agent) { $arguments += @("--agent", $Agent) }
if ($From) { $arguments += @("--from-agent", $From) }
if ($ExcludeBroadcast) { $arguments += "--exclude-broadcast" }

$jsonText = & $invokeScript @arguments
if ($Encoding -ne "human") {
    $jsonText
    exit 0
}

$messages = $jsonText | ConvertFrom-Json
if (-not $messages) {
    Write-Host "agent-bus-empty"
    exit 0
}

$messages | Select-Object timestamp_utc, from, to, topic, priority, request_ack, reply_to | Format-Table -AutoSize
