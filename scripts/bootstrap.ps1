#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Bootstrap the Agent Hub on a new Windows machine.
.DESCRIPTION
    1. Checks prerequisites (Rust, Redis, PostgreSQL, nssm)
    2. Builds the release binary
    3. Installs to ~/bin/agent-bus.exe
    4. Creates config at ~/.config/agent-bus/config.json
    5. Installs Windows service via nssm
    6. Validates health endpoint
.PARAMETER SkipBuild
    Use pre-built binary from T:\RustCache instead of building from source.
.PARAMETER RedisPort
    Redis port (default: 6380).
.PARAMETER PgPort
    PostgreSQL port (default: 5300).
.EXAMPLE
    pwsh -NoLogo -NoProfile -File scripts\bootstrap.ps1
.EXAMPLE
    pwsh -NoLogo -NoProfile -File scripts\bootstrap.ps1 -SkipBuild -RedisPort 6379 -PgPort 5432
#>
param(
    [switch]$SkipBuild,
    [int]$RedisPort = 6380,
    [int]$PgPort = 5300
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

Write-Host "=== Agent Hub Bootstrap ===" -ForegroundColor Cyan
Write-Host "  Repo:        $repoRoot"
Write-Host "  Redis port:  $RedisPort"
Write-Host "  PG port:     $PgPort"

# ---------------------------------------------------------------------------
# Helper: run a command and return whether it succeeded
# ---------------------------------------------------------------------------
function Test-Command {
    param([string]$Cmd)
    try {
        $null = Invoke-Expression $Cmd 2>&1
        return $true
    }
    catch { return $false }
}

# ---------------------------------------------------------------------------
# Step 1: Prerequisites
# ---------------------------------------------------------------------------
Write-Host "`n[1/6] Checking prerequisites..." -ForegroundColor Yellow

$prereqs = @(
    @{ Name = "Rust (rustc)"; Cmd = "rustc --version";              Install = "winget install Rustlang.Rustup" }
    @{ Name = "Cargo";        Cmd = "cargo --version";              Install = "winget install Rustlang.Rustup" }
    @{ Name = "Redis";        Cmd = "redis-cli -p $RedisPort ping"; Install = "winget install Redis.Redis" }
    @{ Name = "nssm";         Cmd = "nssm version";                 Install = "winget install NSSM.NSSM" }
)

$missing = @()
foreach ($p in $prereqs) {
    if (Test-Command $p.Cmd) {
        Write-Host "  [OK]      $($p.Name)" -ForegroundColor Green
    }
    else {
        Write-Host "  [MISSING] $($p.Name) — $($p.Install)" -ForegroundColor Red
        $missing += $p.Name
    }
}

if ($missing.Count -gt 0) {
    $critical = $missing | Where-Object { $_ -in @("Rust (rustc)", "Cargo") }
    if ($critical.Count -gt 0 -and -not $SkipBuild) {
        throw "Critical prerequisites missing: $($critical -join ', '). Install them and re-run."
    }
    if ($missing -contains "Redis") {
        Write-Host "  [WARN] Redis unavailable — health check will fail. Start Redis before running the service." -ForegroundColor Yellow
    }
    if ($missing -contains "nssm") {
        Write-Host "  [WARN] nssm unavailable — service installation will be skipped." -ForegroundColor Yellow
    }
}

# ---------------------------------------------------------------------------
# Step 2: Build
# ---------------------------------------------------------------------------
if (-not $SkipBuild) {
    Write-Host "`n[2/6] Building release binary..." -ForegroundColor Yellow
    Push-Location "$repoRoot\rust-cli"
    try {
        $env:RUSTC_WRAPPER = ""
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed (exit $LASTEXITCODE)" }
        Write-Host "  [OK] Build complete" -ForegroundColor Green
    }
    finally {
        Pop-Location
    }
}
else {
    Write-Host "`n[2/6] Skipping build (-SkipBuild specified)" -ForegroundColor Yellow
}

# ---------------------------------------------------------------------------
# Step 3: Install binary
# ---------------------------------------------------------------------------
Write-Host "`n[3/6] Installing binary..." -ForegroundColor Yellow

$binDir = "$env:USERPROFILE\bin"
if (-not (Test-Path $binDir)) {
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
}

# Probe candidate paths in preference order
$candidates = @(
    "T:\RustCache\cargo-target\release\agent-bus.exe"
    "$repoRoot\rust-cli\target\release\agent-bus.exe"
)
$srcBin = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $srcBin) {
    throw "agent-bus.exe not found in any of: $($candidates -join ', '). Run without -SkipBuild or build manually."
}

Copy-Item $srcBin "$binDir\agent-bus.exe" -Force
Write-Host "  [OK] Installed: $binDir\agent-bus.exe  (from $srcBin)" -ForegroundColor Green

# Ensure ~/bin is in PATH for this session
if ($env:PATH -notlike "*$binDir*") {
    $env:PATH = "$binDir;$env:PATH"
    Write-Host "  [INFO] Added $binDir to session PATH" -ForegroundColor Gray
    Write-Host "  [INFO] To persist, add $binDir to your user PATH via System Properties." -ForegroundColor Gray
}

# ---------------------------------------------------------------------------
# Step 4: Config
# ---------------------------------------------------------------------------
Write-Host "`n[4/6] Creating config..." -ForegroundColor Yellow

$configDir = "$env:USERPROFILE\.config\agent-bus"
if (-not (Test-Path $configDir)) {
    New-Item -ItemType Directory -Path $configDir -Force | Out-Null
}

