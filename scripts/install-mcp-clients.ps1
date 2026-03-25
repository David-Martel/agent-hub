param(
    [switch]$Claude = $true,
    [switch]$Codex = $true,
    [string]$ClaudeConfigPath = (Join-Path $HOME ".claude.json"),
    [string]$CodexConfigPath = (Join-Path $HOME ".codex\config.toml"),
    [string]$RedisUrl = "redis://localhost:6380/0",
    [string]$DatabaseUrl = "postgresql://postgres@localhost:5300/redis_backend",
    [string]$ServerHost = "localhost",
    [string]$CommandPath = "",
    [switch]$NoBackup
)

$ErrorActionPreference = "Stop"

function Resolve-AgentBusCommand {
    param([string]$Requested)

    if ($Requested) {
        return $Requested
    }

    $homeMcpBinary = Join-Path $HOME "bin\agent-bus-mcp.exe"
    if (Test-Path $homeMcpBinary) {
        return $homeMcpBinary
    }

    $homeHttpBinary = Join-Path $HOME "bin\agent-bus-http.exe"
    if (Test-Path $homeHttpBinary) {
        return $homeHttpBinary
    }

    $homeBinary = Join-Path $HOME "bin\agent-bus.exe"
    if (Test-Path $homeBinary) {
        return $homeBinary
    }

    $resolvedMcp = Get-Command agent-bus-mcp.exe -ErrorAction SilentlyContinue
    if ($resolvedMcp) {
        return $resolvedMcp.Source
    }

    $resolvedHttp = Get-Command agent-bus-http.exe -ErrorAction SilentlyContinue
    if ($resolvedHttp) {
        return $resolvedHttp.Source
    }

    $resolved = Get-Command agent-bus.exe -ErrorAction SilentlyContinue
    if ($resolved) {
        return $resolved.Source
    }

    throw "agent-bus-mcp.exe / agent-bus-http.exe / agent-bus.exe was not found in ~/bin or PATH. Run scripts/setup-agent-hub-local.ps1 first."
}

function Backup-File {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [switch]$Skip
    )

    if ($Skip -or -not (Test-Path $Path)) {
        return
    }

    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    Copy-Item -Path $Path -Destination "$Path.bak-$stamp" -Force
}

function Escape-Toml {
    param([Parameter(Mandatory = $true)][string]$Value)

    return $Value.Replace('\', '\\').Replace('"', '\"')
}

$command = Resolve-AgentBusCommand -Requested $CommandPath
$sharedEnv = [ordered]@{
    AGENT_BUS_REDIS_URL       = $RedisUrl
    AGENT_BUS_DATABASE_URL    = $DatabaseUrl
    AGENT_BUS_SERVER_HOST     = $ServerHost
    AGENT_BUS_SERVICE_AGENT_ID = "agent-bus"
    AGENT_BUS_STARTUP_ENABLED = "false"
    RUST_LOG                  = "error"
}

if ($Claude) {
    if (-not (Test-Path $ClaudeConfigPath)) {
        throw "Claude config not found at $ClaudeConfigPath"
    }

    Backup-File -Path $ClaudeConfigPath -Skip:$NoBackup
    $raw = Get-Content $ClaudeConfigPath -Raw
    $json = $raw | ConvertFrom-Json -AsHashtable -Depth 100
    if (-not $json.ContainsKey("mcpServers") -or $null -eq $json.mcpServers) {
        $json.mcpServers = [ordered]@{}
    }

    $json.mcpServers["agent-bus"] = [ordered]@{
        type = "stdio"
        command = $command
        args = @("serve", "--transport", "stdio")
        env = $sharedEnv
    }

    $json | ConvertTo-Json -Depth 100 | Set-Content -Path $ClaudeConfigPath -Encoding UTF8
    Write-Host "Updated Claude MCP config at $ClaudeConfigPath"
}

if ($Codex) {
    $codexDir = Split-Path -Parent $CodexConfigPath
    New-Item -ItemType Directory -Path $codexDir -Force | Out-Null
    Backup-File -Path $CodexConfigPath -Skip:$NoBackup

    $managedBlock = @"
# BEGIN agent-bus MCP (managed by scripts/install-mcp-clients.ps1)
[mcp_servers.agent_bus]
command = "$(Escape-Toml $command)"
args = ["serve", "--transport", "stdio"]
startup_timeout_sec = 10
tool_timeout_sec = 30

[mcp_servers.agent_bus.env]
AGENT_BUS_REDIS_URL = "$(Escape-Toml $RedisUrl)"
AGENT_BUS_DATABASE_URL = "$(Escape-Toml $DatabaseUrl)"
AGENT_BUS_SERVER_HOST = "$(Escape-Toml $ServerHost)"
AGENT_BUS_SERVICE_AGENT_ID = "agent-bus"
AGENT_BUS_STARTUP_ENABLED = "false"
RUST_LOG = "error"
# END agent-bus MCP (managed by scripts/install-mcp-clients.ps1)
"@

    $content = if (Test-Path $CodexConfigPath) {
        Get-Content $CodexConfigPath -Raw
    }
    else {
        ""
    }

    $sectionStartPattern = '(?ms)(# BEGIN agent-bus MCP \(managed by scripts/install-mcp-clients\.ps1\)|# Shared Redis-backed agent coordination bus|\[mcp_servers\.agent_bus(?:\.env)?\])'
    $existingStart = [regex]::Match($content, $sectionStartPattern)
    if ($existingStart.Success) {
        $content = $content.Substring(0, $existingStart.Index)
    }
    $content = $content.TrimEnd()
    if ($content) {
        $content += "`r`n`r`n"
    }
    $content += $managedBlock

    Set-Content -Path $CodexConfigPath -Value $content -Encoding UTF8
    Write-Host "Updated Codex MCP config at $CodexConfigPath"
}
