#!/usr/bin/env pwsh
# Cross-MACHINE reachability test for the agent-bus HTTP coordinator.
#
# Unlike `cross-validate.ps1` (which is a cross-PLATFORM Docker compile check),
# this script verifies that the running `agent-bus-http` server on one or more
# remote hosts is actually reachable over the network from THIS machine:
#   1. DNS / mDNS name resolution
#   2. TCP reachability on the HTTP port
#   3. Application-layer liveness via the public GET /health endpoint (expects 200)
#   4. Auth posture: an unauthenticated GET /messages should return 401 on a
#      remote-exposed hub (AGENT_BUS_ALLOW_REMOTE=true); 200 means the endpoint
#      is unauthenticated (expected only for a loopback/dev bus)
#   5. (optional) Authenticated access with a bearer token
#
# This is the machine-to-machine check the vigilnet integration plan lists as a
# TODO ("extend spark-preflight with agent-bus endpoint/CLI/presence checks").
#
# Usage:
#   pwsh -NoLogo -NoProfile -File scripts/cross-machine-health.ps1
#   pwsh -NoLogo -NoProfile -File scripts/cross-machine-health.ps1 -Hosts asuspro13.local,spark-0060.local
#   pwsh -NoLogo -NoProfile -File scripts/cross-machine-health.ps1 -Strict   # exit 1 if any host's /health != 200
#   $env:AGENT_BUS_AUTH_TOKEN='...'; pwsh ... -CheckAuth   # also test authenticated /messages
#
# Exit codes: 0 = all probed hosts healthy (or non-strict); 1 = a required host failed (-Strict).

[CmdletBinding()]
param(
    # Default host set mirrors the vigil-net topology. Override for other fleets.
    [string[]]$Hosts = @('asuspro13.local', 'spark-0060.local', 'spark-3066.local'),
    [int]$Port = 8400,
    [string]$Token = $env:AGENT_BUS_AUTH_TOKEN,
    [switch]$CheckAuth,
    [switch]$Strict,
    [int]$TimeoutSec = 5
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Continue'

function Test-Endpoint {
    param([string]$Url, [hashtable]$Headers)
    try {
        $params = @{ Uri = $Url; TimeoutSec = $TimeoutSec; UseBasicParsing = $true }
        if ($Headers) { $params.Headers = $Headers }
        $resp = Invoke-WebRequest @params
        return [pscustomobject]@{ Code = [int]$resp.StatusCode; Body = $resp.Content }
    }
    catch {
        $code = 0
        if ($_.Exception.PSObject.Properties.Name -contains 'Response' -and $_.Exception.Response) {
            try { $code = [int]$_.Exception.Response.StatusCode.value__ } catch { $code = -1 }
        }
        return [pscustomobject]@{ Code = $code; Body = $_.Exception.Message }
    }
}

$results = @()
$anyFailure = $false

foreach ($h in $Hosts) {
    $dns = 'UNRESOLVED'
    try {
        $rec = Resolve-DnsName -Name $h -ErrorAction Stop | Where-Object { $_.IPAddress } | Select-Object -First 1
        if ($rec) { $dns = $rec.IPAddress }
    }
    catch { $dns = 'UNRESOLVED' }

    $tcp = Test-NetConnection -ComputerName $h -Port $Port -WarningAction SilentlyContinue -InformationLevel Quiet

    $healthCode = 'n/a'
    $dbOk = 'n/a'
    $authPosture = 'n/a'
    if ($tcp) {
        $health = Test-Endpoint -Url "http://${h}:${Port}/health"
        $healthCode = $health.Code
        if ($health.Code -eq 200 -and $health.Body) {
            try { $dbOk = ([bool]((ConvertFrom-Json $health.Body).database_ok)) } catch { $dbOk = '?' }
        }
        # Probe an authenticated route without a token to read the auth posture.
        $unauth = Test-Endpoint -Url "http://${h}:${Port}/messages"
        $authPosture = switch ($unauth.Code) {
            401 { 'enforced' }
            200 { 'OPEN' }
            default { "http $($unauth.Code)" }
        }
        if ($CheckAuth -and $Token) {
            $withTok = Test-Endpoint -Url "http://${h}:${Port}/messages" -Headers @{ Authorization = "Bearer $Token" }
            $authPosture += " (token->$($withTok.Code))"
        }
    }

    $healthy = ($tcp -and $healthCode -eq 200)
    if (-not $healthy) { $anyFailure = $true }

    $results += [pscustomobject]@{
        Host    = $h
        DNS     = $dns
        TCP     = if ($tcp) { 'up' } else { 'DOWN' }
        Health  = $healthCode
        DbOk    = $dbOk
        Auth    = $authPosture
        Healthy = if ($healthy) { 'YES' } else { 'NO' }
    }
}

$results | Format-Table -AutoSize | Out-String | Write-Output

Write-Output "Probed $($Hosts.Count) host(s) from $(hostname) on port $Port."
if ($anyFailure) {
    Write-Warning "One or more hosts are not serving /health (200). A host bound loopback-only (::1/127.0.0.1) is not reachable cross-machine; the hub must set AGENT_BUS_SERVER_HOST=0.0.0.0 + AGENT_BUS_ALLOW_REMOTE=true + AGENT_BUS_AUTH_TOKEN."
    if ($Strict) { exit 1 }
}
else {
    Write-Output "All probed hosts are healthy."
}
exit 0