$configPath = "$configDir\config.json"
if (-not (Test-Path $configPath)) {
    $config = [ordered]@{
        redis_url    = "redis://localhost:$RedisPort/0"
        database_url = "postgresql://postgres@localhost:$PgPort/redis_backend"
        server_host  = "localhost"
        stream_maxlen = 100000
    } | ConvertTo-Json -Depth 3
    Set-Content -Path $configPath -Value $config -Encoding UTF8
    Write-Host "  [OK] Created: $configPath" -ForegroundColor Green
}
else {
    Write-Host "  [OK] Already exists: $configPath" -ForegroundColor Green
}

# ---------------------------------------------------------------------------
# Step 5: Windows service via nssm
# ---------------------------------------------------------------------------
Write-Host "`n[5/6] Installing Windows service..." -ForegroundColor Yellow

if (-not (Test-Command "nssm version")) {
    Write-Host "  [SKIP] nssm not available — skipping service installation" -ForegroundColor Yellow
    Write-Host "         Run manually: agent-bus serve --transport http --port 8400" -ForegroundColor Gray
}
else {
    $svcName = "AgentHub"
    $svcStatus = nssm status $svcName 2>&1

    if ($svcStatus -match "SERVICE_RUNNING|SERVICE_STOPPED|SERVICE_PAUSED") {
        Write-Host "  [OK] Service '$svcName' already installed (status: $svcStatus)" -ForegroundColor Green
    }
    else {
        $logDir = "C:\ProgramData\AgentHub\logs"
        if (-not (Test-Path $logDir)) {
            New-Item -ItemType Directory -Path $logDir -Force | Out-Null
        }

        nssm install $svcName "$binDir\agent-bus.exe" "serve --transport http --port 8400"
        nssm set $svcName AppDirectory        $env:USERPROFILE
        nssm set $svcName AppStdout           "$logDir\agent-hub.log"
        nssm set $svcName AppStderr           "$logDir\agent-hub-error.log"
        nssm set $svcName AppRotateFiles      1
        nssm set $svcName AppRotateOnline     1
        nssm set $svcName AppRotateBytes      10485760   # 10 MB
        nssm set $svcName AppEnvironmentExtra `
            "AGENT_BUS_REDIS_URL=redis://localhost:$RedisPort/0" `
            "AGENT_BUS_DATABASE_URL=postgresql://postgres@localhost:$PgPort/redis_backend" `
            "RUST_LOG=error"
        nssm set $svcName Description "Agent Hub — Redis-backed multi-agent coordination bus"
        nssm set $svcName Start SERVICE_AUTO_START

        nssm start $svcName
        if ($LASTEXITCODE -ne 0) {
            Write-Host "  [WARN] nssm start returned exit code $LASTEXITCODE — check logs at $logDir" -ForegroundColor Yellow
        }
        else {
            Write-Host "  [OK] Service '$svcName' installed and started" -ForegroundColor Green
            Write-Host "       Logs: $logDir" -ForegroundColor Gray
        }
    }
}

# ---------------------------------------------------------------------------
# Step 6: Validate
# ---------------------------------------------------------------------------
Write-Host "`n[6/6] Validating..." -ForegroundColor Yellow

# Give service a moment to bind the port
Start-Sleep -Seconds 2

$healthPassed = $false
try {
    $healthJson = & "$binDir\agent-bus.exe" health --encoding json 2>&1
    $health = $healthJson | ConvertFrom-Json
    if ($health.ok) {
        $healthPassed = $true
        Write-Host "  [OK] Health check passed" -ForegroundColor Green
        Write-Host "       Redis:    $($health.redis_url)"   -ForegroundColor Gray
        Write-Host "       PG:       ok=$($health.database_ok)" -ForegroundColor Gray
        Write-Host "       Messages: $($health.stream_length) in stream" -ForegroundColor Gray
    }
    else {
        Write-Host "  [WARN] Health returned ok=false — Redis may not be ready" -ForegroundColor Yellow
        Write-Host "         Raw: $healthJson" -ForegroundColor Gray
    }
}
catch {
    Write-Host "  [WARN] Health check failed: $_" -ForegroundColor Yellow
    Write-Host "         Ensure Redis is running on port $RedisPort and retry." -ForegroundColor Gray
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "=== Bootstrap Complete ===" -ForegroundColor Cyan
Write-Host "  Binary:  $binDir\agent-bus.exe"
Write-Host "  Config:  $configPath"
Write-Host "  Logs:    C:\ProgramData\AgentHub\logs\"
Write-Host ""
Write-Host "  Quick commands:" -ForegroundColor Yellow
Write-Host "    agent-bus health --encoding json"
Write-Host "    agent-bus serve --transport http --port 8400"
Write-Host "    agent-bus serve --transport stdio"
Write-Host "    nssm status AgentHub"
Write-Host ""
Write-Host "  Docs: $repoRoot\AGENT_COMMUNICATIONS.md"
Write-Host "  Templates: $repoRoot\docs\agent-templates\"

if (-not $healthPassed) {
    Write-Host ""
    Write-Host "  ACTION REQUIRED: Health check failed." -ForegroundColor Red
    Write-Host "  1. Start Redis:   redis-server --port $RedisPort" -ForegroundColor Red
    Write-Host "  2. Re-run:        agent-bus health --encoding json" -ForegroundColor Red
    exit 1
}
