#Requires -Version 7.0
<#
.SYNOPSIS
    Keep an SSH local forward open to an on-site agent-bus hub.

.DESCRIPTION
    Off the lab fabric, the on-site hub is often reachable only through an SSH
    jump host. This script starts `ssh -N -L <LocalPort>:<Target>` in the
    background when nothing answers on 127.0.0.1:<LocalPort>/health, then
    waits until the forwarded hub answers.

    Pair it with a hub candidate such as:
        {"url": "http://127.0.0.1:18480", "role": "authoritative",
         "hub": "<hub_identity>", "sites": ["<campus-site>"]}
    Set the same `hub` on the direct route; the client rejects a forward that
    reaches a hub reporting a different `hub_identity`.

    Any local process can connect to the forwarded port, and the client sends
    its on-site token to loopback candidates. This is the same posture as any
    loopback hub; do not run it on a shared multi-user machine.

.PARAMETER Jump
    ssh destination for the jump host: an ssh_config alias, or user@host.
.PARAMETER JumpPort
    Optional ssh port for the jump host.
.PARAMETER Target
    host:port of the hub as seen from the jump host.
.PARAMETER LocalPort
    Loopback port to listen on (default 18480; 18400 is the Windows NSSM local hub).
.PARAMETER TimeoutSeconds
    How long to wait for the forwarded /health (default 20).

.EXAMPLE
    ./scripts/agent-bus-tunnel.ps1 -Jump jump-alias -Target 10.0.0.1:8400
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^[A-Za-z0-9._@\[\]:/][A-Za-z0-9._@\[\]:/-]*$')][string]$Jump,
    [ValidateRange(1, 65535)][int]$JumpPort,
    [Parameter(Mandatory)][ValidatePattern('^[A-Za-z0-9.\-\[\]:]+:\d+$')][string]$Target,
    [ValidateRange(1, 65535)][int]$LocalPort = 18480,
    [ValidateRange(1, 300)][int]$TimeoutSeconds = 20
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$health = "http://127.0.0.1:$LocalPort/health"

function Test-Hub {
    try {
        $response = Invoke-WebRequest -Uri $health -TimeoutSec 3 -UseBasicParsing
        return $response.StatusCode -eq 200
    } catch {
        return $false
    }
}

if (Test-Hub) {
    Write-Output "agent-bus tunnel: $health already answers"
    exit 0
}

$ssh = (Get-Command ssh -ErrorAction Stop).Source
$arguments = @(
    '-N',
    '-o', 'BatchMode=yes',
    '-o', 'ExitOnForwardFailure=yes',
    '-o', 'ServerAliveInterval=30',
    '-o', 'ServerAliveCountMax=3',
    '-L', "127.0.0.1:${LocalPort}:$Target"
)
if ($JumpPort) { $arguments += @('-p', "$JumpPort") }
$arguments += @('--', $Jump)

$process = Start-Process -FilePath $ssh -ArgumentList $arguments -WindowStyle Hidden -PassThru
$ready = $false
try {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if ($process.HasExited) {
            throw "agent-bus tunnel: ssh exited with code $($process.ExitCode) before the forward answered"
        }
        if (Test-Hub) {
            $ready = $true
            Write-Output "agent-bus tunnel: $health answers through ssh pid $($process.Id)"
            exit 0
        }
        Start-Sleep -Milliseconds 500
    }
    throw "agent-bus tunnel: no /health answer on $health within $TimeoutSeconds s"
} finally {
    # Timeout, error or Ctrl+C: never leave a half-started forward behind.
    if (-not $ready) { Stop-Process -Id $process.Id -ErrorAction SilentlyContinue }
}
