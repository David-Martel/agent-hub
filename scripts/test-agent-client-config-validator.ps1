param(
    [string]$CliPath = "",
    [string]$McpBinaryPath = ""
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$targetDir = if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    Join-Path $repoRoot "target"
}
else {
    $env:CARGO_TARGET_DIR
}
$releaseDir = Join-Path $targetDir "release"
if ([string]::IsNullOrWhiteSpace($CliPath)) {
    $CliPath = Join-Path $releaseDir "agent-bus$(if ($IsWindows) { '.exe' })"
}
if ([string]::IsNullOrWhiteSpace($McpBinaryPath)) {
    $McpBinaryPath = Join-Path $releaseDir "agent-bus-mcp$(if ($IsWindows) { '.exe' })"
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
        -McpSmokeTimeoutSeconds 10
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

$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "agent-bus-validator-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null

try {
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
            & (Join-Path $PSScriptRoot "install-mcp-clients.ps1") `
                -Claude:$false `
                -Codex:$true `
                -Gemini:$false `
                -CodexConfigPath $duplicateEnvironmentPath `
                -CommandPath $McpBinaryPath `
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
        & (Join-Path $PSScriptRoot "install-mcp-clients.ps1") `
            -Claude:$false `
            -Codex:$true `
            -Gemini:$false `
            -CodexConfigPath $preflightPath `
            -CommandPath $McpBinaryPath `
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

    Write-Output "Agent client config validator fixtures passed."
}
finally {
    Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}
