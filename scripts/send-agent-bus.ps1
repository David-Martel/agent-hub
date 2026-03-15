param(
    [Parameter(Mandatory = $true)][string]$From,
    [Parameter(Mandatory = $true)][string]$To,
    [Parameter(Mandatory = $true)][string]$Topic,
    [Parameter(Mandatory = $true)][string]$Body,
    [ValidateSet("json", "compact", "human")]
    [string]$Encoding = "compact",
    [string[]]$Tags = @(),
    [string]$ThreadId = "",
    [ValidateSet("low", "normal", "high", "urgent")]
    [string]$Priority = "normal",
    [switch]$RequestAck,
    [string]$ReplyTo = "",
    [string]$Metadata = "{}"
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$invokeScript = Join-Path $scriptRoot "invoke-agent-bus-cli.ps1"
$arguments = @(
    "send",
    "--from-agent", $From,
    "--to-agent", $To,
    "--topic", $Topic,
    "--body", $Body,
    "--encoding", $Encoding,
    "--priority", $Priority,
    "--metadata", $Metadata
)
foreach ($tag in $Tags) {
    if ($tag) { $arguments += @("--tag", $tag) }
}
if ($RequestAck) { $arguments += "--request-ack" }
if ($ReplyTo) { $arguments += @("--reply-to", $ReplyTo) }
if ($ThreadId) { $arguments += @("--thread-id", $ThreadId) }

& $invokeScript @arguments
if ($LASTEXITCODE -ne 0) {
    throw "send-agent-bus failed"
}
