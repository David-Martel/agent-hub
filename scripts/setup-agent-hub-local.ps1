param(
    [string]$DatabaseName = "redis_backend",
    [switch]$SkipBuild,
    [switch]$SkipHealth
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$rustCliRoot = Join-Path $repoRoot "rust-cli"
$invokeScript = Join-Path $PSScriptRoot "invoke-agent-bus-cli.ps1"
$homeBin = Join-Path $HOME "bin"
$installedBinary = Join-Path $homeBin "agent-bus.exe"
$postgresBin = "C:\Program Files\PostgreSQL\18\pgsql\bin"
$psql = Join-Path $postgresBin "psql.exe"
$createdb = Join-Path $postgresBin "createdb.exe"
$queryDatabaseName = $DatabaseName.Replace("'", "''")

if (-not (Test-Path $psql)) {
    throw "psql.exe was not found at $psql"
}
if (-not (Test-Path $createdb)) {
    throw "createdb.exe was not found at $createdb"
}

foreach ($serviceName in @("Redis", "postgresql-x64-18")) {
    $service = Get-Service -Name $serviceName -ErrorAction Stop
    if ($service.Status -ne "Running") {
        Write-Host "Starting service $serviceName ..."
        Start-Service -Name $serviceName
        $service.WaitForStatus("Running", [TimeSpan]::FromSeconds(30))
    }
}

$databaseExists = & $psql -h localhost -U postgres -d postgres -tAc "select 1 from pg_database where datname = '$queryDatabaseName'"
if ($LASTEXITCODE -ne 0) {
    throw "Failed to query PostgreSQL for database '$DatabaseName'."
}
if ($databaseExists.Trim() -ne "1") {
    Write-Host "Creating PostgreSQL database $DatabaseName ..."
    & $createdb -h localhost -U postgres $DatabaseName
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to create PostgreSQL database '$DatabaseName'."
    }
}

$env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
$env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@localhost:5432/$DatabaseName"
$env:AGENT_BUS_SERVER_HOST = "localhost"

if (-not $SkipBuild) {
    Write-Host "Building native agent-bus binary ..."
    Push-Location $rustCliRoot
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release failed."
        }
    }
    finally {
        Pop-Location
    }
}

$repoBinary = Join-Path $rustCliRoot "target\release\agent-bus.exe"
if (-not (Test-Path $repoBinary)) {
    throw "Built agent-bus binary was not found at $repoBinary"
}

New-Item -ItemType Directory -Path $homeBin -Force | Out-Null
Copy-Item -Path $repoBinary -Destination $installedBinary -Force
Write-Host "Installed $installedBinary"

if (-not $SkipHealth) {
    Write-Host "Bootstrapping health/storage check ..."
    & $invokeScript "health" "--encoding" "json"
    if ($LASTEXITCODE -ne 0) {
        throw "agent-bus health check failed."
    }
}
