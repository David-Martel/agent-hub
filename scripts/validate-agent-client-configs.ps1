param(
    [string]$HomeDir = $HOME,
    [string]$ExpectedServerUrl = "http://localhost:8400",
    [string]$ExpectedRedisUrl = "redis://127.0.0.1:6380/0",
    [string]$ExpectedDatabaseUrl = "postgresql://postgres@127.0.0.1:5300/redis_backend",
    [string]$MinimumAgentBusVersion = "0.5.0",
    [switch]$Strict
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$results = New-Object System.Collections.Generic.List[object]

function Add-CheckResult {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Status,
        [Parameter(Mandatory = $true)][string]$Detail,
        [string]$Path = ""
    )

    $results.Add([pscustomobject]@{
            name   = $Name
            status = $Status
            detail = $Detail
            path   = $Path
        }) | Out-Null
}

function Test-JsonSyntax {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path $Path)) {
        Add-CheckResult -Name "json:$Path" -Status "missing" -Detail "File not found" -Path $Path
        return $null
    }

    try {
        $raw = Get-Content -Path $Path -Raw
        $json = $raw | ConvertFrom-Json -AsHashtable -Depth 100
        Add-CheckResult -Name "json:$Path" -Status "ok" -Detail "JSON parsed" -Path $Path
        if ($raw -match '192\.168\.|10\.|172\.(1[6-9]|2[0-9]|3[0-1])\.') {
            Add-CheckResult -Name "numeric-url:$Path" -Status "warn" -Detail "Private numeric IP found; prefer localhost or a stable hostname" -Path $Path
        }
        if ($raw -match 'Bearer\s+[A-Za-z0-9_\-\.]{12,}') {
            Add-CheckResult -Name "token:$Path" -Status "warn" -Detail "Literal bearer token pattern found; prefer environment variables" -Path $Path
        }
        return $json
    }
    catch {
        Add-CheckResult -Name "json:$Path" -Status "fail" -Detail $_.Exception.Message -Path $Path
        return $null
    }
}

function Test-TextConfig {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Needle,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if (-not (Test-Path $Path)) {
        Add-CheckResult -Name $Label -Status "missing" -Detail "File not found" -Path $Path
        return
    }

    $raw = Get-Content -Path $Path -Raw
    if ($raw.Contains($Needle)) {
        Add-CheckResult -Name $Label -Status "ok" -Detail "Expected marker found" -Path $Path
    }
    else {
        Add-CheckResult -Name $Label -Status "warn" -Detail "Expected marker not found" -Path $Path
    }
    if ($raw -match '192\.168\.|10\.|172\.(1[6-9]|2[0-9]|3[0-1])\.') {
        Add-CheckResult -Name "numeric-url:$Path" -Status "warn" -Detail "Private numeric IP found; prefer localhost or a stable hostname" -Path $Path
    }
}

function Get-AgentBusVersion {
    param([Parameter(Mandatory = $true)][string]$CommandName)

    $cmd = Get-Command $CommandName -ErrorAction SilentlyContinue
    if (-not $cmd) {
        Add-CheckResult -Name "binary:$CommandName" -Status "fail" -Detail "Command not found"
        return
    }

    Add-CheckResult -Name "binary:$CommandName" -Status "ok" -Detail "Resolved command" -Path $cmd.Source
    if ($CommandName -ne "agent-bus") {
        Add-CheckResult -Name "version:$CommandName" -Status "ok" -Detail "Version inherited from the validated workspace install; stdio/service binary is not invoked for --version" -Path $cmd.Source
        return
    }
    try {
        $versionText = & $cmd.Source --version 2>$null | Select-Object -First 1
        if ($versionText -match '(\d+\.\d+\.\d+)') {
            $actual = [version]$Matches[1]
            $minimum = [version]$MinimumAgentBusVersion
            if ($actual -lt $minimum) {
                Add-CheckResult -Name "version:$CommandName" -Status "fail" -Detail "Version $actual is older than required $minimum" -Path $cmd.Source
            }
            else {
                Add-CheckResult -Name "version:$CommandName" -Status "ok" -Detail "Version $actual meets required $minimum" -Path $cmd.Source
            }
        }
        else {
            Add-CheckResult -Name "version:$CommandName" -Status "warn" -Detail "Could not parse version output" -Path $cmd.Source
        }
    }
    catch {
        Add-CheckResult -Name "version:$CommandName" -Status "warn" -Detail $_.Exception.Message -Path $cmd.Source
    }
}

