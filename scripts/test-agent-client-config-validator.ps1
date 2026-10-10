param(
    [string]$CliPath = "",
    [string]$McpBinaryPath = "",
    [switch]$WineTestLauncher
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$targetDir = if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    Join-Path $repoRoot "target"
}
else {
    $env:CARGO_TARGET_DIR
}
$releaseDir = Join-Path $targetDir $(if ($WineTestLauncher) { 'x86_64-pc-windows-gnu/release' } else { 'release' })
if ([string]::IsNullOrWhiteSpace($CliPath)) {
    $CliPath = Join-Path $releaseDir "agent-bus$(if ($IsWindows -or $WineTestLauncher) { '.exe' })"
}
if ([string]::IsNullOrWhiteSpace($McpBinaryPath)) {
    $McpBinaryPath = Join-Path $releaseDir "agent-bus-mcp$(if ($IsWindows -or $WineTestLauncher) { '.exe' })"
}
foreach ($binary in @($CliPath, $McpBinaryPath)) {
    if (-not (Test-Path -LiteralPath $binary)) {
        throw "Required validator fixture binary not found: $binary"
    }
}

function ConvertTo-TomlBasicString {
    param([Parameter(Mandatory = $true)][string]$Value)

    return $Value.Replace('\', '\\').Replace('"', '\"')
}

function Invoke-CodexFixtureValidation {
    param(
        [Parameter(Mandatory = $true)][string]$ConfigPath,
        [switch]$Strict
    )

    & (Join-Path $PSScriptRoot "validate-agent-client-configs.ps1") `
        -CodexConfigPath $ConfigPath `
        -CodexOnly `
        -Strict:$Strict `
        -McpSmokeTimeoutSeconds 10 `
        -WineTestLauncher:$WineTestLauncher
}

function Assert-ConfigRejected {
    param(
        [Parameter(Mandatory = $true)][string]$ConfigPath,
        [string]$ForbiddenOutput = ""
    )

    $rejected = $false
    $output = ""
    try {
        $output = (
            Invoke-CodexFixtureValidation -ConfigPath $ConfigPath -Strict |
                Out-String
        )
    }
    catch {
        $rejected = $true
        $output += $_.Exception.Message
    }
    if (-not $rejected) {
        throw "Validator accepted invalid Codex MCP config '$ConfigPath'."
    }
    if (
        -not [string]::IsNullOrEmpty($ForbiddenOutput) -and
        $output.Contains($ForbiddenOutput, [System.StringComparison]::Ordinal)
    ) {
        throw "Validator output disclosed forbidden fixture material."
    }
}

function Invoke-OwnedWineFixtureOperation {
    param([ValidateSet('prepare', 'finalize')][string]$Operation)

    $clock = [Diagnostics.Stopwatch]::StartNew()
    # Separate fixture bootstrap: existing provider wineboot (120s) plus prefix
    # wait (10s) and bounded drainage. MCP response deadlines remain 10s.
    $budget = if ($Operation -eq 'prepare') { 140000 } else { 12000 }
    $python = Get-Command python3 -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $python.Source
    foreach ($value in @('-B', $wineAdapter, $Operation, '--root', $fixtureRoot)) {
        $info.ArgumentList.Add($value)
    }
    $info.Environment.Clear()
    $info.Environment['PATH'] = $env:PATH
    $info.Environment['HOME'] = $fixtureRoot
    $info.Environment['TMPDIR'] = $fixtureRoot
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $client = [Diagnostics.Process]::new()
    $client.StartInfo = $info
    $started = $false
    $retain = $false
    $failure = $null
    $output = $null
    $errorOutput = $null
    $identity = $null
    $childStartTicks = $null
    $settled = $false
    $directKillAttempted = $false
    $directKilled = $false
    try {
        if (-not $client.Start()) { throw 'Owned Wine fixture client failed to start.' }
        $started = $true
        $identity = $client.Id
        try { $childStartTicks = $client.StartTime.ToUniversalTime().Ticks } catch { }
        $output = $client.StandardOutput.ReadToEndAsync()
        $errorOutput = $client.StandardError.ReadToEndAsync()
        $remaining = [Math]::Max(0, $budget - [int]$clock.ElapsedMilliseconds)
        if (-not $client.WaitForExit($remaining) -or $clock.ElapsedMilliseconds -ge $budget) {
            $failure = 'Owned Wine fixture client deadline expired; prefix retained.'
            if (-not $client.HasExited) {
                # Only the exact newly started Process object; never a tree or PID search.
                $directKillAttempted = $true
                try { $client.Kill(); $directKilled = $true } catch { }
            }
        }
        # One shared settlement budget covers both direct-child exit and both pipes.
        $settlement = [Diagnostics.Stopwatch]::StartNew()
        $exited = $client.WaitForExit(2000)
        [Threading.Tasks.Task[]]$tasks = @($output, $errorOutput)
        $drained = [Threading.Tasks.Task]::WaitAll($tasks, [Math]::Max(0, 2000 - [int]$settlement.ElapsedMilliseconds))
        $settled = $exited -and $drained
        $retain = -not $settled
        if (-not $failure -and $clock.ElapsedMilliseconds -ge $budget) {
            $failure = 'Owned Wine fixture client deadline expired; prefix retained.'
        }
        if (-not $failure -and (-not $settled -or $client.ExitCode -ne 0)) {
            $failure = 'Owned Wine fixture operation did not settle; prefix retained.'
        }
        if ($failure) {
            throw $failure
        }
        # Never print stderr: guest output may contain configured fixture material.
        $result = $output.Result | ConvertFrom-Json
        if (($Operation -eq 'prepare' -and -not $result.prepared) -or
            ($Operation -eq 'finalize' -and -not $result.settled)) {
            throw 'Owned Wine fixture receipt is incomplete.'
        }
        # Receipt parsing is inside the same admitted operation budget. A fully
        # exited, drained client cannot pass after its deadline.
        if ($clock.ElapsedMilliseconds -ge $budget) {
            throw 'Owned Wine fixture client deadline expired; prefix retained.'
        }
    }
    catch {
        $errorRecord = $_
        $retain = $started -and (-not $client.HasExited -or
            ($output -and -not $output.IsCompleted) -or ($errorOutput -and -not $errorOutput.IsCompleted))
        $metadata = [ordered]@{
            operation = $Operation
            original_child_pid = $identity
            original_child_start_ticks = $childStartTicks
            elapsed_ms = $clock.ElapsedMilliseconds
            deadline_expired = ($clock.ElapsedMilliseconds -ge $budget)
            child_exited = ($started -and $client.HasExited)
            exit_code = $(if ($started -and $client.HasExited) { $client.ExitCode } else { $null })
            direct_child_kill_attempted = $directKillAttempted
            direct_child_killed = $directKilled
            stdout_complete = ($null -ne $output -and $output.IsCompletedSuccessfully)
            stderr_complete = ($null -ne $errorOutput -and $errorOutput.IsCompletedSuccessfully)
            handles_retained = $retain
        }
        $errorRecord.Exception.Data['OwnedWineFixtureClient'] = $metadata
        if ($retain) {
            if (-not (Get-Variable AgentBusUnsettledWineFixtureClients -Scope Script -ErrorAction SilentlyContinue)) {
                $script:AgentBusUnsettledWineFixtureClients = [Collections.Generic.List[object]]::new()
            }
            $script:AgentBusUnsettledWineFixtureClients.Add([pscustomobject]@{
                Client = $client; OutputTask = $output; ErrorTask = $errorOutput; Metadata = $metadata
            })
        }
        throw
    }
    finally {
        if (-not $retain) { $client.Dispose() }
    }
}

$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "agent-bus-validator-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
$wineRootBefore = [Environment]::GetEnvironmentVariable('AGENT_BUS_TEST_MCP_WINE_ROOT', 'Process')
$wineAdapter = Join-Path $PSScriptRoot 'ci/mcp_wine_adapter.py'
$winePrepared = $false
$wineSettled = $false

try {
    if ($WineTestLauncher) {
        if (-not $IsLinux) { throw 'Wine configured MCP fixtures require Linux.' }
        [IO.File]::SetUnixFileMode($fixtureRoot, [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute)
        $env:AGENT_BUS_TEST_MCP_WINE_ROOT = $fixtureRoot
        # Separate setup; real initialize/tools-list deadlines remain unchanged.
        Invoke-OwnedWineFixtureOperation -Operation prepare
        $winePrepared = $true
    }
    # Run the real installer unchanged, with its adjacent validator forwarding
    # explicitly to the real config/parser/stdio checks for this fixture only.
    # Full machine-install auditing belongs to deployment validation, not this test.
    $fixtureInstallerRoot = Join-Path $fixtureRoot "installer"
    New-Item -ItemType Directory -Path $fixtureInstallerRoot | Out-Null
    $fixtureInstaller = Join-Path $fixtureInstallerRoot "install-mcp-clients.ps1"
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "install-mcp-clients.ps1") -Destination $fixtureInstaller
    if ((Get-FileHash -LiteralPath $fixtureInstaller).Hash -cne
        (Get-FileHash -LiteralPath (Join-Path $PSScriptRoot "install-mcp-clients.ps1")).Hash) {
        throw "Fixture installer copy differs from the real installer."
    }
    Write-Output "Fixture installer byte-copy verified."
    $validatorSource = (Join-Path $PSScriptRoot "validate-agent-client-configs.ps1").Replace("'", "''")
    $wineProxySwitch = if ($WineTestLauncher) { '-WineTestLauncher' } else { '' }
    @"
[CmdletBinding()]
param(
    [Parameter(Mandatory = `$true)][string]`$CodexConfigPath,
    [Parameter(Mandatory = `$true)][string]`$ExpectedServerUrl,
    [Parameter(Mandatory = `$true)][string]`$ExpectedRedisUrl,
    [Parameter(Mandatory = `$true)][string]`$ExpectedDatabaseUrl
)
& '$validatorSource' -CodexOnly -CodexConfigPath `$CodexConfigPath -ExpectedServerUrl `$ExpectedServerUrl -ExpectedRedisUrl `$ExpectedRedisUrl -ExpectedDatabaseUrl `$ExpectedDatabaseUrl $wineProxySwitch
"@ | Set-Content -LiteralPath (Join-Path $fixtureInstallerRoot "validate-agent-client-configs.ps1") -Encoding utf8

    $multilinePath = Join-Path $fixtureRoot "multiline-args.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $CliPath)"
args = [
    "serve",
    # The validator must retain arguments after newlines, but ignore "--debug" here.
    "--transport",
    "stdio",
]

[mcp_servers.agent_bus.env]
AGENT_BUS_STARTUP_ENABLED = "false"
RUST_LOG = "error"
"@ | Set-Content -LiteralPath $multilinePath -Encoding utf8
    Invoke-CodexFixtureValidation -ConfigPath $multilinePath

    $lfMultilinePath = Join-Path $fixtureRoot "multiline-args-lf.toml"
    (Get-Content -LiteralPath $multilinePath -Raw).Replace("`r`n", "`n") |
        Set-Content -LiteralPath $lfMultilinePath -Encoding utf8 -NoNewline
    Invoke-CodexFixtureValidation -ConfigPath $lfMultilinePath

    $authConfigPath = Join-Path $fixtureRoot "authenticated-client.json"
    '{"auth_token":"validator-fixture-token"}' |
        Set-Content -LiteralPath $authConfigPath -Encoding utf8
    $validEnvironmentPath = Join-Path $fixtureRoot "valid-environment.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $McpBinaryPath)"
