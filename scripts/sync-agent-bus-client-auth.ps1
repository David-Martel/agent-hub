param(
    [string]$HomeDir = $HOME,
    [string]$ServiceName = "AgentHub",
    [string]$ServerUrl = "http://localhost:8400",
    [string]$RedisUrl = "redis://127.0.0.1:6380/0",
    [string]$DatabaseUrl = "postgresql://postgres@127.0.0.1:5300/redis_backend",
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

$configPath = Join-Path $HomeDir ".config/agent-bus/config.json"
$serviceRegPath = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName\Parameters"

if (-not (Test-Path $serviceRegPath)) {
    throw "Service registry path not found: $serviceRegPath"
}

$service = Get-ItemProperty -Path $serviceRegPath
$envExtra = @($service.AppEnvironmentExtra)
$tokenEntry = $envExtra | Where-Object { $_ -like "AGENT_BUS_AUTH_TOKEN=*" } | Select-Object -First 1
if (-not $tokenEntry) {
    throw "AGENT_BUS_AUTH_TOKEN was not found in $ServiceName AppEnvironmentExtra."
}

$authToken = $tokenEntry.Substring("AGENT_BUS_AUTH_TOKEN=".Length)
if ([string]::IsNullOrWhiteSpace($authToken)) {
    throw "AGENT_BUS_AUTH_TOKEN in $ServiceName AppEnvironmentExtra is empty."
}

$config = [ordered]@{}
if (Test-Path $configPath) {
    $existing = Get-Content -Path $configPath -Raw | ConvertFrom-Json
    foreach ($property in $existing.PSObject.Properties) {
        $config[$property.Name] = $property.Value
    }
}

$config["server_url"] = $ServerUrl
$config["redis_url"] = $RedisUrl
$config["database_url"] = $DatabaseUrl
$config["auth_token"] = $authToken

$json = $config | ConvertTo-Json -Depth 20

if ($DryRun) {
    Write-Host "Would update $configPath with server_url, redis_url, database_url, and auth_token=***."
    exit 0
}

$parent = Split-Path -Parent $configPath
New-Item -ItemType Directory -Path $parent -Force | Out-Null
if (Test-Path $configPath) {
    $backupPath = "$configPath.bak-$(Get-Date -Format yyyyMMdd-HHmmss)"
    Copy-Item -Path $configPath -Destination $backupPath -Force
    Write-Host "Backed up existing config to $backupPath"
}

Set-Content -Path $configPath -Value $json -Encoding UTF8
Write-Host "Updated $configPath with AgentHub client auth token (redacted)."
