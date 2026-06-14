#!/usr/bin/env pwsh
# Cross-platform validation: confirm the agent-bus workspace compiles and its
# library unit tests pass on Linux glibc, using the system Docker daemon.
#
# Local-execution-first: this runs against the local Docker daemon. If Docker is
# not installed/running, it prints a clear message and exits 0 (no-op) so it can
# be invoked opportunistically without failing developer workflows.
#
# Usage:
#   pwsh -NoLogo -NoProfile -File scripts/cross-validate.ps1
#   pwsh -NoLogo -NoProfile -File scripts/cross-validate.ps1 -Musl
#   pwsh -NoLogo -NoProfile -File scripts/cross-validate.ps1 -Strict   # fail if Docker absent
#   pwsh -NoLogo -NoProfile -File scripts/cross-validate.ps1 -DryRun

[CmdletBinding()]
param(
    [switch]$Musl,
    [string]$Tag = "agent-bus-linux-validate",
    [switch]$Strict,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$dockerfile = Join-Path $repoRoot "docker/Dockerfile.linux"

if (-not (Test-Path $dockerfile)) {
    Write-Error "Dockerfile not found at $dockerfile"
    exit 1
}

$buildArgs = @("build", "-f", $dockerfile, "-t", $Tag)
if ($Musl) {
    $buildArgs += @("--target", "musl")
}
$buildArgs += $repoRoot

Write-Host "==> docker $($buildArgs -join ' ')"
if ($DryRun) {
    Write-Host "[DRY-RUN] Docker cross-validation command was not executed."
    exit 0
}

$docker = Get-Command docker -ErrorAction SilentlyContinue
if (-not $docker) {
    $msg = "Docker is not installed or not on PATH; skipping Linux cross-platform validation."
    if ($Strict) {
        Write-Error $msg
        exit 1
    }
    Write-Warning $msg
    exit 0
}

# Confirm the daemon is actually reachable, not just the CLI present.
& docker info *> $null
if ($LASTEXITCODE -ne 0) {
    $msg = "Docker CLI found but the daemon is not reachable; skipping Linux cross-platform validation."
    if ($Strict) {
        Write-Error $msg
        exit 1
    }
    Write-Warning $msg
    exit 0
}

& docker @buildArgs
if ($LASTEXITCODE -ne 0) {
    Write-Error "Linux Docker build/test validation failed (exit $LASTEXITCODE)."
    exit $LASTEXITCODE
}

Write-Host "`nLinux cross-platform validation passed (build + workspace lib tests)."
