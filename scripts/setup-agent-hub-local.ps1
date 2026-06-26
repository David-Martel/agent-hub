param(
    [string]$DatabaseName = "redis_backend",
    [int]$DatabasePort = 5300,
    [switch]$SkipBuild,
    [switch]$SkipHealth,
    [string]$TargetDir,
    [string]$TargetNamespace,
    [switch]$DisableSccache,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot

$commonBuildScript = Join-Path $PSScriptRoot "rust-build-common.ps1"
$homeBin = Join-Path $HOME "bin"
$installedBinary = Join-Path $homeBin "agent-bus.exe"
$installedHttpBinary = Join-Path $homeBin "agent-bus-http.exe"
$installedMcpBinary = Join-Path $homeBin "agent-bus-mcp.exe"
$psql = (Get-Command psql.exe -ErrorAction SilentlyContinue).Source
$createdb = (Get-Command createdb.exe -ErrorAction SilentlyContinue).Source
$queryDatabaseName = $DatabaseName.Replace("'", "''")

if (-not (Test-Path $commonBuildScript)) {
    throw "Common Rust build helper not found at $commonBuildScript"
}
. $commonBuildScript

$resolvedTargetDir = Resolve-AgentBusTargetDir -RepoRoot $repoRoot -ExplicitTargetDir $TargetDir -ExplicitNamespace $TargetNamespace
if ($DryRun) {
    Write-Host "[DRY-RUN] Local setup plan:" -ForegroundColor Cyan
    Write-Host "  - Start or verify Windows services: Redis, postgresql-x64-18"
    Write-Host "  - Query/create PostgreSQL database: $DatabaseName on port $DatabasePort"
    Write-Host "  - Set process env:"
    Write-Host "      AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0"
    Write-Host "      AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:$DatabasePort/$DatabaseName"
    Write-Host "      AGENT_BUS_SERVER_HOST=localhost"
    if ($SkipBuild) {
        Write-Host "  - Skip cargo build and use existing binaries under: $resolvedTargetDir"
    }
    else {
        Write-Host "  - Run: cargo build --release --bins"
        Write-Host "  - Build target dir: $resolvedTargetDir"
    }
    Write-Host "  - Install binaries to:"
    Write-Host "      $installedBinary"
    Write-Host "      $installedHttpBinary"
    Write-Host "      $installedMcpBinary"
    if ($SkipHealth) {
        Write-Host "  - Skip final agent-bus health check"
    }
    else {
        Write-Host "  - Run final agent-bus health check"
    }
    Write-Host "  - If the AgentHub service uses bearer auth, align the local client with:"
    Write-Host "      pwsh -NoLogo -NoProfile -File scripts\sync-agent-bus-client-auth.ps1"
    Write-Host "      pwsh -NoLogo -NoProfile -File scripts\validate-agent-client-configs.ps1"
    if (-not $psql) { Write-Host "  - Note: psql.exe is not currently on PATH" -ForegroundColor Yellow }
    if (-not $createdb) { Write-Host "  - Note: createdb.exe is not currently on PATH" -ForegroundColor Yellow }
    Write-Host "  No services, databases, binaries, or environment variables were changed."
    exit 0
}

if (-not $psql) {
    throw "psql.exe was not found on PATH"
}
if (-not $createdb) {
    throw "createdb.exe was not found on PATH"
}

foreach ($serviceName in @("Redis", "postgresql-x64-18")) {
    $service = Get-Service -Name $serviceName -ErrorAction Stop
    if ($service.Status -ne "Running") {
        Write-Host "Starting service $serviceName ..."
        Start-Service -Name $serviceName
        $service.WaitForStatus("Running", [TimeSpan]::FromSeconds(30))
    }
}

$databaseExists = & $psql -h localhost -p $DatabasePort -U postgres -d postgres -tAc "select 1 from pg_database where datname = '$queryDatabaseName'"
if ($LASTEXITCODE -ne 0) {
    throw "Failed to query PostgreSQL for database '$DatabaseName'."
}
if ($databaseExists.Trim() -ne "1") {
    Write-Host "Creating PostgreSQL database $DatabaseName ..."
    & $createdb -h localhost -p $DatabasePort -U postgres $DatabaseName
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to create PostgreSQL database '$DatabaseName'."
    }
}

$env:AGENT_BUS_REDIS_URL = "redis://127.0.0.1:6380/0"
$env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@127.0.0.1:$DatabasePort/$DatabaseName"
$env:AGENT_BUS_SERVER_HOST = "localhost"

$buildEnvState = Use-AgentBusRustBuildEnv `
    -RepoRoot $repoRoot `
    -TargetDir $resolvedTargetDir `
    -PreferSccache:(-not $DisableSccache) `
    -PreferLldLink `
    -ShowSummary

try {
    if (-not $SkipBuild) {
        Write-Host "Building native agent-bus binaries ..."
        Push-Location $repoRoot
        try {
            & cargo build --release --bins
            if ($LASTEXITCODE -ne 0) {
                throw "cargo build --release --bins failed."
            }
        }
        finally {
            Pop-Location
        }
    }

    $repoCliBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus"
    $repoHttpBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus-http"
    $repoMcpBinary = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus-mcp"
    if (-not $repoCliBinary) {
        throw "Built agent-bus CLI binary was not found."
    }
    if (-not $repoHttpBinary) {
        throw "Built agent-bus HTTP binary was not found."
    }
    if (-not $repoMcpBinary) {
        throw "Built agent-bus MCP binary was not found."
    }

    New-Item -ItemType Directory -Path $homeBin -Force | Out-Null
    $healthBinary = $repoCliBinary
    try {
        Copy-Item -Path $repoCliBinary -Destination $installedBinary -Force
        Write-Host "Installed $installedBinary"
        $healthBinary = $installedBinary
    }
    catch {
        Write-Warning "Could not replace $installedBinary. Keeping the existing command binary because it is likely in use."
        Write-Warning $_.Exception.Message
    }

    try {
        Copy-Item -Path $repoHttpBinary -Destination $installedHttpBinary -Force
        Write-Host "Installed $installedHttpBinary"
    }
    catch {
        Write-Warning "Could not replace $installedHttpBinary. Keeping the existing service binary because it is likely in use."
        Write-Warning $_.Exception.Message
    }

    try {
        Copy-Item -Path $repoMcpBinary -Destination $installedMcpBinary -Force
        Write-Host "Installed $installedMcpBinary"
    }
    catch {
        Write-Warning "Could not replace $installedMcpBinary. Keeping the existing MCP binary because it is likely in use."
        Write-Warning $_.Exception.Message
    }

    if (-not $SkipHealth) {
        Write-Host "Bootstrapping health/storage check ..."
        & $healthBinary "health" "--encoding" "json"
        if ($LASTEXITCODE -ne 0) {
            throw "agent-bus health check failed."
        }
        Write-Host "Validating installed client configuration ..."
        & (Join-Path $PSScriptRoot "validate-agent-client-configs.ps1")
        if ($LASTEXITCODE -ne 0) {
            throw "agent-bus client config validation failed."
        }
    }
}
finally {
    Write-AgentBusSccacheStats
    Restore-AgentBusRustBuildEnv -State $buildEnvState
}