args = []

[mcp_servers.agent_bus.env]
AGENT_BUS_SERVER_HOST = "0.0.0.0"
AGENT_BUS_ALLOW_REMOTE = "true"
AGENT_BUS_CONFIG = "$(ConvertTo-TomlBasicString -Value $authConfigPath)"
AGENT_BUS_SERVICE_AGENT_ID = "fixture # value = spaces with \\ slash and \"quote\""
AGENT_BUS_STARTUP_ENABLED = "false"
RUST_LOG = "error"
"@ | Set-Content -LiteralPath $validEnvironmentPath -Encoding utf8
    Invoke-CodexFixtureValidation -ConfigPath $validEnvironmentPath -Strict

    $versionedMcpPath = Join-Path $fixtureRoot "agent-bus-mcp-fd70c8d$(if ($IsWindows -or $WineTestLauncher) { '.exe' })"
    Copy-Item -LiteralPath $McpBinaryPath -Destination $versionedMcpPath
    $versionedMcpConfigPath = Join-Path $fixtureRoot "versioned-mcp-command.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $versionedMcpPath)"
args = []

[mcp_servers.agent_bus.env]
AGENT_BUS_STARTUP_ENABLED = "false"
RUST_LOG = "error"
"@ | Set-Content -LiteralPath $versionedMcpConfigPath -Encoding utf8
    Invoke-CodexFixtureValidation -ConfigPath $versionedMcpConfigPath -Strict

    $invalidMcpNamePath = Join-Path $fixtureRoot "agent-bus-mcp-$(if ($IsWindows -or $WineTestLauncher) { '.exe' })"
    Copy-Item -LiteralPath $McpBinaryPath -Destination $invalidMcpNamePath
    $invalidMcpNameConfigPath = Join-Path $fixtureRoot "invalid-versioned-mcp-command.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $invalidMcpNamePath)"
