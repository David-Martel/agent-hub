param(
    [string]$Command = "agent-bus-mcp",
    [string[]]$ArgumentList = @(),
    [hashtable]$EnvironmentVariables = @{},
    [int]$TimeoutSeconds = 5,
    [string]$ExpectedProtocolVersion = "2024-11-05",
    [string]$ExpectedServerName = "agent-bus",
    [int]$ExpectedToolCount = 17,
    [switch]$WineTestLauncher
)

$ErrorActionPreference = "Stop"

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "PowerShell 7 or newer is required."
}
if ($TimeoutSeconds -lt 1) {
    throw "TimeoutSeconds must be at least 1."
}
if ($ExpectedToolCount -lt 1) {
    throw "ExpectedToolCount must be at least 1."
}
if ($WineTestLauncher -and ($ExpectedToolCount -ne 17 -or
    $ExpectedProtocolVersion -ne '2024-11-05' -or $ExpectedServerName -ne 'agent-bus')) {
    throw 'Wine fixture launch requires the unchanged agent-bus protocol and 17-tool registration.'
}

if ($WineTestLauncher) {
    if (-not $IsLinux -or [string]::IsNullOrWhiteSpace($env:AGENT_BUS_TEST_MCP_WINE_ROOT) -or
        -not (Test-Path -LiteralPath $Command -PathType Leaf)) {
        throw "Wine MCP launch requires Linux, a prepared private fixture and an actual configured EXE."
    }
    $resolvedCommand = [pscustomobject]@{ Source = [IO.Path]::GetFullPath($Command) }
    $winePython = Get-Command python3 -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $wineAdapter = Join-Path $PSScriptRoot "ci/mcp_wine_adapter.py"
}
else {
    $resolvedCommand = Get-Command $Command -CommandType Application -ErrorAction Stop |
        Select-Object -First 1
}

function Read-McpResponse {
    param(
        [Parameter(Mandatory = $true)]
        [System.Diagnostics.Process]$Process,
        [Parameter(Mandatory = $true)]
        [int]$ExpectedId,
        [Parameter(Mandatory = $true)]
        [string]$Stage,
        [Parameter(Mandatory = $true)]
        [int]$TimeoutMilliseconds
    )

    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    while ($deadline.ElapsedMilliseconds -lt $TimeoutMilliseconds) {
        $remaining = $TimeoutMilliseconds - [int]$deadline.ElapsedMilliseconds
        $readTask = $Process.StandardOutput.ReadLineAsync()
        if (-not $readTask.Wait($remaining)) {
            throw "$Stage response timeout."
        }

        $line = $readTask.Result
        if ($null -eq $line) {
            throw "$Stage ended before a response was received."
        }
        if ([string]::IsNullOrWhiteSpace($line)) {
            continue
        }

        try {
            $response = $line | ConvertFrom-Json -Depth 100
        }
        catch {
            throw "$Stage returned invalid JSON."
        }
        if ($response.id -eq $ExpectedId) {
            return $response
        }
    }

    throw "$Stage response timeout."
}

$processInfo = [System.Diagnostics.ProcessStartInfo]::new()
if ($WineTestLauncher) {
    $processInfo.FileName = $winePython.Source
    foreach ($argument in @('-B', $wineAdapter, 'bridge', '--root', $env:AGENT_BUS_TEST_MCP_WINE_ROOT,
        '--command', $resolvedCommand.Source, '--arguments-json', (ConvertTo-Json -InputObject @($ArgumentList) -Compress))) {
        $processInfo.ArgumentList.Add($argument)
    }
    # The configured environment is data; the adapter validates every accepted key,
    # maps private paths and supplies closed backend routes in a fresh guest environment.
    $processInfo.Environment.Clear()
    $processInfo.Environment['PATH'] = $env:PATH
    $processInfo.Environment['HOME'] = $env:AGENT_BUS_TEST_MCP_WINE_ROOT
    $processInfo.Environment['AGENT_BUS_TEST_MCP_CONFIGURED_ENV'] = ConvertTo-Json -InputObject $EnvironmentVariables -Compress
}
else {
    $processInfo.FileName = $resolvedCommand.Source
    foreach ($argument in $ArgumentList) {
        $processInfo.ArgumentList.Add($argument)
    }
}
$processInfo.UseShellExecute = $false
$processInfo.RedirectStandardInput = $true
$processInfo.RedirectStandardOutput = $true
$processInfo.RedirectStandardError = $true
$processInfo.CreateNoWindow = $true
foreach ($entry in $(if ($WineTestLauncher) { @() } else { $EnvironmentVariables.GetEnumerator() })) {
    if ($null -eq $entry.Value) {
        $processInfo.Environment.Remove([string]$entry.Key)
    }
    else {
        $processInfo.Environment[[string]$entry.Key] = [string]$entry.Value
    }
}
if (-not $EnvironmentVariables.ContainsKey("AGENT_BUS_STARTUP_ENABLED")) {
    $processInfo.Environment["AGENT_BUS_STARTUP_ENABLED"] = "false"
}
if (-not $EnvironmentVariables.ContainsKey("RUST_LOG")) {
    $processInfo.Environment["RUST_LOG"] = "error"
}

