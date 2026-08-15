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
.PARAMETER DryRun
    Print the deployment plan without building, copying binaries, changing
    services, or running smoke tests.
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
    [switch]$DisableSccache,
    # Reinstall the service bound off-loopback (LAN peers can reach this coordinator).
    # Passed through to install-agent-hub-service.ps1; requires an auth token in
    # ~/.config/agent-bus/config.json (or set one there first).
    [switch]$AllowRemote,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
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
if ($DryRun) {
    Write-Host "[DRY-RUN] Build/deploy plan:" -ForegroundColor Cyan
    if ($SkipBuild) {
        Write-Host "  - Skip cargo build and use binaries from: $resolvedTargetDir"
    }
    else {
        Write-Host "  - Run: cargo build --release --bins"
        Write-Host "  - Build target dir: $resolvedTargetDir"
        Write-Host "  - Sccache preference: $(-not $DisableSccache)"
    }
    Write-Host "  - Resolve built binaries: agent-bus, agent-bus-http, agent-bus-mcp"
    Write-Host "  - Deploy CLI binary to: $CliDeployPath"
    Write-Host "  - Deploy HTTP/service binary to: $DeployPath"
    Write-Host "  - Deploy MCP binary to: $McpDeployPath"
    if ($SkipService) {
        Write-Host "  - Skip service pause/stop/reinstall/start"
    }
    else {
        Write-Host "  - Pause/stop service through maintenance controls, reinstall $serviceName, start service"
        Write-Host "  - Health check: $healthUrl"
    }
    if ($SkipSmoke -or $SkipService) {
        Write-Host "  - Skip live SSE smoke test"
    }
    else {
        Write-Host "  - Run live SSE smoke script: $smokeScript"
    }
    Write-Host "  No binaries, services, or logs were changed."
    exit 0
}

