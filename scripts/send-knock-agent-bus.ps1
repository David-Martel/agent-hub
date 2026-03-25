param(
    [Parameter(Mandatory = $true)][string]$From,
    [Parameter(Mandatory = $true)][string]$To,
    [string]$Body = "check the bus",
    [string[]]$Tags = @(),
    [string]$ThreadId = "",
    [switch]$NoAck,
    [ValidateSet("json", "compact", "human")]
    [string]$Encoding = "compact"
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$invokeScript = Join-Path $scriptRoot "invoke-agent-bus-cli.ps1"

$arguments = @(
    "knock",
    "--from-agent", $From,
    "--to-agent", $To,
    "--body", $Body,
    "--encoding", $Encoding
)

foreach ($tag in $Tags) {
    if ($tag) { $arguments += @("--tag", $tag) }
}

if (-not $NoAck) {
    $arguments += "--request-ack"
}

if ($ThreadId) {
    $arguments += @("--thread-id", $ThreadId)
}

Write-Host ("Sending knock from '{0}' to '{1}' ..." -f $From, $To)
& $invokeScript @arguments
if ($LASTEXITCODE -ne 0) {
    throw "send-knock-agent-bus failed"
}
