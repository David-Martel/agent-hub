#Requires -Version 7
<#
.SYNOPSIS
    Idempotent installer for Rust acceleration + profiling tooling used by the agent-bus workspace.

.DESCRIPTION
    Installs missing tools only — skips anything already on PATH.
    Run from anywhere; does not require the repo to be built first.

.NOTES
    See docs/rust-acceleration.md for usage details on each tool.
#>

[CmdletBinding()]
param(
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── helpers ──────────────────────────────────────────────────────────────────

function Find-Command {
    param([string]$Name)
    $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

$Results = [System.Collections.Generic.List[pscustomobject]]::new()

function Install-CargoBin {
    param(
        [string]$BinName,          # binary that should appear on PATH after install
        [string]$CrateName,        # crate name passed to `cargo install`
        [string]$Description,
        [string[]]$ExtraArgs = @() # e.g. @("--features", "foo")
    )

    $status = if (Find-Command $BinName) { 'ALREADY INSTALLED' } else {
        if ($DryRun) {
            'WOULD INSTALL'
        } else {
            Write-Host "Installing $CrateName ..." -ForegroundColor Cyan
            $installArgs = @('install', '--locked', $CrateName) + $ExtraArgs
            & cargo @installArgs
            if ($LASTEXITCODE -ne 0) { 'FAILED' } else { 'INSTALLED' }
        }
    }

    $Results.Add([pscustomobject]@{
        Tool        = $BinName
        Crate       = $CrateName
        Status      = $status
        Description = $Description
    })
}

function Install-RustupComponent {
    param(
        [string]$Component,
        [string]$Description,
        [string]$Toolchain = 'stable'
    )

    $installed = (rustup component list --toolchain $Toolchain 2>$null) -match "^$Component \(installed\)"
    $status = if ($installed) { 'ALREADY INSTALLED' } else {
        if ($DryRun) {
            'WOULD INSTALL'
        } else {
            Write-Host "Adding rustup component $Component ($Toolchain) ..." -ForegroundColor Cyan
            & rustup component add $Component --toolchain $Toolchain
            if ($LASTEXITCODE -ne 0) { 'FAILED' } else { 'INSTALLED' }
        }
    }

    $Results.Add([pscustomobject]@{
        Tool        = $Component
        Crate       = "(rustup component)"
        Status      = $status
        Description = $Description
    })
}

# ── prerequisite check ────────────────────────────────────────────────────────

if (-not (Find-Command 'cargo')) {
    Write-Error 'cargo not found on PATH. Install Rust via https://rustup.rs first.'
}

if (-not (Find-Command 'rustup')) {
    Write-Error 'rustup not found on PATH. Install Rust via https://rustup.rs first.'
}

# ── rustup components ─────────────────────────────────────────────────────────

Install-RustupComponent `
    -Component   'llvm-tools-preview' `
    -Description 'Required by cargo-llvm-cov for source-based coverage'

# ── cargo-installable tools ───────────────────────────────────────────────────

# Test runner — faster, better isolation, structured output
Install-CargoBin `
    -BinName     'cargo-nextest' `
    -CrateName   'cargo-nextest' `
    -Description 'Faster test runner (alias: cargo ab-nextest)'

# Coverage — LLVM source-based, LCOV/HTML output
Install-CargoBin `
    -BinName     'cargo-llvm-cov' `
    -CrateName   'cargo-llvm-cov' `
    -Description 'LLVM coverage reports (alias: cargo ab-cov)'

# Security audit — RustSec advisory database
Install-CargoBin `
    -BinName     'cargo-audit' `
    -CrateName   'cargo-audit' `
    -Description 'Dependency vulnerability audit (alias: cargo ab-audit)'

# Unused dependency finder — fast, static analysis
Install-CargoBin `
    -BinName     'cargo-machete' `
    -CrateName   'cargo-machete' `
    -Description 'Unused dependency finder (alias: cargo ab-machete)'

# Flamegraph profiling — requires dtrace on Windows or use samply instead
Install-CargoBin `
    -BinName     'cargo-flamegraph' `
    -CrateName   'flamegraph' `
    -Description 'CPU flamegraph profiler (alias: cargo ab-flame; uses [profile.profiling])'

# samply — Windows ETW profiler, opens Firefox Profiler UI (recommended on Windows)
Install-CargoBin `
    -BinName     'samply' `
    -CrateName   'samply' `
    -Description 'Windows ETW profiler → Firefox Profiler UI (preferred over flamegraph on Windows)'

# tokio-console — async task inspector
Install-CargoBin `
    -BinName     'tokio-console' `
    -CrateName   'tokio-console' `
    -Description 'Live Tokio async task dashboard (requires console-subscriber in binary)'

# ── summary table ─────────────────────────────────────────────────────────────

Write-Host ''
Write-Host '─── Installation Summary ────────────────────────────────────────────' -ForegroundColor White

$Results | Format-Table -AutoSize -Property `
    @{Label='Tool';        Expression={$_.Tool};        Width=24},
    @{Label='Status';      Expression={$_.Status};      Width=18},
    @{Label='Description'; Expression={$_.Description}}

$failed = $Results | Where-Object { $_.Status -eq 'FAILED' }
if ($failed) {
    Write-Host "WARNING: $($failed.Count) tool(s) failed to install. Check output above." -ForegroundColor Yellow
    exit 1
} else {
    Write-Host 'All tools ready.' -ForegroundColor Green
}