args = []

[mcp_servers.agent_bus.env]
AGENT_BUS_STARTUP_ENABLED = "false"
RUST_LOG = "error"
"@ | Set-Content -LiteralPath $invalidMcpNameConfigPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $invalidMcpNameConfigPath

    $invalidEnvironmentPath = Join-Path $fixtureRoot "invalid-environment.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $McpBinaryPath)"
args = []

[mcp_servers.agent_bus.env]
AGENT_BUS_SERVER_HOST = "0.0.0.0"
AGENT_BUS_ALLOW_REMOTE = "false"
AGENT_BUS_CONFIG = "$(ConvertTo-TomlBasicString -Value (Join-Path $fixtureRoot 'missing-client.json'))"
AGENT_BUS_STARTUP_ENABLED = "false"
RUST_LOG = "error"
"@ | Set-Content -LiteralPath $invalidEnvironmentPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $invalidEnvironmentPath

    $missingCommaPath = Join-Path $fixtureRoot "missing-comma.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $CliPath)"
args = ["serve" "--transport", "stdio"]
"@ | Set-Content -LiteralPath $missingCommaPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $missingCommaPath

    $bareArgumentPath = Join-Path $fixtureRoot "bare-argument.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $CliPath)"
args = ["serve", --transport, "stdio"]
"@ | Set-Content -LiteralPath $bareArgumentPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $bareArgumentPath

    $emptyAssignmentPath = Join-Path $fixtureRoot "empty-args-assignment.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $McpBinaryPath)"
