param(
    [string]$ServiceName = "AgentHub",
    [string]$DisplayName = "Agent Hub Coordination Service",
    [int]$Port = 8400,
    [string]$DatabaseName = "redis_backend",
    [string]$BinaryPath = (Join-Path $HOME "bin/agent-bus-http.exe"),
    [switch]$ForceReinstall,
    [switch]$StartService = $true
)

$ErrorActionPreference = "Stop"

$nssmPath = (Get-Command nssm -ErrorAction Stop).Source
$logRoot = "C:\ProgramData\AgentHub\logs"
$stdoutLog = Join-Path $logRoot "agent-hub-stdout.log"
$stderrLog = Join-Path $logRoot "agent-hub-stderr.log"

# Verify binary exists
if (-not (Test-Path $binaryPath)) {
    throw "agent-bus binary not found at $binaryPath. Build with: cd rust-cli && cargo build --release"
}

# Create log directory
New-Item -ItemType Directory -Path $logRoot -Force | Out-Null

# Remove existing service if reinstalling
$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing) {
    if (-not $ForceReinstall) {
        throw "Service '$ServiceName' already exists. Use -ForceReinstall to replace it."
    }
    Write-Host "Removing existing service..."
    if ($existing.Status -ne "Stopped") {
        Stop-Service -Name $ServiceName -Force
        $existing.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
    }
    & $nssmPath remove $ServiceName confirm | Out-Null
    Start-Sleep -Seconds 2
}

# Install service — run Rust binary DIRECTLY (no PowerShell wrapper)
Write-Host "Installing $ServiceName service..."
& $nssmPath install $ServiceName $binaryPath "serve" "--transport" "http" "--port" "$Port"
& $nssmPath set $ServiceName DisplayName $DisplayName
& $nssmPath set $ServiceName Description "Agent Hub coordination service (Redis + PostgreSQL). HTTP REST + SSE at localhost:$Port"
& $nssmPath set $ServiceName Start SERVICE_AUTO_START
& $nssmPath set $ServiceName AppDirectory (Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path))

# Environment variables
& $nssmPath set $ServiceName AppEnvironmentExtra `
    "AGENT_BUS_REDIS_URL=redis://localhost:6380/0" `
    "AGENT_BUS_DATABASE_URL=postgresql://postgres@localhost:5300/$DatabaseName" `
    "AGENT_BUS_SERVER_HOST=localhost" `
    "AGENT_BUS_SERVICE_NAME=$ServiceName" `
    "AGENT_BUS_STARTUP_ENABLED=true" `
    "AGENT_BUS_STREAM_MAXLEN=100000" `
    "RUST_LOG=warn"

# Logging with rotation
& $nssmPath set $ServiceName AppStdout $stdoutLog
& $nssmPath set $ServiceName AppStderr $stderrLog
& $nssmPath set $ServiceName AppRotateFiles 1
& $nssmPath set $ServiceName AppRotateOnline 1
& $nssmPath set $ServiceName AppRotateSeconds 86400
& $nssmPath set $ServiceName AppRotateBytes 10485760

# Automatic restart on failure (3 attempts, 5s delay each, reset after 24h)
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/5000/restart/5000

Write-Host "Service installed: $ServiceName"

if ($StartService) {
    Write-Host "Starting service..."
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus("Running", [TimeSpan]::FromSeconds(30))

    # Health check
    $healthUri = "http://localhost:$Port/health"
    $deadline = (Get-Date).AddSeconds(15)
    do {
        try {
            $health = Invoke-RestMethod -Uri $healthUri -Method Get -TimeoutSec 3
            if ($health.ok -eq $true) {
                Write-Host "`nService running and healthy:"
                Write-Host ($health | ConvertTo-Json -Depth 5)
                return
            }
        }
        catch {
            Start-Sleep -Seconds 1
        }
    } while ((Get-Date) -lt $deadline)

    throw "Service started but health endpoint not ready at $healthUri"
}
