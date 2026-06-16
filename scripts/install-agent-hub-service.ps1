param(
    [string]$ServiceName = "AgentHub",
    [string]$DisplayName = "Agent Hub Coordination Service",
    [int]$Port = 8400,
    [string]$DatabaseName = "redis_backend",
    [string]$BinaryPath = (Join-Path $HOME "bin/agent-bus-http.exe"),
    [switch]$ForceReinstall,
    [bool]$StartService = $true,
    # Bind the HTTP server off-loopback so LAN peers can reach this coordinator.
    # When set, AGENT_BUS_SERVER_HOST defaults to 0.0.0.0 and an auth token is required.
    [switch]$AllowRemote,
    [string]$ServerHost,
    # Bearer token for remote auth. If -AllowRemote and this is empty, the token is
    # read from ~/.config/agent-bus/config.json (.auth_token).
    [string]$AuthToken,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

# Resolve the effective bind host + auth token for remote mode.
if ($AllowRemote) {
    if (-not $ServerHost) { $ServerHost = "0.0.0.0" }
    if (-not $AuthToken) {
        $cfgPath = Join-Path $HOME ".config/agent-bus/config.json"
        if (Test-Path $cfgPath) {
            try { $AuthToken = (Get-Content $cfgPath -Raw | ConvertFrom-Json).auth_token } catch { }
        }
    }
    if (-not $AuthToken) {
        throw "-AllowRemote requires an auth token (pass -AuthToken or set .auth_token in ~/.config/agent-bus/config.json)."
    }
}
elseif (-not $ServerHost) {
    $ServerHost = "localhost"
}

$logRoot = "C:\ProgramData\AgentHub\logs"
$stdoutLog = Join-Path $logRoot "agent-hub-stdout.log"
$stderrLog = Join-Path $logRoot "agent-hub-stderr.log"

if ($DryRun) {
    Write-Host "[DRY-RUN] Service install plan:" -ForegroundColor Cyan
    Write-Host "  - Service name: $ServiceName"
    Write-Host "  - Display name: $DisplayName"
    Write-Host "  - Binary: $BinaryPath"
    Write-Host "  - Command: $BinaryPath serve --transport http --port $Port"
    Write-Host "  - Logs:"
    Write-Host "      stdout: $stdoutLog"
    Write-Host "      stderr: $stderrLog"
    Write-Host "  - Environment:"
    Write-Host "      AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0"
    Write-Host "      AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:5300/$DatabaseName"
    Write-Host "      AGENT_BUS_SERVER_HOST=$ServerHost"
    if ($AllowRemote) {
        Write-Host "      AGENT_BUS_ALLOW_REMOTE=true"
        Write-Host "      AGENT_BUS_AUTH_TOKEN=*** (set; from param or config.json)"
    }
    Write-Host "      AGENT_BUS_SERVICE_NAME=$ServiceName"
    Write-Host "  - Force reinstall: $ForceReinstall"
    Write-Host "  - Start service after install: $StartService"
    if (-not (Test-Path $BinaryPath)) {
        Write-Host "  - Note: binary does not currently exist at $BinaryPath" -ForegroundColor Yellow
    }
    if (-not (Get-Command nssm -ErrorAction SilentlyContinue)) {
        Write-Host "  - Note: nssm is not currently on PATH" -ForegroundColor Yellow
    }
    Write-Host "  No service, log directory, or NSSM configuration was changed."
    exit 0
}

$nssmPath = (Get-Command nssm -ErrorAction Stop).Source

# Verify binary exists
if (-not (Test-Path $binaryPath)) {
    throw "agent-bus binary not found at $binaryPath. Build with: cargo build --release"
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
& $nssmPath set $ServiceName Description "Agent Hub coordination service (Redis + PostgreSQL). HTTP REST + SSE at ${ServerHost}:$Port$(if ($AllowRemote) { ' (remote-enabled, bearer auth)' })"
& $nssmPath set $ServiceName Start SERVICE_AUTO_START
& $nssmPath set $ServiceName AppDirectory (Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path))

# Environment variables. Redis/PostgreSQL stay pinned to 127.0.0.1 (loopback) in
# all modes; only the HTTP server binds off-loopback when -AllowRemote is set.
$envPairs = [System.Collections.Generic.List[string]]::new()
$envPairs.Add("AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0")
$envPairs.Add("AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:5300/$DatabaseName")
$envPairs.Add("AGENT_BUS_SERVER_HOST=$ServerHost")
if ($AllowRemote) {
    $envPairs.Add("AGENT_BUS_ALLOW_REMOTE=true")
    $envPairs.Add("AGENT_BUS_AUTH_TOKEN=$AuthToken")
}
$envPairs.Add("AGENT_BUS_SERVICE_NAME=$ServiceName")
$envPairs.Add("AGENT_BUS_STARTUP_ENABLED=true")
$envPairs.Add("AGENT_BUS_STREAM_MAXLEN=100000")
$envPairs.Add("RUST_LOG=warn")
& $nssmPath set $ServiceName AppEnvironmentExtra @($envPairs.ToArray())

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
