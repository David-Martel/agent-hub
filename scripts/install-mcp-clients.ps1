param(
    [switch]$Claude = $true,
    [switch]$Codex = $true,
    [switch]$Gemini = $true,
    [string]$ClaudeConfigPath = (Join-Path (Join-Path $HOME ".claude") "mcp.json"),
    [string]$CodexConfigPath = (Join-Path (Join-Path $HOME ".codex") "config.toml"),
    [string]$GeminiConfigPath = (Join-Path (Join-Path $HOME ".gemini") "settings.json"),
    [string]$RedisUrl = "redis://127.0.0.1:6380/0",
    [string]$DatabaseUrl = "postgresql://postgres@127.0.0.1:5300/redis_backend",
    [string]$ServerUrl = "http://localhost:8400",
    [string]$ServerHost = "localhost",
    [string]$CommandPath = "",
    [switch]$NoBackup,
    [switch]$ValidateOnly,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

function Resolve-AgentBusCommand {
    param([string]$Requested)

    if ($Requested) {
        return [pscustomobject]@{ Command = $Requested; Args = @() }
    }

    $exeSuffix = if ($IsWindows) { ".exe" } else { "" }
    $homeMcpBinary = Join-Path (Join-Path $HOME "bin") "agent-bus-mcp$exeSuffix"
    if (Test-Path $homeMcpBinary) {
        return [pscustomobject]@{ Command = $homeMcpBinary; Args = @() }
    }

    $homeBinary = Join-Path (Join-Path $HOME "bin") "agent-bus$exeSuffix"
    if (Test-Path $homeBinary) {
        return [pscustomobject]@{ Command = $homeBinary; Args = @("serve", "--transport", "stdio") }
    }

    $resolvedMcp = Get-Command "agent-bus-mcp$exeSuffix" -ErrorAction SilentlyContinue
    if (-not $resolvedMcp) {
        $resolvedMcp = Get-Command "agent-bus-mcp" -ErrorAction SilentlyContinue
    }
    if ($resolvedMcp) {
        return [pscustomobject]@{ Command = $resolvedMcp.Source; Args = @() }
    }

    $resolved = Get-Command "agent-bus$exeSuffix" -ErrorAction SilentlyContinue
    if (-not $resolved) {
        $resolved = Get-Command "agent-bus" -ErrorAction SilentlyContinue
    }
    if ($resolved) {
        return [pscustomobject]@{ Command = $resolved.Source; Args = @("serve", "--transport", "stdio") }
    }

    throw "agent-bus-mcp.exe / agent-bus.exe was not found in ~/bin or PATH. Run scripts/setup-agent-hub-local.ps1 first."
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

$resolvedCommand = Resolve-AgentBusCommand -Requested $CommandPath
$command = $resolvedCommand.Command
$stdioArgs = @($resolvedCommand.Args)
$sharedEnv = [ordered]@{
    AGENT_BUS_REDIS_URL       = $RedisUrl
    AGENT_BUS_DATABASE_URL    = $DatabaseUrl
    AGENT_BUS_SERVER_HOST     = $ServerHost
    AGENT_BUS_SERVER_URL      = $ServerUrl
    AGENT_BUS_SERVICE_AGENT_ID = "agent-bus"
    AGENT_BUS_STARTUP_ENABLED = "false"
    RUST_LOG                  = "error"
}

if ($ValidateOnly) {
    $validator = Join-Path $PSScriptRoot "validate-agent-client-configs.ps1"
    if (-not (Test-Path $validator)) {
        throw "Validator not found at $validator"
    }
    & $validator -ExpectedServerUrl $ServerUrl -ExpectedRedisUrl $RedisUrl -ExpectedDatabaseUrl $DatabaseUrl
    exit $LASTEXITCODE
}

if ($DryRun) {
    Write-Host "[DRY-RUN] MCP client install plan:" -ForegroundColor Cyan
    Write-Host "  - Command: $command"
    Write-Host "  - Args: $($stdioArgs -join ' ')"
    Write-Host "  - Backups: $(if ($NoBackup) { 'disabled' } else { 'enabled when target exists' })"
    Write-Host "  - Shared environment:"
    foreach ($entry in $sharedEnv.GetEnumerator()) {
        Write-Host "      $($entry.Key)=$($entry.Value)"
    }
    if ($Claude) {
        Write-Host "  - Would update Claude MCP config: $ClaudeConfigPath"
    }
    if ($Codex) {
        Write-Host "  - Would update Codex MCP config: $CodexConfigPath"
    }
    if ($Gemini) {
        Write-Host "  - Would update Gemini MCP config: $GeminiConfigPath"
    }
    Write-Host "  No client config files were changed."
    exit 0
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
        args = $stdioArgs
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
args = [$(($stdioArgs | ForEach-Object { '"' + (Escape-Toml $_) + '"' }) -join ", ")]
startup_timeout_sec = 10
tool_timeout_sec = 30

[mcp_servers.agent_bus.env]
AGENT_BUS_REDIS_URL = "$(Escape-Toml $RedisUrl)"
AGENT_BUS_DATABASE_URL = "$(Escape-Toml $DatabaseUrl)"
AGENT_BUS_SERVER_HOST = "$(Escape-Toml $ServerHost)"
AGENT_BUS_SERVER_URL = "$(Escape-Toml $ServerUrl)"
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

if ($Gemini) {
    $geminiDir = Split-Path -Parent $GeminiConfigPath
    New-Item -ItemType Directory -Path $geminiDir -Force | Out-Null
    if (Test-Path $GeminiConfigPath) {
        Backup-File -Path $GeminiConfigPath -Skip:$NoBackup
        $json = (Get-Content $GeminiConfigPath -Raw) | ConvertFrom-Json -AsHashtable -Depth 100
    }
    else {
        $json = [ordered]@{}
    }
    if (-not $json.ContainsKey("mcpServers") -or $null -eq $json.mcpServers) {
        $json.mcpServers = [ordered]@{}
    }
    $json.mcpServers["agent-bus"] = [ordered]@{
        command = $command
        args = $stdioArgs
        timeout = 30000
        env = $sharedEnv
    }
    $json | ConvertTo-Json -Depth 100 | Set-Content -Path $GeminiConfigPath -Encoding UTF8
    Write-Host "Updated Gemini MCP config at $GeminiConfigPath"
}

$validator = Join-Path $PSScriptRoot "validate-agent-client-configs.ps1"
if (Test-Path $validator) {
    & $validator -ExpectedServerUrl $ServerUrl -ExpectedRedisUrl $RedisUrl -ExpectedDatabaseUrl $DatabaseUrl
}
