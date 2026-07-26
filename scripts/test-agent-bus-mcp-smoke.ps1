param(
    [string]$Command = "agent-bus-mcp",
    [string[]]$ArgumentList = @(),
    [int]$TimeoutSeconds = 5,
    [string]$ExpectedProtocolVersion = "2024-11-05",
    [string]$ExpectedServerName = "agent-bus",
    [int]$ExpectedToolCount = 17
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

$resolvedCommand = Get-Command $Command -CommandType Application -ErrorAction Stop |
    Select-Object -First 1

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
$processInfo.FileName = $resolvedCommand.Source
foreach ($argument in $ArgumentList) {
    $processInfo.ArgumentList.Add($argument)
}
$processInfo.UseShellExecute = $false
$processInfo.RedirectStandardInput = $true
$processInfo.RedirectStandardOutput = $true
$processInfo.RedirectStandardError = $true
$processInfo.CreateNoWindow = $true
$processInfo.Environment["AGENT_BUS_STARTUP_ENABLED"] = "false"
$processInfo.Environment["RUST_LOG"] = "error"

$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $processInfo
$processStarted = $false
$stderrTask = $null

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

    [pscustomobject]@{
        ok              = $true
        command         = $resolvedCommand.Source
        protocolVersion = $protocolVersion
        serverName      = $serverName
        serverVersion   = $serverVersion
        toolCount       = $tools.Count
        tools           = $toolNames
    } | ConvertTo-Json -Depth 5 -Compress
}
finally {
    if ($processStarted -and -not $process.HasExited) {
        try {
            $process.StandardInput.Close()
        }
        catch {
            Write-Verbose "MCP stdin was already closed during cleanup."
        }
        if (-not $process.WaitForExit(500)) {
            $process.Kill($true)
            [void]$process.WaitForExit(2000)
        }
    }
    if ($stderrTask -and $stderrTask.IsCompleted) {
        # Drain stderr without printing it; it may contain environment-derived details.
        [void]$stderrTask.Result
    }
    $process.Dispose()
}
