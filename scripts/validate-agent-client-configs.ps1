param(
    [string]$HomeDir = $HOME,
    [string]$ExpectedServerUrl = "http://localhost:8400",
    [string]$ExpectedRedisUrl = "redis://127.0.0.1:6380/0",
    [string]$ExpectedDatabaseUrl = "postgresql://postgres@127.0.0.1:5300/redis_backend",
    [string]$MinimumAgentBusVersion = "0.5.0",
    [switch]$Strict,
    [switch]$SkipMcpSmoke,
    [int]$ExpectedMcpToolCount = 17,
    [int]$McpSmokeTimeoutSeconds = 5,
    [string]$CodexConfigPath = "",
    [switch]$CodexOnly
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$resolvedCodexConfigPath = if ([string]::IsNullOrWhiteSpace($CodexConfigPath)) {
    Join-Path $HomeDir ".codex/config.toml"
}
else {
    $CodexConfigPath
}
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

function Test-PrivateNumericEndpoint {
    param([Parameter(Mandatory = $true)][string]$Text)

    $privateIpv4 = '(?:192\.168\.|10\.|172\.(?:1[6-9]|2[0-9]|3[0-1])\.)'
    return ($Text -match "(?i)(?:https?://|ws://|wss://|AGENT_BUS_SERVER_URL\s*[:=]\s*['""]?|server_url\s*[:=]\s*['""]?|httpUrl\s*[:=]\s*['""]?)$privateIpv4")
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
        if (Test-PrivateNumericEndpoint -Text $raw) {
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
    if (Test-PrivateNumericEndpoint -Text $raw) {
        Add-CheckResult -Name "numeric-url:$Path" -Status "warn" -Detail "Private numeric IP found; prefer localhost or a stable hostname" -Path $Path
    }
}

function Get-AgentBusVersion {
    param(
        [Parameter(Mandatory = $true)][string]$CommandName,
        [Parameter(Mandatory = $true)][string]$MinimumVersion
    )

    $cmd = Get-Command $CommandName -ErrorAction SilentlyContinue
    if (-not $cmd) {
        Add-CheckResult -Name "binary:$CommandName" -Status "fail" -Detail "Command not found"
        return
    }

    Add-CheckResult -Name "binary:$CommandName" -Status "ok" -Detail "Resolved command" -Path $cmd.Source
    if ($CommandName -ne "agent-bus") {
        Add-CheckResult -Name "version:$CommandName" -Status "skipped" -Detail "Server binary is not invoked with --version; validate its runtime identity through the applicable smoke or health endpoint" -Path $cmd.Source
        return
    }
    try {
        $versionText = & $cmd.Source --version 2>$null | Select-Object -First 1
        if ($versionText -match '(\d+\.\d+\.\d+)') {
            $actual = [version]$Matches[1]
            $minimum = [version]$MinimumVersion
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

function ConvertFrom-TomlBasicString {
    param([Parameter(Mandatory = $true)][string]$Value)

    if ($Value -match '\\U[0-9A-Fa-f]{8}') {
        throw "TOML eight-digit Unicode escapes are not supported by this validator."
    }
    $withoutSupportedEscapes = [regex]::Replace(
        $Value,
        '\\(?:[btnfr"\\]|u[0-9A-Fa-f]{4})',
        ""
    )
    if ($withoutSupportedEscapes.Contains('\')) {
        throw "Unsupported or incomplete TOML basic-string escape."
    }
    return [System.Text.Json.JsonSerializer]::Deserialize(
        ('"' + $Value + '"'),
        [string]
    )
}

function Get-TomlSectionBody {
    param(
        [Parameter(Mandatory = $true)][string]$Document,
        [Parameter(Mandatory = $true)][string]$Name
    )

    $escapedName = [regex]::Escape($Name)
    $pattern = "(?ms)^\s*\[$escapedName\]\s*(?<body>.*?)(?=^\s*\[|\z)"
    $sections = [regex]::Matches($Document, $pattern)
    if ($sections.Count -gt 1) {
        throw "Multiple [$Name] TOML sections found."
    }
    if ($sections.Count -eq 0) {
        return $null
    }
    return $sections[0].Groups["body"].Value
}

function Get-TomlStringValue {
    param(
        [Parameter(Mandatory = $true)][string]$Body,
        [Parameter(Mandatory = $true)][string]$Name
    )

    $escapedName = [regex]::Escape($Name)
    $pattern = '(?m)^\s*{0}\s*=\s*(?:"(?<basic>(?:\\.|[^"\\])*)"|''(?<literal>[^'']*)'')\s*(?:#.*)?$' -f $escapedName
    $match = [regex]::Match($Body, $pattern)
    if (-not $match.Success) {
        return $null
    }
    if ($match.Groups["literal"].Success) {
        return $match.Groups["literal"].Value
    }
    return ConvertFrom-TomlBasicString -Value $match.Groups["basic"].Value
}

function Get-TomlValue {
    param(
        [Parameter(Mandatory = $true)][string]$Body,
        [Parameter(Mandatory = $true)][string]$Name
    )

    $escapedName = [regex]::Escape($Name)
    $pattern = @'
(?msx)
^\s*{0}\s*=\s*
(?<value>
    \[
        (?:
            "(?:\\.|[^"\\])*"
            | '(?:[^']*)'
            | \#[^\r\n]*(?:\r?\n|\z)
            | [^"'\]]
        )*
    \]
    | [^#\r\n]+
)
[ \t]*(?:\#[^\r\n]*)?\r?$
'@ -f $escapedName
    $match = [regex]::Match($Body, $pattern)
    if (-not $match.Success) {
        return $null
    }
    return $match.Groups["value"].Value.Trim()
}

function ConvertFrom-TomlStringArray {
    param([AllowNull()][string]$Value)

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return @()
    }
    if (-not $Value.Trim().StartsWith("[") -or -not $Value.Trim().EndsWith("]")) {
        throw "Expected a TOML array."
    }

    $source = $Value.Trim()
    $values = New-Object System.Collections.Generic.List[string]
    $index = 1
    $expectValue = $true
    while ($index -lt ($source.Length - 1)) {
        while ($index -lt ($source.Length - 1) -and [char]::IsWhiteSpace($source[$index])) {
            $index++
        }
        if ($index -ge ($source.Length - 1)) {
            break
        }
        if ($source[$index] -eq '#') {
            while ($index -lt ($source.Length - 1) -and $source[$index] -notin @("`r", "`n")) {
                $index++
            }
            continue
        }
        if ($source[$index] -eq ',') {
            if ($expectValue) {
                throw "Unexpected comma in TOML string array."
            }
            $expectValue = $true
            $index++
            continue
        }
        if (-not $expectValue) {
            throw "Missing comma in TOML string array."
        }

        $quote = $source[$index]
        if ($quote -notin @('"', "'")) {
            throw "Only quoted strings are supported in the Codex args array."
        }
        $index++
        $buffer = [System.Text.StringBuilder]::new()
        $closed = $false
        while ($index -lt ($source.Length - 1)) {
            $character = $source[$index]
            if ($quote -eq '"' -and $character -eq '\') {
                if (($index + 1) -ge ($source.Length - 1)) {
                    throw "Incomplete escape in TOML basic string."
                }
                [void]$buffer.Append($character)
                $index++
                [void]$buffer.Append($source[$index])
                $index++
                continue
            }
            if ($character -eq $quote) {
                $closed = $true
                $index++
                break
            }
            [void]$buffer.Append($character)
            $index++
        }
        if (-not $closed) {
            throw "Unterminated string in TOML args array."
        }

        if ($quote -eq '"') {
            $values.Add((ConvertFrom-TomlBasicString -Value $buffer.ToString()))
        }
        else {
            $values.Add($buffer.ToString())
        }
        $expectValue = $false
    }
    if ($expectValue -and $values.Count -gt 0) {
        # A trailing comma is valid TOML.
        return @($values)
    }
    return @($values)
}

function Get-TomlStringTable {
    param([AllowNull()][string]$Body)

    $table = @{}
    if ([string]::IsNullOrWhiteSpace($Body)) {
        return $table
    }

    $pattern = '^\s*(?<name>[A-Za-z0-9_-]+)\s*=\s*(?:"(?<basic>(?:\\.|[^"\\])*)"|''(?<literal>[^'']*)'')\s*(?:#.*)?$'
    foreach ($line in $Body -split '\r?\n') {
        if ([string]::IsNullOrWhiteSpace($line) -or $line -match '^\s*#') {
            continue
        }
        $match = [regex]::Match($line, $pattern)
        if (-not $match.Success) {
            throw "Unsupported or malformed entry in [mcp_servers.agent_bus.env]."
        }
        $name = $match.Groups["name"].Value
        if ($table.ContainsKey($name)) {
            throw "Duplicate '$name' entry in [mcp_servers.agent_bus.env]."
        }
        $value = if ($match.Groups["literal"].Success) {
            $match.Groups["literal"].Value
        }
        else {
            ConvertFrom-TomlBasicString -Value $match.Groups["basic"].Value
        }
        $table[$name] = $value
    }
    return $table
}

function Test-AgentBusDedicatedMcpCommand {
    param([Parameter(Mandatory = $true)][string]$Command)

    # Windows cannot replace a running executable. Atomic deployments may
    # therefore use a version-suffixed binary and repoint new MCP sessions
    # without terminating active agents.
    return $Command -match '(?i)(?:^|[\\/])agent-bus-mcp(?:-[0-9a-z][0-9a-z._-]*)?(?:\.exe)?$'
}

function Test-CodexAgentBusToml {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path $Path)) {
        Add-CheckResult -Name "codex:agent-bus" -Status "missing" -Detail "File not found" -Path $Path
        return
    }

    $raw = Get-Content -Path $Path -Raw
    if (Test-PrivateNumericEndpoint -Text $raw) {
        Add-CheckResult -Name "numeric-url:$Path" -Status "warn" -Detail "Private numeric IP found; prefer localhost or a stable hostname" -Path $Path
    }

    try {
        $body = Get-TomlSectionBody -Document $raw -Name "mcp_servers.agent_bus"
        $environmentBody = Get-TomlSectionBody -Document $raw -Name "mcp_servers.agent_bus.env"
        $environment = Get-TomlStringTable -Body $environmentBody
    }
    catch {
        Add-CheckResult -Name "codex:agent-bus-sections" -Status "fail" -Detail $_.Exception.Message -Path $Path
        return
    }
    if ($null -eq $body) {
        Add-CheckResult -Name "codex:agent-bus" -Status "warn" -Detail "agent_bus MCP server entry not found" -Path $Path
        return
    }

    $url = Get-TomlStringValue -Body $body -Name "url"
    $command = Get-TomlStringValue -Body $body -Name "command"
    $enabled = Get-TomlValue -Body $body -Name "enabled"
    $commandArgs = Get-TomlValue -Body $body -Name "args"
    $argsAssignmentPresent = $body -match '(?m)^\s*args\s*='
    if ($argsAssignmentPresent -and [string]::IsNullOrWhiteSpace($commandArgs)) {
        Add-CheckResult -Name "codex:args" -Status "fail" -Detail "The agent_bus args assignment is malformed or unsupported" -Path $Path
        return $null
    }
    if (
        $environment.ContainsKey("AGENT_BUS_AUTH_TOKEN") -and
        -not [string]::IsNullOrWhiteSpace([string]$environment["AGENT_BUS_AUTH_TOKEN"])
    ) {
        Add-CheckResult -Name "codex:inline-token-env" -Status "fail" -Detail "Active agent-bus MCP entry embeds AGENT_BUS_AUTH_TOKEN; use process env or ~/.config/agent-bus/config.json instead" -Path $Path
    }

    if ($enabled -and $enabled -match '^(?i:false)$') {
        Add-CheckResult -Name "codex:agent-bus-enabled" -Status "warn" -Detail "agent_bus MCP entry is disabled" -Path $Path
    }

    if (-not [string]::IsNullOrWhiteSpace($url)) {
        Add-CheckResult -Name "codex:agent-bus" -Status "ok" -Detail "agent_bus MCP entry found (streamable HTTP)" -Path $Path
        if ($url -match '^https?://(?:localhost|127\.0\.0\.1|\[?::1\]?)(?::|/|$)') {
            Add-CheckResult -Name "codex:transport" -Status "warn" -Detail "Local HTTP MCP depends on AgentHub startup; prefer the dedicated agent-bus-mcp stdio binary for same-machine Codex" -Path $Path
        }
        else {
            Add-CheckResult -Name "codex:transport" -Status "ok" -Detail "HTTP MCP uses a non-loopback endpoint" -Path $Path
        }
        return [pscustomobject]@{
            Transport = "http"
            Url       = $url
            Command   = $null
            Arguments = @()
            Environment = $environment
        }
    }

    if ([string]::IsNullOrWhiteSpace($command)) {
        Add-CheckResult -Name "codex:agent-bus" -Status "fail" -Detail "agent_bus MCP entry has neither url nor command" -Path $Path
        return $null
    }

    try {
        $arguments = @(ConvertFrom-TomlStringArray -Value $commandArgs)
    }
    catch {
        Add-CheckResult -Name "codex:args" -Status "fail" -Detail "Could not parse agent_bus args: $($_.Exception.Message)" -Path $Path
        return $null
    }
    Add-CheckResult -Name "codex:agent-bus" -Status "ok" -Detail "agent_bus MCP entry found (stdio)" -Path $Path
    if (Test-AgentBusDedicatedMcpCommand -Command $command) {
        Add-CheckResult -Name "codex:transport" -Status "ok" -Detail "Uses the dedicated agent-bus-mcp stdio binary" -Path $Path
    }
    elseif (
        $command -match '(?i)(?:^|[\\/])agent-bus(?:\.exe)?$' -and
        $arguments.Count -ge 3 -and
        $arguments[0] -eq "serve" -and
        $arguments[1] -eq "--transport" -and
        $arguments[2] -eq "stdio"
    ) {
        Add-CheckResult -Name "codex:transport" -Status "warn" -Detail "Uses the compatible agent-bus serve --transport stdio fallback instead of the dedicated MCP binary" -Path $Path
    }
    else {
        Add-CheckResult -Name "codex:transport" -Status "fail" -Detail "Stdio command is not the dedicated agent-bus-mcp binary or the supported CLI fallback" -Path $Path
    }
    return [pscustomobject]@{
        Transport = "stdio"
        Url       = $null
        Command   = $command
        Arguments = $arguments
        Environment = $environment
    }
}

function Test-AgentBusMcpSmoke {
    param(
        [switch]$Skip,
        $ActiveTransport,
        [Parameter(Mandatory = $true)][int]$ExpectedToolCount,
        [Parameter(Mandatory = $true)][int]$TimeoutSeconds
    )

    $smokeScript = Join-Path $PSScriptRoot "test-agent-bus-mcp-smoke.ps1"
    if ($Skip) {
        Add-CheckResult -Name "mcp-smoke:stdio" -Status "skipped" -Detail "Skipped by -SkipMcpSmoke" -Path $smokeScript
        return
    }
    if ($null -eq $ActiveTransport) {
        Add-CheckResult -Name "mcp-smoke:active" -Status "fail" -Detail "Active Codex agent_bus transport could not be resolved" -Path $smokeScript
        return
    }
    if ($ActiveTransport.Transport -ne "stdio") {
        Add-CheckResult -Name "mcp-smoke:active" -Status "warn" -Detail "Active Codex transport is HTTP; stdio smoke was not substituted for the configured endpoint" -Path $ActiveTransport.Url
        return
    }
    if (-not (Test-Path $smokeScript)) {
        Add-CheckResult -Name "mcp-smoke:stdio" -Status "fail" -Detail "MCP smoke script not found" -Path $smokeScript
        return
    }

    $mcpCommand = Get-Command $ActiveTransport.Command -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $mcpCommand) {
        Add-CheckResult -Name "mcp-smoke:active" -Status "fail" -Detail "Configured Codex MCP command not found" -Path $ActiveTransport.Command
        return
    }

    try {
        $smokeOutput = & $smokeScript `
            -Command $mcpCommand.Source `
            -ArgumentList @($ActiveTransport.Arguments) `
            -EnvironmentVariables $ActiveTransport.Environment `
            -TimeoutSeconds $TimeoutSeconds `
            -ExpectedToolCount $ExpectedToolCount
        $smoke = $smokeOutput | ConvertFrom-Json -Depth 100
        if (-not $smoke.ok) {
            throw "MCP smoke did not report success."
        }
        Add-CheckResult `
            -Name "mcp-smoke:active" `
            -Status "ok" `
            -Detail "Initialize/tools-list passed; protocol=$($smoke.protocolVersion), server=$($smoke.serverName) $($smoke.serverVersion), tools=$($smoke.toolCount)" `
            -Path $mcpCommand.Source
    }
    catch {
        Add-CheckResult -Name "mcp-smoke:active" -Status "fail" -Detail "Configured Codex MCP stdio smoke failed: $($_.Exception.Message)" -Path $mcpCommand.Source
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

    if ($entry.ContainsKey("httpUrl")) {
        $httpUrl = [string]$entry["httpUrl"]
        if ($httpUrl -match 'http://(192\.168\.|10\.|172\.(1[6-9]|2[0-9]|3[0-1])\.)') {
            Add-CheckResult -Name "${ClientName}:agent-bus-http-url" -Status "warn" -Detail "Active agent-bus MCP URL uses a private numeric IP; prefer localhost or a stable tailnet/headscale hostname" -Path $Path
        }
        elseif ($httpUrl -match '^http://localhost(:|/)') {
            Add-CheckResult -Name "${ClientName}:agent-bus-http-url" -Status "ok" -Detail "Active agent-bus MCP URL uses localhost" -Path $Path
        }
    }

    if ($entry.ContainsKey("headers")) {
        $headers = $entry["headers"]
        foreach ($key in @("Authorization", "authorization")) {
            if ($headers.ContainsKey($key) -and ([string]$headers[$key]) -match '^Bearer\s+\S+') {
                Add-CheckResult -Name "${ClientName}:inline-token" -Status "fail" -Detail "Active agent-bus MCP entry contains an inline bearer token; use AGENT_BUS_AUTH_TOKEN or the agent-bus config file" -Path $Path
            }
        }
    }

    if ($entry.ContainsKey("env")) {
        $entryEnv = $entry["env"]
        if ($entryEnv.ContainsKey("AGENT_BUS_AUTH_TOKEN") -and -not [string]::IsNullOrWhiteSpace([string]$entryEnv["AGENT_BUS_AUTH_TOKEN"])) {
            Add-CheckResult -Name "${ClientName}:inline-token-env" -Status "fail" -Detail "Active agent-bus MCP entry embeds AGENT_BUS_AUTH_TOKEN; use process env or ~/.config/agent-bus/config.json instead" -Path $Path
        }
    }

    if ($entry.ContainsKey("args")) {
        foreach ($arg in @($entry["args"])) {
            if ([string]$arg -match 'Bearer\s+\S+' -or [string]$arg -match 'AGENT_BUS_AUTH_TOKEN=') {
                Add-CheckResult -Name "${ClientName}:inline-token-args" -Status "fail" -Detail "Active agent-bus MCP args appear to contain bearer/auth-token material" -Path $Path
            }
        }
    }

    if ($entry.ContainsKey("command")) {
        $command = [string]$entry["command"]
        if (Test-AgentBusDedicatedMcpCommand -Command $command) {
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

function Test-ExampleConfig {
    $examplesRoot = Join-Path $repoRoot "examples/mcp"
    if (-not (Test-Path $examplesRoot)) {
        Add-CheckResult -Name "examples:mcp" -Status "missing" -Detail "examples/mcp not found" -Path $examplesRoot
        return
    }

    Get-ChildItem -Path $examplesRoot -File | ForEach-Object {
        $raw = Get-Content -Path $_.FullName -Raw
        if (Test-PrivateNumericEndpoint -Text $raw) {
            Add-CheckResult -Name "examples:numeric-url:$($_.Name)" -Status "warn" -Detail "Private numeric IP found in example; prefer localhost or <hostname>" -Path $_.FullName
        }
        if ($raw -match 'Bearer\s+[A-Za-z0-9_\-\.]{12,}' -or $raw -match '"auth_token"\s*:\s*"[A-Za-z0-9_\-\.]{12,}"') {
            Add-CheckResult -Name "examples:literal-token:$($_.Name)" -Status "fail" -Detail "Example appears to contain a literal bearer/auth token" -Path $_.FullName
        }
    }
    Add-CheckResult -Name "examples:mcp" -Status "ok" -Detail "Scanned examples/mcp for literal tokens and private numeric IPs" -Path $examplesRoot
}

$codexMcpTransport = Test-CodexAgentBusToml -Path $resolvedCodexConfigPath
Test-AgentBusMcpSmoke `
    -Skip:$SkipMcpSmoke `
    -ActiveTransport $codexMcpTransport `
    -ExpectedToolCount $ExpectedMcpToolCount `
    -TimeoutSeconds $McpSmokeTimeoutSeconds

if (-not $CodexOnly) {
    foreach ($commandName in @("agent-bus", "agent-bus-mcp", "agent-bus-http")) {
        Get-AgentBusVersion -CommandName $commandName -MinimumVersion $MinimumAgentBusVersion
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
    Test-TextConfig -Path (Join-Path $HomeDir ".agents/AGENT_COORDINATION.md") -Needle "agent-bus" -Label "agents:coordination-doc"
    Test-TextConfig -Path (Join-Path $HomeDir ".codex/AGENT_COORDINATION.md") -Needle "agent-bus" -Label "codex:coordination-doc"
    Test-ExampleConfig

    if ($ExpectedRedisUrl -match 'localhost') {
        Add-CheckResult -Name "defaults:redis-url" -Status "ok" -Detail "Redis default uses the portable localhost route"
    }
    else {
        Add-CheckResult -Name "defaults:redis-url" -Status "warn" -Detail "Redis default uses a numeric loopback; prefer localhost for portable client routing"
    }

    if ($ExpectedDatabaseUrl -match 'localhost') {
        Add-CheckResult -Name "defaults:database-url" -Status "ok" -Detail "Database default uses the portable localhost route"
    }
    else {
        Add-CheckResult -Name "defaults:database-url" -Status "warn" -Detail "Database default uses a numeric loopback; prefer localhost for portable client routing"
    }

    Add-CheckResult -Name "defaults:server-url" -Status "ok" -Detail "Expected MCP HTTP URL is $ExpectedServerUrl"
}

$results | Format-Table -AutoSize

$failures = @($results | Where-Object { $_.status -eq "fail" })
$warnings = @($results | Where-Object { $_.status -eq "warn" })
if ($failures.Count -gt 0 -or ($Strict -and $warnings.Count -gt 0)) {
    throw "Agent client config validation found $($failures.Count) failure(s) and $($warnings.Count) warning(s)."
}
