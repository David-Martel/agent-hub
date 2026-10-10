#Requires -Version 7.0
<#
.SYNOPSIS
Forwards cloud-token operations to the canonical Rust CLI.
.DESCRIPTION
All private file writes, complete-map preservation, recovery verification and
HTTP failure handling live in agent-bus cloud-tokens. Unlock Bitwarden using
this host's supported secret tooling before bw-upsert or wrangler-put.
#>
[CmdletBinding()]
param(
    [ValidateSet('init-manifest','mint','rotate','activate','write-client','bw-upsert','wrangler-put','revoke','wrangler-hint','smoke','status')]
    [string]$Action = 'status',
    [string]$Manifest = '',
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
    [string]$AgentBusPath = 'agent-bus',
    [switch]$DryRun,
    [switch]$Force
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($Force) { throw 'The recovery upload gate cannot be bypassed with -Force.' }
$cliArguments = @('cloud-tokens', $Action, '--repo-root', $RepoRoot)
if ($Manifest) { $cliArguments += @('--manifest', $Manifest) }
if ($DryRun) { $cliArguments += '--dry-run' }
& $AgentBusPath @cliArguments
exit $LASTEXITCODE
