param(
    [string]$BaseUrl = $env:AGENT_BUS_SERVER_URL,
    [string]$Agent = "codex-remote-smoke",
    [string]$AuthToken = $env:AGENT_BUS_AUTH_TOKEN,
    [int]$TimeoutSeconds = 10,
    [switch]$SkipSend,
    [switch]$Strict
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($BaseUrl)) {
    $BaseUrl = "http://localhost:8400"
}

$results = New-Object System.Collections.Generic.List[object]

function Add-Result {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Status,
        [Parameter(Mandatory = $true)][string]$Detail
    )
    $results.Add([pscustomobject]@{
            name   = $Name
            status = $Status
            detail = $Detail
        }) | Out-Null
}

function Invoke-AgentBus {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)

    $oldServer = $env:AGENT_BUS_SERVER_URL
    $oldToken = $env:AGENT_BUS_AUTH_TOKEN
    try {
        $env:AGENT_BUS_SERVER_URL = $BaseUrl
        if (-not [string]::IsNullOrWhiteSpace($AuthToken)) {
            $env:AGENT_BUS_AUTH_TOKEN = $AuthToken
        }
        $output = & agent-bus @Arguments 2>&1
        return [pscustomobject]@{
            exitCode = $LASTEXITCODE
            output   = ($output -join "`n")
        }
    }
    finally {
        $env:AGENT_BUS_SERVER_URL = $oldServer
        $env:AGENT_BUS_AUTH_TOKEN = $oldToken
    }
}

try {
    $uri = [Uri]$BaseUrl
    $healthUri = [Uri]::new($uri, "/health")
    $response = Invoke-WebRequest -Uri $healthUri -Method Get -TimeoutSec $TimeoutSeconds -UseBasicParsing
    Add-Result -Name "http:health" -Status "ok" -Detail "HTTP $($response.StatusCode)"
}
catch {
    Add-Result -Name "http:health" -Status "fail" -Detail $_.Exception.Message
}

foreach ($step in @(
        @{ name = "cli:health"; args = @("health", "--encoding", "compact") },
        @{ name = "cli:presence"; args = @("presence", "--agent", $Agent, "--status", "online", "--capability", "remote-smoke", "--ttl-seconds", "120", "--encoding", "compact") },
        @{ name = "cli:read"; args = @("read", "--agent", $Agent, "--since-minutes", "5", "--limit", "5", "--encoding", "compact") }
    )) {
    try {
        $result = Invoke-AgentBus -Arguments $step.args
        if ($result.exitCode -eq 0) {
            Add-Result -Name $step.name -Status "ok" -Detail "exit=0"
        }
        else {
            Add-Result -Name $step.name -Status "fail" -Detail "exit=$($result.exitCode)"
        }
    }
    catch {
        Add-Result -Name $step.name -Status "fail" -Detail $_.Exception.Message
    }
}

if (-not $SkipSend) {
    try {
        $result = Invoke-AgentBus -Arguments @(
            "send",
            "--from-agent", $Agent,
            "--to-agent", "all",
            "--topic", "remote-smoke",
            "--body", "agent-bus remote smoke test",
            "--tag", "type:smoke",
            "--encoding", "compact"
        )
        if ($result.exitCode -eq 0) {
            Add-Result -Name "cli:send" -Status "ok" -Detail "exit=0"
        }
        else {
            Add-Result -Name "cli:send" -Status "fail" -Detail "exit=$($result.exitCode)"
        }
    }
    catch {
        Add-Result -Name "cli:send" -Status "fail" -Detail $_.Exception.Message
    }
}

$results | Format-Table -AutoSize
$failures = @($results | Where-Object { $_.status -eq "fail" })
if ($failures.Count -gt 0 -and $Strict) {
    throw "agent-bus remote smoke failed $($failures.Count) check(s) for $BaseUrl"
}
if ($failures.Count -gt 0) {
    exit 1
}