args = # Invalid TOML must not be treated as an absent args key.
"@ | Set-Content -LiteralPath $emptyAssignmentPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $emptyAssignmentPath

    $wrongFallbackOrderPath = Join-Path $fixtureRoot "wrong-fallback-order.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $CliPath)"
args = ["serve", "stdio", "--transport"]
"@ | Set-Content -LiteralPath $wrongFallbackOrderPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $wrongFallbackOrderPath

    $duplicateEnvironmentPath = Join-Path $fixtureRoot "duplicate-environment.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $McpBinaryPath)"
args = []

[mcp_servers.agent_bus.env]
RUST_LOG = "error"
RUST_LOG = "debug"
"@ | Set-Content -LiteralPath $duplicateEnvironmentPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $duplicateEnvironmentPath

    $nonStringEnvironmentPath = Join-Path $fixtureRoot "non-string-environment.toml"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $McpBinaryPath)"
args = []

[mcp_servers.agent_bus.env]
AGENT_BUS_STARTUP_ENABLED = false
"@ | Set-Content -LiteralPath $nonStringEnvironmentPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $nonStringEnvironmentPath

    $inlineTokenPath = Join-Path $fixtureRoot "inline-token.toml"
    $inlineToken = "must-not-appear-validator-token"
    @"
[mcp_servers.agent_bus]
command = "$(ConvertTo-TomlBasicString -Value $McpBinaryPath)"
args = []

