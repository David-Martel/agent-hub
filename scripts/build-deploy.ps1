<#
.SYNOPSIS
    Build, deploy, and restart the Agent Hub service.
.DESCRIPTION
    1. Builds the release binary (uses sccache if available)
    2. Copies the built CLI, HTTP, and MCP binaries into ~\bin
    3. Restarts the AgentHub Windows service
    4. Verifies health endpoint responds
    5. Runs a live HTTP/SSE notification smoke test
.PARAMETER SkipBuild
    Skip the cargo build step (deploy existing binary only)
.PARAMETER SkipService
    Skip service restart (build and copy only)
.PARAMETER SkipSmoke
    Skip the live HTTP/SSE notification smoke test
#>
param(
    [switch]$SkipBuild,
    [switch]$SkipService,
    [switch]$SkipSmoke,
    [string]$CliDeployPath = (Join-Path $HOME "bin/agent-bus.exe"),
    [string]$DeployPath = (Join-Path $HOME "bin/agent-bus-http.exe"),
    [string]$McpDeployPath = (Join-Path $HOME "bin/agent-bus-mcp.exe"),
    [string]$TargetDir,
    [string]$TargetNamespace,
    [switch]$DisableSccache
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$rustCliDir = Join-Path $repoRoot "rust-cli"
$scriptsDir = Join-Path $repoRoot "scripts"
$commonBuildScript = Join-Path $scriptsDir "rust-build-common.ps1"
$serviceName = "AgentHub"
$healthUrl = "http://localhost:8400/health"
$smokeScript = Join-Path $scriptsDir "test-agent-bus-sse-smoke.ps1"
$installServiceScript = Join-Path $scriptsDir "install-agent-hub-service.ps1"

if (-not (Test-Path $commonBuildScript)) {
    throw "Common Rust build helper not found at $commonBuildScript"
}
. $commonBuildScript

$resolvedTargetDir = Resolve-AgentBusTargetDir -RepoRoot $repoRoot -ExplicitTargetDir $TargetDir -ExplicitNamespace $TargetNamespace
$buildEnvState = Use-AgentBusRustBuildEnv `
    -RepoRoot $repoRoot `
    -TargetDir $resolvedTargetDir `
    -PreferSccache:(-not $DisableSccache) `
    -PreferLldLink `
    -ResetSccacheStats `
    -ShowSummary

$cliTargetBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliDir -TargetDir $resolvedTargetDir -BinaryName "agent-bus"
$httpTargetBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliDir -TargetDir $resolvedTargetDir -BinaryName "agent-bus-http"
$mcpTargetBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliDir -TargetDir $resolvedTargetDir -BinaryName "agent-bus-mcp"

function Get-HealthSummary {
    param([Parameter(Mandatory = $true)]$Health)

    $protocol = if ($Health.protocol_version) { $Health.protocol_version } else { "n/a" }
    $runtime = if ($Health.runtime) { $Health.runtime } else { "n/a" }
    $codec = if ($Health.codec) { $Health.codec } else { "n/a" }
    $redisCount = if ($Health.stream_length -ne $null) { $Health.stream_length } else { "n/a" }
    $pgMessages = if ($Health.pg_message_count -ne $null) { $Health.pg_message_count } else { "n/a" }
    $pgPresence = if ($Health.pg_presence_count -ne $null) { $Health.pg_presence_count } else { "n/a" }

    return @(
        "  Protocol: $protocol"
        "  Runtime: $runtime"
        "  Codec: $codec"
        "  Redis stream length: $redisCount"
        "  PostgreSQL messages: $pgMessages"
        "  PostgreSQL presence: $pgPresence"
    )
}

function Write-ServerVersionDiagnostics {
    if (Get-Command redis-cli -ErrorAction SilentlyContinue) {
        try {
            $redisVersion = & redis-cli -u "redis://localhost:6380/0" INFO server 2>$null |
                Select-String -Pattern '^redis_version:' |
                ForEach-Object { $_.ToString().Split(':', 2)[1].Trim() } |
                Select-Object -First 1
            if ($redisVersion) {
                Write-Host "  Redis server version: $redisVersion"
            }
        }
        catch {
        }
    }

    if (Get-Command psql -ErrorAction SilentlyContinue) {
        try {
            $pgVersion = & psql "postgresql://postgres@localhost:5300/redis_backend" -Atqc "SHOW server_version;" 2>$null
            if ($pgVersion) {
                Write-Host "  PostgreSQL server version: $pgVersion"
            }
        }
        catch {
        }
    }
}

# Step 1: Build
try {
    if (-not $SkipBuild) {
        Write-Host "Building release binary..."
        Push-Location $rustCliDir
        try {
            & cargo build --release --bins
            if ($LASTEXITCODE -ne 0) { throw "cargo build --release --bins failed" }
        }
        finally {
            Pop-Location
        }
        Write-Host "Build complete."
    }

    # Step 2: Verify binaries exist
    $cliTargetBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliDir -TargetDir $resolvedTargetDir -BinaryName "agent-bus"
    $httpTargetBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliDir -TargetDir $resolvedTargetDir -BinaryName "agent-bus-http"
    $mcpTargetBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliDir -TargetDir $resolvedTargetDir -BinaryName "agent-bus-mcp"
    if (-not (Test-Path $cliTargetBinary)) {
        throw "CLI binary not found after build."
    }
    if (-not (Test-Path $httpTargetBinary)) {
        throw "HTTP binary not found after build."
    }
    if (-not (Test-Path $mcpTargetBinary)) {
        throw "MCP binary not found after build."
    }
    $cliSize = (Get-Item $cliTargetBinary).Length / 1MB
    $httpSize = (Get-Item $httpTargetBinary).Length / 1MB
    Write-Host "CLI binary:  $cliTargetBinary ($([math]::Round($cliSize, 1)) MB)"
    Write-Host "HTTP binary: $httpTargetBinary ($([math]::Round($httpSize, 1)) MB)"

    # Step 3: Stop service if running
    if (-not $SkipService) {
        $svc = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
        if ($svc -and $svc.Status -eq "Running") {
            Write-Host "Pausing and stopping $serviceName service via built-in maintenance controls..."
            try {
                & $cliTargetBinary service --action pause --reason "build-deploy maintenance" --base-url "http://localhost:8400" --service-name $serviceName --encoding compact | Out-Null
            }
            catch {
                Write-Warning "Could not pause service via HTTP admin endpoint. Continuing with direct stop."
                Write-Warning $_.Exception.Message
            }

            try {
                & $cliTargetBinary service --action stop --reason "build-deploy maintenance" --base-url "http://localhost:8400" --service-name $serviceName --encoding compact | Out-Null
            }
            catch {
                Write-Warning "Built-in service stop failed; falling back to Stop-Service."
                Write-Warning $_.Exception.Message
                Stop-Service -Name $serviceName -Force
                Start-Sleep -Seconds 2
            }
        }
    }

    # Step 4: Deploy binaries
    if ($CliDeployPath -and ($CliDeployPath -ne $DeployPath)) {
        Write-Host "Deploying CLI binary to $CliDeployPath..."
        try {
            Copy-Item -Path $cliTargetBinary -Destination $CliDeployPath -Force
            Write-Host "CLI deploy complete."
        }
        catch {
            Write-Warning "Could not replace $CliDeployPath. Keeping the existing CLI binary because it is likely in use."
            Write-Warning $_.Exception.Message
        }
    }

    Write-Host "Deploying HTTP/service binary to $deployPath..."
    Copy-Item -Path $httpTargetBinary -Destination $deployPath -Force
    Write-Host "HTTP/service deploy complete."

    if ($McpDeployPath) {
        Write-Host "Deploying MCP binary to $McpDeployPath..."
        try {
            Copy-Item -Path $mcpTargetBinary -Destination $McpDeployPath -Force
            Write-Host "MCP deploy complete."
        }
        catch {
            Write-Warning "Could not replace $McpDeployPath. Keeping the existing MCP binary because it is likely in use."
            Write-Warning $_.Exception.Message
        }
    }

    # Step 5: Reinstall/start service
    if (-not $SkipService) {
        if (-not (Test-Path $installServiceScript)) {
            throw "Service install script not found at $installServiceScript"
        }

        Write-Host "Reinstalling $serviceName service against $deployPath..."
        & $installServiceScript -ServiceName $serviceName -BinaryPath $deployPath -ForceReinstall -StartService:$false
        if ($LASTEXITCODE -ne 0) {
            throw "Service reinstall failed"
        }

        $svc = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
        if ($svc) {
            Write-Host "Starting $serviceName service via built-in control..."
            & $cliTargetBinary service --action start --service-name $serviceName --base-url "http://localhost:8400" --timeout-seconds 15 --encoding compact | Out-Null

            # Health check
            $deadline = (Get-Date).AddSeconds(10)
            $health = $null
            do {
                try {
                    $health = Invoke-RestMethod -Uri $healthUrl -Method Get -TimeoutSec 3
                    if ($health.ok -eq $true) {
                        Write-Host "`nService healthy:"
                        Get-HealthSummary -Health $health | ForEach-Object { Write-Host $_ }
                        Write-ServerVersionDiagnostics
                        break
                    }
                }
                catch { Start-Sleep -Seconds 1 }
            } while ((Get-Date) -lt $deadline)

            if ($health -eq $null -or $health.ok -ne $true) {
                Write-Warning "Service started but health check timed out"
            }
        }
        else {
            Write-Host "Service '$serviceName' not installed. Run install-agent-hub-service.ps1 first."
        }
    }

    if (-not $SkipSmoke -and -not $SkipService) {
        if (-not (Test-Path $smokeScript)) {
            throw "Smoke test script not found at $smokeScript"
        }

        Write-Host "`nRunning SSE notification smoke test..."
        & $smokeScript -BaseUrl "http://localhost:8400"
    }
    elseif ($SkipService -and -not $SkipSmoke) {
        Write-Host "`nSkipping SSE smoke test because service restart was skipped."
    }

    Write-Host "Done."
}
finally {
    Write-AgentBusSccacheStats
    Restore-AgentBusRustBuildEnv -State $buildEnvState
}