$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $processInfo
$processStarted = $false
$stderrTask = $null
$wineSuccess = $null

try {
    if (-not $process.Start()) {
        throw "Failed to start the MCP server."
    }
    $processStarted = $true
    $stderrTask = $process.StandardError.ReadToEndAsync()

    $initializeRequest = @{
        jsonrpc = "2.0"
        id      = 1
        method  = "initialize"
        params  = @{
            protocolVersion = $ExpectedProtocolVersion
            capabilities    = @{}
            clientInfo      = @{
                name    = "agent-bus-powershell-smoke"
                version = "1"
            }
        }
    } | ConvertTo-Json -Depth 8 -Compress
    $process.StandardInput.WriteLine($initializeRequest)
    $process.StandardInput.Flush()

    $initializeResponse = Read-McpResponse `
        -Process $process `
        -ExpectedId 1 `
        -Stage "initialize" `
        -TimeoutMilliseconds ($TimeoutSeconds * 1000)
    if ($initializeResponse.error) {
        throw "initialize returned a JSON-RPC error."
    }

    $protocolVersion = [string]$initializeResponse.result.protocolVersion
    $serverName = [string]$initializeResponse.result.serverInfo.name
    $serverVersion = [string]$initializeResponse.result.serverInfo.version
    if ($protocolVersion -ne $ExpectedProtocolVersion) {
        throw "Unexpected MCP protocol version '$protocolVersion'."
    }
    if ($serverName -ne $ExpectedServerName) {
        throw "Unexpected MCP server name '$serverName'."
    }
    if ([string]::IsNullOrWhiteSpace($serverVersion)) {
        throw "MCP server version is missing."
    }

    $initializedNotification = @{
        jsonrpc = "2.0"
        method  = "notifications/initialized"
        params  = @{}
    } | ConvertTo-Json -Depth 5 -Compress
    $toolsListRequest = @{
        jsonrpc = "2.0"
        id      = 2
        method  = "tools/list"
        params  = @{}
    } | ConvertTo-Json -Depth 5 -Compress
    $process.StandardInput.WriteLine($initializedNotification)
    $process.StandardInput.WriteLine($toolsListRequest)
    $process.StandardInput.Flush()

    $toolsListResponse = Read-McpResponse `
        -Process $process `
        -ExpectedId 2 `
        -Stage "tools/list" `
        -TimeoutMilliseconds ($TimeoutSeconds * 1000)
    if ($toolsListResponse.error) {
        throw "tools/list returned a JSON-RPC error."
    }

    $tools = @($toolsListResponse.result.tools)
    if ($tools.Count -ne $ExpectedToolCount) {
        throw "Unexpected MCP tool count '$($tools.Count)'; expected '$ExpectedToolCount'."
    }
    $toolNames = @($tools | ForEach-Object { [string]$_.name })
    if ($toolNames | Where-Object { [string]::IsNullOrWhiteSpace($_) }) {
        throw "One or more MCP tools have a missing name."
    }

    $success = [pscustomobject]@{
        ok              = $true
        command         = $resolvedCommand.Source
        protocolVersion = $protocolVersion
        serverName      = $serverName
        serverVersion   = $serverVersion
        toolCount       = $tools.Count
        tools           = $toolNames
    }
    if ($WineTestLauncher) {
        $wineSuccess = $success
    }
    else {
        $success | ConvertTo-Json -Depth 5 -Compress
    }
}
finally {
    if ($processStarted -and -not $process.HasExited) {
        try {
            $process.StandardInput.Close()
        }
        catch {
            Write-Verbose "MCP stdin was already closed during cleanup."
        }
        if ($WineTestLauncher) {
            # Guest/prefix settlement has its own bounded adapter contract. An exited
            # Unix launcher alone is not evidence that the Wine guest stopped.
            if (-not $process.WaitForExit(12000)) {
                $process.Kill()
                [void]$process.WaitForExit(2000)
                Set-Content -LiteralPath (Join-Path $env:AGENT_BUS_TEST_MCP_WINE_ROOT 'mcp-wine-HOLD') -Value 'launcher settlement failed' -Encoding utf8
                throw "Wine MCP launcher settlement failed; prefix retained."
            }
        }
        elseif (-not $process.WaitForExit(500)) {
            $process.Kill($true)
            [void]$process.WaitForExit(2000)
        }
    }
    if ($WineTestLauncher -and $processStarted) {
        if (-not $process.HasExited -or -not $stderrTask.Wait(2000)) {
            Set-Content -LiteralPath (Join-Path $env:AGENT_BUS_TEST_MCP_WINE_ROOT 'mcp-wine-HOLD') -Value 'guest or stream settlement failed' -Encoding utf8
            throw "Wine MCP guest or stream settlement failed; prefix retained."
        }
        if ($process.ExitCode -ne 0) {
            throw "Configured Wine MCP launch failed."
        }
    }
    if ($stderrTask -and $stderrTask.IsCompleted) {
        # Drain stderr without printing it; it may contain environment-derived details.
        [void]$stderrTask.Result
    }
    $process.Dispose()
}
if ($WineTestLauncher -and $wineSuccess) {
    $wineSuccess | ConvertTo-Json -Depth 5 -Compress
}