[mcp_servers.agent_bus.env]
AGENT_BUS_AUTH_TOKEN = "$inlineToken"
AGENT_BUS_STARTUP_ENABLED = "false"
"@ | Set-Content -LiteralPath $inlineTokenPath -Encoding utf8
    Assert-ConfigRejected -ConfigPath $inlineTokenPath -ForbiddenOutput $inlineToken

    $customPathRejected = $false
    try {
        $null = (
            & $fixtureInstaller `
                -Claude:$false `
                -Codex:$true `
                -Gemini:$false `
                -CodexConfigPath $duplicateEnvironmentPath `
                -CommandPath $McpBinaryPath `
                -RedisUrl "redis://localhost:1/0" `
                -DatabaseUrl "postgresql://postgres@localhost:1/validator_fixture" `
                -ServerUrl "http://localhost:1" `
                -ValidateOnly |
                Out-String
        )
    }
    catch {
        $customPathRejected = $true
    }
    if (-not $customPathRejected) {
        throw "ValidateOnly did not validate the requested custom Codex config path."
    }

    $preflightPath = Join-Path $fixtureRoot "preflight-no-mutation.toml"
    $preflightMarker = "# preserve-before-preflight"
    $preflightMarker | Set-Content -LiteralPath $preflightPath -Encoding utf8
    $unsafeHostRejected = $false
    try {
        & $fixtureInstaller `
            -Claude:$false `
            -Codex:$true `
            -Gemini:$false `
            -CodexConfigPath $preflightPath `
            -CommandPath $McpBinaryPath `
            -RedisUrl "redis://localhost:1/0" `
            -DatabaseUrl "postgresql://postgres@localhost:1/validator_fixture" `
            -ServerUrl "http://localhost:1" `
            -ServerHost "0.0.0.0"
    }
    catch {
        $unsafeHostRejected = $true
    }
    if (-not $unsafeHostRejected) {
        throw "Installer accepted a non-loopback stdio ServerHost."
    }
    if ((Get-Content -LiteralPath $preflightPath -Raw).Trim() -ne $preflightMarker) {
        throw "Installer mutated the Codex config before rejecting an unsafe ServerHost."
    }

    $managedSuffixPath = Join-Path $fixtureRoot "managed-suffix.toml"
    @"
model = "gpt-5.6-sol"

# BEGIN agent-bus MCP (managed by scripts/install-mcp-clients.ps1)
[mcp_servers.agent_bus]
command = "stale"
args = []

[mcp_servers.agent_bus.env]
AGENT_BUS_STARTUP_ENABLED = "false"
# END agent-bus MCP (managed by scripts/install-mcp-clients.ps1)

[mcp_servers.agent_bus.tools.post_message]
approval_mode = "approve"

[windows]
sandbox = "elevated"

[tui]
status_line = ["model"]
"@ | Set-Content -LiteralPath $managedSuffixPath -Encoding utf8

    & $fixtureInstaller `
        -Claude:$false `
        -Codex:$true `
        -Gemini:$false `
        -CodexConfigPath $managedSuffixPath `
        -CommandPath $McpBinaryPath `
        -RedisUrl "redis://localhost:1/0" `
        -DatabaseUrl "postgresql://postgres@localhost:1/validator_fixture" `
        -ServerUrl "http://localhost:1" `
        -NoBackup

    $managedSuffixContent = Get-Content -LiteralPath $managedSuffixPath -Raw
    if ($managedSuffixContent -notmatch '(?m)^\[windows\]\r?$' -or $managedSuffixContent -notmatch '(?m)^\[tui\]\r?$') {
        throw "Installer removed configuration following the managed agent-bus block."
    }
    if ([regex]::Matches($managedSuffixContent, '(?m)^# BEGIN agent-bus MCP').Count -ne 1) {
        throw "Installer did not produce exactly one managed agent-bus block."
    }
    if ($managedSuffixContent.IndexOf('# END agent-bus MCP') -gt $managedSuffixContent.IndexOf('[mcp_servers.agent_bus.tools.post_message]')) {
        throw "Installer moved the managed parent tables after agent-bus tool configuration."
    }

    $legacyIndentedSuffixPath = Join-Path $fixtureRoot "legacy-indented-suffix.toml"
    @"
model = "gpt-5.6-sol"

# Shared Redis-backed agent coordination bus
[mcp_servers.agent_bus]
command = "stale"
args = []

[mcp_servers.agent_bus.env]
AGENT_BUS_STARTUP_ENABLED = "false"

  [windows]
sandbox = "elevated"

  [tui]
status_line = ["model"]
"@ | Set-Content -LiteralPath $legacyIndentedSuffixPath -Encoding utf8

    & $fixtureInstaller `
        -Claude:$false `
        -Codex:$true `
        -Gemini:$false `
        -CodexConfigPath $legacyIndentedSuffixPath `
        -CommandPath $McpBinaryPath `
        -RedisUrl "redis://localhost:1/0" `
        -DatabaseUrl "postgresql://postgres@localhost:1/validator_fixture" `
        -ServerUrl "http://localhost:1" `
        -NoBackup

    $legacyIndentedSuffixContent = Get-Content -LiteralPath $legacyIndentedSuffixPath -Raw
    if ($legacyIndentedSuffixContent -notmatch '(?m)^\s+\[windows\]\r?$' -or
        $legacyIndentedSuffixContent -notmatch '(?m)^\s+\[tui\]\r?$') {
        throw "Installer removed an indented TOML section following a legacy agent-bus block."
    }
    if ([regex]::Matches($legacyIndentedSuffixContent, '(?m)^# BEGIN agent-bus MCP').Count -ne 1) {
        throw "Legacy upgrade did not produce exactly one managed agent-bus block."
    }

    if (-not $WineTestLauncher) {
        Write-Output "Agent client config validator fixtures passed."
    }
}
finally {
    try {
        if ($WineTestLauncher -and $winePrepared) {
            Invoke-OwnedWineFixtureOperation -Operation finalize
            $wineSettled = $true
        }
        if (-not $WineTestLauncher -or $wineSettled) {
            Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    finally {
        [Environment]::SetEnvironmentVariable('AGENT_BUS_TEST_MCP_WINE_ROOT', $wineRootBefore, 'Process')
    }
}
if ($WineTestLauncher -and $wineSettled) {
    Write-Output "Agent client config validator fixtures passed."
}
