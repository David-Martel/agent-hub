param(
    [int]$Port = 8400,
    [string]$DatabaseName = "redis_backend",
    [string]$BinaryPath = (Join-Path $HOME "bin/agent-bus-http.exe"),
    [string]$LogRoot = "C:\ProgramData\AgentHub\logs"
)

$ErrorActionPreference = "Stop"

New-Item -ItemType Directory -Path $LogRoot -Force | Out-Null
$logPath = Join-Path $LogRoot "agent-hub-service.log"

if (-not (Test-Path $BinaryPath)) {
    throw "agent-bus binary not found at $BinaryPath"
}

$env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
$env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@localhost:5300/$DatabaseName"
$env:AGENT_BUS_SERVER_HOST = "localhost"
$env:AGENT_BUS_STARTUP_ENABLED = "true"
$env:RUST_LOG = "warn"

Start-Transcript -Path $logPath -Append | Out-Null
try {
    & $BinaryPath "serve" "--transport" "http" "--port" $Port
    if ($LASTEXITCODE -ne 0) {
        throw "agent-bus service runner exited with code $LASTEXITCODE"
    }
}
finally {
    Stop-Transcript | Out-Null
}
