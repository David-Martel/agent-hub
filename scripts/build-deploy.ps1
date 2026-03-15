<#
.SYNOPSIS
    Build, deploy, and restart the Agent Hub service.
.DESCRIPTION
    1. Builds the release binary (uses sccache if available)
    2. Copies from T:\RustCache\cargo-target\release\ to ~\bin\
    3. Restarts the AgentHub Windows service
    4. Verifies health endpoint responds
.PARAMETER SkipBuild
    Skip the cargo build step (deploy existing binary only)
.PARAMETER SkipService
    Skip service restart (build and copy only)
#>
param(
    [switch]$SkipBuild,
    [switch]$SkipService
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$rustCliDir = Join-Path $repoRoot "rust-cli"
$targetBinary = "T:\RustCache\cargo-target\release\agent-bus.exe"
$deployPath = "C:\Users\david\bin\agent-bus.exe"
$serviceName = "AgentHub"
$healthUrl = "http://localhost:8400/health"

# Step 1: Build
if (-not $SkipBuild) {
    Write-Host "Building release binary..."
    Push-Location $rustCliDir
    try {
        $env:RUSTC_WRAPPER = ""
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    }
    finally {
        Pop-Location
    }
    Write-Host "Build complete."
}

# Step 2: Verify binary exists
if (-not (Test-Path $targetBinary)) {
    throw "Release binary not found at $targetBinary"
}
$size = (Get-Item $targetBinary).Length / 1MB
Write-Host "Binary: $targetBinary ($([math]::Round($size, 1)) MB)"

# Step 3: Stop service if running
if (-not $SkipService) {
    $svc = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -eq "Running") {
        Write-Host "Stopping $serviceName service..."
        Stop-Service -Name $serviceName -Force
        Start-Sleep -Seconds 2
    }
}

# Step 4: Deploy binary
Write-Host "Deploying to $deployPath..."
Copy-Item -Path $targetBinary -Destination $deployPath -Force
Write-Host "Deployed."

# Step 5: Start service
if (-not $SkipService) {
    $svc = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
    if ($svc) {
        Write-Host "Starting $serviceName service..."
        Start-Service -Name $serviceName
        $svc.WaitForStatus("Running", [TimeSpan]::FromSeconds(15))

        # Health check
        $deadline = (Get-Date).AddSeconds(10)
        do {
            try {
                $health = Invoke-RestMethod -Uri $healthUrl -Method Get -TimeoutSec 3
                if ($health.ok -eq $true) {
                    Write-Host "`nService healthy:"
                    Write-Host "  Redis: $($health.stream_length) messages"
                    Write-Host "  PostgreSQL: $($health.pg_message_count) messages"
                    Write-Host "  Presence: $($health.pg_presence_count) records"
                    return
                }
            }
            catch { Start-Sleep -Seconds 1 }
        } while ((Get-Date) -lt $deadline)

        Write-Warning "Service started but health check timed out"
    }
    else {
        Write-Host "Service '$serviceName' not installed. Run install-agent-hub-service.ps1 first."
    }
}

Write-Host "Done."