$buildEnvState = Use-AgentBusRustBuildEnv `
    -RepoRoot $repoRoot `
    -TargetDir $resolvedTargetDir `
    -PreferSccache:(-not $DisableSccache) `
    -PreferLldLink `
    -ShowSummary

if ($DisableSccache) {
    Disable-AgentBusSccacheForCargoSteps
}

$cliTargetBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus"
$httpTargetBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus-http"
$mcpTargetBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus-mcp"

function Get-HealthSummary {
    param([Parameter(Mandatory = $true)]$Health)

    $protocol = if ($Health.protocol_version) { $Health.protocol_version } else { "n/a" }
    $runtime = if ($Health.runtime) { $Health.runtime } else { "n/a" }
    $codec = if ($Health.codec) { $Health.codec } else { "n/a" }
    $redisCount = if ($null -ne $Health.stream_length) { $Health.stream_length } else { "n/a" }
    $pgMessages = if ($null -ne $Health.pg_message_count) { $Health.pg_message_count } else { "n/a" }
    $pgPresence = if ($null -ne $Health.pg_presence_count) { $Health.pg_presence_count } else { "n/a" }

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
            $redisVersion = & redis-cli -u "redis://127.0.0.1:6380/0" INFO server 2>$null |
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
            $pgVersion = & psql "postgresql://postgres@127.0.0.1:5300/redis_backend" -Atqc "SHOW server_version;" 2>$null
            if ($pgVersion) {
                Write-Host "  PostgreSQL server version: $pgVersion"
            }
        }
        catch {
        }
    }
}

function Set-AgentBusAuthTokenForChildProcesses {
    if (-not [string]::IsNullOrWhiteSpace($env:AGENT_BUS_AUTH_TOKEN)) {
        Write-Host "Using AGENT_BUS_AUTH_TOKEN from current process for deploy validation."
        return
    }

    $configPath = Join-Path $HOME ".config/agent-bus/config.json"
    if (-not (Test-Path $configPath)) {
        Write-Host "No agent-bus client config found; deploy validation will run without bearer auth."
        return
    }

    try {
        $config = Get-Content -Path $configPath -Raw | ConvertFrom-Json
        if (-not [string]::IsNullOrWhiteSpace([string]$config.auth_token)) {
            $env:AGENT_BUS_AUTH_TOKEN = [string]$config.auth_token
            Write-Host "Loaded AGENT_BUS_AUTH_TOKEN for child deploy validation from client config (redacted)."
        }
    }
    catch {
        Write-Warning "Could not read $configPath for deploy auth token. Continuing without token."
        Write-Warning $_.Exception.Message
    }
}

function Copy-AgentBusBinary {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$Label
    )

    try {
        Copy-Item -LiteralPath $Source -Destination $Destination -Force
    }
    catch {
        throw "$Label deployment failed for '$Destination'. The existing binary may be in use. $($_.Exception.Message)"
    }

    $sourceHash = (Get-FileHash -LiteralPath $Source -Algorithm SHA256).Hash
    $destinationHash = (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash
    if ($sourceHash -ne $destinationHash) {
        throw "$Label deployment verification failed for '$Destination': source and destination hashes differ."
    }
}

function Get-NssmSetting {
    param(
        [Parameter(Mandatory = $true)][string]$NssmPath,
        [Parameter(Mandatory = $true)][string]$ServiceName,
        [Parameter(Mandatory = $true)][string]$Setting
    )

    $output = @(& $NssmPath get $ServiceName $Setting)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not capture NSSM setting '$Setting' for $ServiceName."
    }
    return $output
}

function Invoke-NssmChecked {
    param(
        [Parameter(Mandatory = $true)][string]$NssmPath,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    & $NssmPath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "nssm $($Arguments[0]) failed with exit code $LASTEXITCODE."
    }
}

function Restore-AgentBusServiceSnapshot {
    param(
        [Parameter(Mandatory = $true)][string]$NssmPath,
        [Parameter(Mandatory = $true)][string]$ServiceName,
        [Parameter(Mandatory = $true)]$Snapshot,
        [Parameter(Mandatory = $true)][string]$RollbackBinaryPath
    )

    $existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($existing) {
        if ($existing.Status -ne "Stopped") {
            Stop-Service -Name $ServiceName -Force
            $existing.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
        }
        Invoke-NssmChecked -NssmPath $NssmPath -Arguments @("remove", $ServiceName, "confirm") | Out-Null
        $removeDeadline = (Get-Date).AddSeconds(15)
        do {
            Start-Sleep -Milliseconds 200
            $existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
        } while ($existing -and (Get-Date) -lt $removeDeadline)
        if ($existing) {
            throw "Service '$ServiceName' still exists during rollback."
        }
    }

    $restoredApplication = $Snapshot.Application
    try {
        Copy-AgentBusBinary -Source $RollbackBinaryPath -Destination $restoredApplication -Label "Service rollback restore"
    }
    catch {
        Write-Warning "Could not restore the original service binary path; using the verified rollback copy directly."
        $restoredApplication = $RollbackBinaryPath
    }
    Invoke-NssmChecked -NssmPath $NssmPath -Arguments @("install", $ServiceName, $restoredApplication) | Out-Null
    Invoke-NssmChecked -NssmPath $NssmPath -Arguments @("set", $ServiceName, "AppParameters", $Snapshot.AppParameters) | Out-Null
    foreach ($setting in @("DisplayName", "Description", "Start", "AppDirectory", "AppStdout", "AppStderr", "AppRotateFiles", "AppRotateOnline", "AppRotateSeconds", "AppRotateBytes")) {
        Invoke-NssmChecked -NssmPath $NssmPath -Arguments @("set", $ServiceName, $setting, [string]$Snapshot.$setting) | Out-Null
    }
    $environmentEntries = @($Snapshot.AppEnvironmentExtra | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) })
    if ($environmentEntries.Count -gt 0) {
        $environmentArguments = @("set", $ServiceName, "AppEnvironmentExtra") + $environmentEntries
        Invoke-NssmChecked -NssmPath $NssmPath -Arguments $environmentArguments | Out-Null
    }
    else {
        Invoke-NssmChecked -NssmPath $NssmPath -Arguments @("reset", $ServiceName, "AppEnvironmentExtra") | Out-Null
    }
    & sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "sc.exe failure configuration failed during rollback with exit code $LASTEXITCODE."
    }
    if (-not (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue)) {
        throw "Service '$ServiceName' was not registered during rollback."
    }
    return $restoredApplication
}

Set-AgentBusAuthTokenForChildProcesses

# Step 1: Build
$serviceWasRunning = $false
$serviceExisted = $false
$serviceMutationStarted = $false
$previousServiceBinaryPath = $null
$previousServiceRollbackBinaryPath = $null
$previousServiceSnapshot = $null
$keepRollbackBinary = $false
if (-not $SkipService) {
    $initialService = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
    if ($initialService) {
        $serviceExisted = $true
        $serviceWasRunning = $initialService.Status -eq "Running"
        $nssmPath = (Get-Command nssm -ErrorAction Stop).Source
        $previousServiceBinaryPath = ([string](Get-NssmSetting -NssmPath $nssmPath -ServiceName $serviceName -Setting "Application" | Select-Object -Last 1)).Trim()
        if ([string]::IsNullOrWhiteSpace($previousServiceBinaryPath)) {
            throw "Could not capture the existing $serviceName binary path before deployment."
        }
        if (-not (Test-Path -LiteralPath $previousServiceBinaryPath)) {
            throw "Existing $serviceName binary was not found at '$previousServiceBinaryPath'."
        }
        $rollbackFileName = "agent-bus-http-rollback-$([guid]::NewGuid().ToString('N')).exe"
        $previousServiceRollbackBinaryPath = Join-Path (Split-Path -Parent $previousServiceBinaryPath) $rollbackFileName
        Copy-AgentBusBinary -Source $previousServiceBinaryPath -Destination $previousServiceRollbackBinaryPath -Label "Service rollback"
        $previousServiceSnapshot = [pscustomobject]@{
            Application        = $previousServiceBinaryPath
            AppParameters      = [string](Get-NssmSetting -NssmPath $nssmPath -ServiceName $serviceName -Setting "AppParameters" | Select-Object -Last 1)
            AppEnvironmentExtra = @(Get-NssmSetting -NssmPath $nssmPath -ServiceName $serviceName -Setting "AppEnvironmentExtra")
        }
        foreach ($setting in @("DisplayName", "Description", "Start", "AppDirectory", "AppStdout", "AppStderr", "AppRotateFiles", "AppRotateOnline", "AppRotateSeconds", "AppRotateBytes")) {
            $previousServiceSnapshot | Add-Member -NotePropertyName $setting -NotePropertyValue ([string](Get-NssmSetting -NssmPath $nssmPath -ServiceName $serviceName -Setting $setting | Select-Object -Last 1))
        }
    }
}
try {
    if (-not $SkipBuild) {
        Write-Host "Building release binary..."
        Invoke-AgentBusCargo `
            -Label "Build release binaries" `
            -Command "build" `
            -AdditionalArgs @("--release", "--bins") `
            -WorkDir $repoRoot
        Write-Host "Build complete."
    }

    # Step 2: Verify binaries exist
    $cliTargetBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus"
    $httpTargetBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus-http"
    $mcpTargetBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus-mcp"
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
    $versionOutput = & $httpTargetBinary --version
    if ($LASTEXITCODE -ne 0) {
        throw "Built HTTP binary version check failed with exit code $LASTEXITCODE."
    }
    $expectedBuildVersion = ([string]($versionOutput | Select-Object -Last 1)) -replace '^agent-bus-http\s+', ''
    if ([string]::IsNullOrWhiteSpace($expectedBuildVersion)) {
        throw "Could not determine the built HTTP binary version."
    }
    Write-Host "CLI binary:  $cliTargetBinary ($([math]::Round($cliSize, 1)) MB)"
    Write-Host "HTTP binary: $httpTargetBinary ($([math]::Round($httpSize, 1)) MB)"

    # Step 3: Stop service if running
    if (-not $SkipService) {
        $svc = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
        if ($svc -and $svc.Status -eq "Running") {
            $serviceMutationStarted = $true
            Write-Host "Pausing and stopping $serviceName service via built-in maintenance controls..."
            try {
                & $cliTargetBinary service --action pause --reason "build-deploy maintenance" --base-url "http://localhost:8400" --service-name $serviceName --encoding compact | Out-Null
                if ($LASTEXITCODE -ne 0) {
                    throw "Built-in service pause failed with exit code $LASTEXITCODE."
                }
            }
            catch {
                Write-Warning "Could not pause service via HTTP admin endpoint. Continuing with direct stop."
                Write-Warning $_.Exception.Message
            }

            try {
                & $cliTargetBinary service --action stop --reason "build-deploy maintenance" --base-url "http://localhost:8400" --service-name $serviceName --encoding compact | Out-Null
                if ($LASTEXITCODE -ne 0) {
                    throw "Built-in service stop failed with exit code $LASTEXITCODE."
                }
                $svc = Get-Service -Name $serviceName -ErrorAction Stop
                $svc.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(15))
            }
            catch {
                Write-Warning "Built-in service stop failed; falling back to Stop-Service."
                Write-Warning $_.Exception.Message
                Stop-Service -Name $serviceName -Force
                $svc = Get-Service -Name $serviceName -ErrorAction Stop
                $svc.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(15))
            }
        }
    }

    # Step 4: Deploy binaries
    if ($serviceExisted) {
        $serviceMutationStarted = $true
    }
    if ($CliDeployPath -and ($CliDeployPath -ne $DeployPath)) {
        Write-Host "Deploying CLI binary to $CliDeployPath..."
        Copy-AgentBusBinary -Source $cliTargetBinary -Destination $CliDeployPath -Label "CLI"
        Write-Host "CLI deploy complete."
    }

    Write-Host "Deploying HTTP/service binary to $deployPath..."
    Copy-AgentBusBinary -Source $httpTargetBinary -Destination $deployPath -Label "HTTP/service"
    Write-Host "HTTP/service deploy complete."

    if ($McpDeployPath) {
        Write-Host "Deploying MCP binary to $McpDeployPath..."
        Copy-AgentBusBinary -Source $mcpTargetBinary -Destination $McpDeployPath -Label "MCP"
        Write-Host "MCP deploy complete."
    }

    # Step 5: Reinstall/start service
    if (-not $SkipService) {
        if (-not (Test-Path $installServiceScript)) {
            throw "Service install script not found at $installServiceScript"
        }

        Write-Host "Reinstalling $serviceName service against $deployPath$(if ($AllowRemote) { ' (remote-enabled)' })..."
        $serviceMutationStarted = $true
        & $installServiceScript -ServiceName $serviceName -BinaryPath $deployPath -ForceReinstall -StartService:$false -AllowRemote:$AllowRemote
        if ($LASTEXITCODE -ne 0) {
            throw "Service reinstall failed"
        }

        $svc = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
        if ($svc) {
            Write-Host "Starting $serviceName service via built-in control..."
            & $cliTargetBinary service --action start --service-name $serviceName --base-url "http://localhost:8400" --timeout-seconds 15 --encoding compact | Out-Null
            if ($LASTEXITCODE -ne 0) {
                throw "Service start failed"
            }
            & $cliTargetBinary service --action resume --reason "build-deploy complete" --service-name $serviceName --base-url "http://localhost:8400" --timeout-seconds 15 --encoding compact | Out-Null
            if ($LASTEXITCODE -ne 0) {
                throw "Service resume failed"
            }

            # Health check
            $deadline = (Get-Date).AddSeconds(10)
            $health = $null
            $healthValidated = $false
            do {
                try {
                    $health = Invoke-RestMethod -Uri $healthUrl -Method Get -TimeoutSec 3
                    if (Test-AgentBusWritableHealth -Health $health) {
                        if ([string]$health.build_version -ne $expectedBuildVersion) {
                            throw "Service build version '$($health.build_version)' does not match deployed build '$expectedBuildVersion'."
                        }
                        Write-Host "`nService healthy:"
                        Get-HealthSummary -Health $health | ForEach-Object { Write-Host $_ }
                        Write-ServerVersionDiagnostics
                        $healthValidated = $true
                        break
                    }
                }
                catch { Start-Sleep -Seconds 1 }
            } while ((Get-Date) -lt $deadline)

            if (-not $healthValidated) {
                throw "Service started but writable health validation timed out."
            }
        }
        else {
            throw "Service '$serviceName' was not installed after deployment."
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
catch {
    if (-not $SkipService -and $serviceExisted -and $serviceMutationStarted) {
        try {
            Write-Warning "Deployment failed after changing $serviceName; restoring the previous service configuration."
            $keepRollbackBinary = $true
            $restoredApplication = Restore-AgentBusServiceSnapshot `
                -NssmPath $nssmPath `
                -ServiceName $serviceName `
                -Snapshot $previousServiceSnapshot `
                -RollbackBinaryPath $previousServiceRollbackBinaryPath
            $keepRollbackBinary = $restoredApplication -eq $previousServiceRollbackBinaryPath
            $recoveryService = Get-Service -Name $serviceName -ErrorAction Stop
            if ($serviceWasRunning) {
                Start-Service -Name $serviceName
                $recoveryService.WaitForStatus("Running", [TimeSpan]::FromSeconds(15))
                & $cliTargetBinary service --action resume --reason "build-deploy recovery" --service-name $serviceName --base-url "http://localhost:8400" --timeout-seconds 15 --encoding compact | Out-Null
                if ($LASTEXITCODE -ne 0) {
                    throw "Service recovery resume failed with exit code $LASTEXITCODE."
                }
            }
        }
        catch {
            Write-Warning "Could not restore $serviceName after deployment failure: $($_.Exception.Message)"
        }
    }
    throw
}
finally {
    if ($previousServiceRollbackBinaryPath -and -not $keepRollbackBinary -and (Test-Path -LiteralPath $previousServiceRollbackBinaryPath)) {
        try {
            Remove-Item -LiteralPath $previousServiceRollbackBinaryPath -Force
        }
        catch {
            Write-Warning "Could not remove unused rollback binary '$previousServiceRollbackBinaryPath': $($_.Exception.Message)"
        }
    }
    Write-AgentBusSccacheStats
    Restore-AgentBusRustBuildEnv -State $buildEnvState
}
