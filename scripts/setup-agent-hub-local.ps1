param(
    [string]$DatabaseName = "redis_backend",
    [int]$DatabasePort = 5300,
    [switch]$SkipBuild,
    [switch]$SkipHealth,
    [string]$TargetDir,
    [string]$TargetNamespace,
    [switch]$DisableSccache
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$rustCliRoot = Join-Path $repoRoot "rust-cli"
$invokeScript = Join-Path $PSScriptRoot "invoke-agent-bus-cli.ps1"
$commonBuildScript = Join-Path $PSScriptRoot "rust-build-common.ps1"
$homeBin = Join-Path $HOME "bin"
$installedBinary = Join-Path $homeBin "agent-bus.exe"
$installedHttpBinary = Join-Path $homeBin "agent-bus-http.exe"
$installedMcpBinary = Join-Path $homeBin "agent-bus-mcp.exe"
$psql = (Get-Command psql.exe -ErrorAction SilentlyContinue).Source
$createdb = (Get-Command createdb.exe -ErrorAction SilentlyContinue).Source
$queryDatabaseName = $DatabaseName.Replace("'", "''")

if (-not $psql) {
    throw "psql.exe was not found on PATH"
}
if (-not $createdb) {
    throw "createdb.exe was not found on PATH"
}

if (-not (Test-Path $commonBuildScript)) {
    throw "Common Rust build helper not found at $commonBuildScript"
}
. $commonBuildScript

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

$env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
$env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@localhost:$DatabasePort/$DatabaseName"
$env:AGENT_BUS_SERVER_HOST = "localhost"

$resolvedTargetDir = Resolve-AgentBusTargetDir -RepoRoot $repoRoot -ExplicitTargetDir $TargetDir -ExplicitNamespace $TargetNamespace
$buildEnvState = Use-AgentBusRustBuildEnv `
    -RepoRoot $repoRoot `
    -TargetDir $resolvedTargetDir `
    -PreferSccache:(-not $DisableSccache) `
    -PreferLldLink `
    -ShowSummary

try {
    if (-not $SkipBuild) {
        Write-Host "Building native agent-bus binaries ..."
        Push-Location $rustCliRoot
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

    $repoCliBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus"
    $repoHttpBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus-http"
    $repoMcpBinary = Find-AgentBusBuiltBinary -RustCliDir $rustCliRoot -TargetDir $resolvedTargetDir -BinaryName "agent-bus-mcp"
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
    }
}
finally {
    Write-AgentBusSccacheStats
    Restore-AgentBusRustBuildEnv -State $buildEnvState
}