function Test-AgentBusJsonMcp {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ClientName
    )

    $json = Test-JsonSyntax -Path $Path
    if ($null -eq $json) {
        return
    }

    $servers = $json.mcpServers
    if ($null -eq $servers) {
        $servers = $json.mcp_servers
    }
    if ($null -eq $servers -or -not $servers.ContainsKey("agent-bus")) {
        Add-CheckResult -Name "${ClientName}:agent-bus" -Status "warn" -Detail "agent-bus MCP server entry not found" -Path $Path
        return
    }

    $entry = $servers["agent-bus"]
    $transport = if ($entry.ContainsKey("type")) { $entry["type"] } elseif ($entry.ContainsKey("httpUrl")) { "http" } else { "stdio" }
    Add-CheckResult -Name "${ClientName}:agent-bus" -Status "ok" -Detail "agent-bus MCP entry found ($transport)" -Path $Path

    if ($entry.ContainsKey("command")) {
        $command = [string]$entry["command"]
        if ($command -match 'agent-bus-mcp(\.exe)?$') {
            Add-CheckResult -Name "${ClientName}:command" -Status "ok" -Detail "Uses dedicated MCP binary" -Path $Path
        }
        else {
            Add-CheckResult -Name "${ClientName}:command" -Status "warn" -Detail "Does not use dedicated MCP binary" -Path $Path
        }
    }
}

function Test-AgentBusInstallShadowing {
    $installDirs = @(
        (Join-Path $HomeDir "bin"),
        (Join-Path $HomeDir ".local/bin")
    )
    foreach ($dir in $installDirs) {
        foreach ($name in @("agent-bus.exe", "agent-bus-http.exe", "agent-bus-mcp.exe")) {
            $path = Join-Path $dir $name
            $item = Get-Item -LiteralPath $path -ErrorAction SilentlyContinue
            if (-not $item) {
                continue
            }
            if ($item.Length -eq 0) {
                Add-CheckResult -Name "install:zero-byte:$name" -Status "fail" -Detail "Zero-byte agent-bus binary/shim shadows valid installs; delete or replace it" -Path $path
            }
            else {
                Add-CheckResult -Name "install:file:$name" -Status "ok" -Detail "Installed file size=$($item.Length)" -Path $path
            }
        }
    }
}

function Get-AgentHubServiceAuthState {
    param([Parameter(Mandatory = $true)][string]$ServiceName)

    $path = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName\Parameters"
    if (-not (Test-Path $path)) {
        return [pscustomobject]@{
            installed = $false
            authToken = $false
            allowRemote = $false
            path = $path
        }
    }

    $props = Get-ItemProperty -Path $path
    $envExtra = @($props.AppEnvironmentExtra)
    return [pscustomobject]@{
        installed = $true
        authToken = [bool]($envExtra | Where-Object { $_ -like "AGENT_BUS_AUTH_TOKEN=*" })
        allowRemote = [bool]($envExtra | Where-Object { $_ -like "AGENT_BUS_ALLOW_REMOTE=true" })
        path = $path
    }
}

function Test-AgentBusClientConfig {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)]$ServiceAuthState
    )

    $json = Test-JsonSyntax -Path $Path
    if ($null -eq $json) {
        return
    }

    $serverUrl = [string]$json.server_url
    $authToken = [string]$json.auth_token
    $hasProcessToken = -not [string]::IsNullOrWhiteSpace($env:AGENT_BUS_AUTH_TOKEN)

    if ([string]::IsNullOrWhiteSpace($serverUrl)) {
        Add-CheckResult -Name "client-config:server-url" -Status "warn" -Detail "server_url is absent; CLI will use direct Redis mode" -Path $Path
    }
    elseif ($serverUrl -ne $ExpectedServerUrl) {
        Add-CheckResult -Name "client-config:server-url" -Status "warn" -Detail "server_url is '$serverUrl', expected '$ExpectedServerUrl' for same-machine use" -Path $Path
    }
    else {
        Add-CheckResult -Name "client-config:server-url" -Status "ok" -Detail "server_url is $ExpectedServerUrl" -Path $Path
    }

    if ($ServiceAuthState.installed -and $ServiceAuthState.authToken -and -not $hasProcessToken -and [string]::IsNullOrWhiteSpace($authToken)) {
        Add-CheckResult -Name "client-config:auth-token" -Status "fail" -Detail "AgentHub service requires bearer auth, but neither AGENT_BUS_AUTH_TOKEN nor config auth_token is available to this client" -Path $Path
    }
    elseif ($ServiceAuthState.authToken) {
        Add-CheckResult -Name "client-config:auth-token" -Status "ok" -Detail "Client has a bearer-token source for authenticated AgentHub routes" -Path $Path
    }

    if ([string]$json.redis_url -ne $ExpectedRedisUrl) {
        Add-CheckResult -Name "client-config:redis-url" -Status "warn" -Detail "redis_url should be $ExpectedRedisUrl to avoid Windows localhost/IPv6 drift" -Path $Path
    }
    else {
        Add-CheckResult -Name "client-config:redis-url" -Status "ok" -Detail "redis_url is IPv4-loopback explicit" -Path $Path
    }

    if ([string]$json.database_url -ne $ExpectedDatabaseUrl) {
        Add-CheckResult -Name "client-config:database-url" -Status "warn" -Detail "database_url should be $ExpectedDatabaseUrl for local durability checks" -Path $Path
    }
    else {
        Add-CheckResult -Name "client-config:database-url" -Status "ok" -Detail "database_url is IPv4-loopback explicit" -Path $Path
    }
}

function Test-ExampleConfigs {
    $examplesRoot = Join-Path $repoRoot "examples/mcp"
    if (-not (Test-Path $examplesRoot)) {
        Add-CheckResult -Name "examples:mcp" -Status "missing" -Detail "examples/mcp not found" -Path $examplesRoot
        return
    }

    Get-ChildItem -Path $examplesRoot -File | ForEach-Object {
        $raw = Get-Content -Path $_.FullName -Raw
        if ($raw -match '192\.168\.|10\.|172\.(1[6-9]|2[0-9]|3[0-1])\.') {
            Add-CheckResult -Name "examples:numeric-url:$($_.Name)" -Status "warn" -Detail "Private numeric IP found in example; prefer localhost or <hostname>" -Path $_.FullName
        }
        if ($raw -match 'Bearer\s+[A-Za-z0-9_\-\.]{12,}' -or $raw -match '"auth_token"\s*:\s*"[A-Za-z0-9_\-\.]{12,}"') {
            Add-CheckResult -Name "examples:literal-token:$($_.Name)" -Status "fail" -Detail "Example appears to contain a literal bearer/auth token" -Path $_.FullName
        }
    }
    Add-CheckResult -Name "examples:mcp" -Status "ok" -Detail "Scanned examples/mcp for literal tokens and private numeric IPs" -Path $examplesRoot
}

foreach ($commandName in @("agent-bus", "agent-bus-mcp", "agent-bus-http")) {
    Get-AgentBusVersion -CommandName $commandName
}
Test-AgentBusInstallShadowing

$serviceAuthState = Get-AgentHubServiceAuthState -ServiceName "AgentHub"
if ($serviceAuthState.installed) {
    Add-CheckResult -Name "service:AgentHub" -Status "ok" -Detail "Service installed; authToken=$($serviceAuthState.authToken); allowRemote=$($serviceAuthState.allowRemote)" -Path $serviceAuthState.path
}
else {
    Add-CheckResult -Name "service:AgentHub" -Status "warn" -Detail "AgentHub service registry entry was not found" -Path $serviceAuthState.path
}

$homeBinCli = Join-Path $HomeDir "bin/agent-bus.exe"
if ($IsWindows -and -not (Test-Path $homeBinCli)) {
    Add-CheckResult -Name "install:home-bin-cli" -Status "warn" -Detail "Documented ~/bin/agent-bus.exe is missing; command may be resolving from another path" -Path $homeBinCli
}

Test-AgentBusClientConfig -Path (Join-Path $HomeDir ".config/agent-bus/config.json") -ServiceAuthState $serviceAuthState
Test-AgentBusJsonMcp -Path (Join-Path $HomeDir ".claude/mcp.json") -ClientName "claude"
Test-AgentBusJsonMcp -Path (Join-Path $HomeDir ".claude.json") -ClientName "claude-legacy"
Test-AgentBusJsonMcp -Path (Join-Path $HomeDir ".gemini/settings.json") -ClientName "gemini"
Test-JsonSyntax -Path (Join-Path $HomeDir ".antigravity/argv.json") | Out-Null
Test-TextConfig -Path (Join-Path $HomeDir ".codex/config.toml") -Needle "[mcp_servers.agent_bus]" -Label "codex:agent-bus"
Test-TextConfig -Path (Join-Path $HomeDir ".agents/AGENT_COORDINATION.md") -Needle "agent-bus" -Label "agents:coordination-doc"
Test-TextConfig -Path (Join-Path $HomeDir ".codex/AGENT_COORDINATION.md") -Needle "agent-bus" -Label "codex:coordination-doc"
Test-ExampleConfigs

if ($ExpectedRedisUrl -match 'localhost') {
    Add-CheckResult -Name "defaults:redis-url" -Status "warn" -Detail "Redis default uses localhost; prefer 127.0.0.1 for IPv4-only Redis on Windows"
}
else {
    Add-CheckResult -Name "defaults:redis-url" -Status "ok" -Detail "Redis default is loopback-family explicit"
}

if ($ExpectedDatabaseUrl -match 'localhost') {
    Add-CheckResult -Name "defaults:database-url" -Status "warn" -Detail "Database default uses localhost; prefer 127.0.0.1 when avoiding dual-stack ambiguity"
}
else {
    Add-CheckResult -Name "defaults:database-url" -Status "ok" -Detail "Database default is loopback-family explicit"
}

Add-CheckResult -Name "defaults:server-url" -Status "ok" -Detail "Expected MCP HTTP URL is $ExpectedServerUrl"

$results | Format-Table -AutoSize

$failures = @($results | Where-Object { $_.status -eq "fail" })
$warnings = @($results | Where-Object { $_.status -eq "warn" })
if ($failures.Count -gt 0 -or ($Strict -and $warnings.Count -gt 0)) {
    throw "Agent client config validation found $($failures.Count) failure(s) and $($warnings.Count) warning(s)."
}
