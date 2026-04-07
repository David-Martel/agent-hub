param(
    [switch]$SkipBuild,
    [switch]$SkipTests,
    [switch]$SkipHealth,
    [switch]$SkipSmoke,
    [string]$TargetDir,
    [string]$TargetNamespace,
    [switch]$DisableSccache,
    [switch]$DisableNextest
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$rustCliDir = Join-Path $repoRoot "rust-cli"
$workspaceManifest = Join-Path $repoRoot "Cargo.toml"
$invokeScript = Join-Path $PSScriptRoot "invoke-agent-bus-cli.ps1"
$functionalSmokeScript = Join-Path $PSScriptRoot "test-agent-bus-functional.ps1"
$commonBuildScript = Join-Path $PSScriptRoot "rust-build-common.ps1"

if (-not (Test-Path $commonBuildScript)) {
    throw "Common Rust build helper not found at $commonBuildScript"
}
. $commonBuildScript

function Write-ServerVersionDiagnostics {
    if (Get-Command redis-cli -ErrorAction SilentlyContinue) {
        try {
            $redisVersion = & redis-cli -u $env:AGENT_BUS_REDIS_URL INFO server 2>$null |
                Select-String -Pattern '^redis_version:' |
                ForEach-Object { $_.ToString().Split(':', 2)[1].Trim() } |
                Select-Object -First 1
            if ($redisVersion) {
                Write-Host "Redis server version: $redisVersion"
            }
        }
        catch {
            Write-Host "Redis version probe skipped: $($_.Exception.Message)"
        }
    }

    if (Get-Command psql -ErrorAction SilentlyContinue) {
        try {
            $pgVersion = & psql $env:AGENT_BUS_DATABASE_URL -Atqc "SHOW server_version;" 2>$null
            if ($pgVersion) {
                Write-Host "PostgreSQL server version: $pgVersion"
            }
        }
        catch {
            Write-Host "PostgreSQL version probe skipped: $($_.Exception.Message)"
        }
    }
}

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
}
if (-not $env:AGENT_BUS_DATABASE_URL) {
    $env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@localhost:5300/redis_backend"
}
if (-not $env:AGENT_BUS_SERVER_HOST) {
    $env:AGENT_BUS_SERVER_HOST = "localhost"
}

$resolvedTargetDir = Resolve-AgentBusTargetDir -RepoRoot $repoRoot -ExplicitTargetDir $TargetDir -ExplicitNamespace $TargetNamespace
$buildEnvState = Use-AgentBusRustBuildEnv `
    -RepoRoot $repoRoot `
    -TargetDir $resolvedTargetDir `
    -PreferSccache:(-not $DisableSccache) `
    -PreferLldLink `
    -PreferFastLink `
    -EnableIncremental `
    -ShowSummary

$useNextest = (-not $DisableNextest) -and [bool](Get-AgentBusCommandPath -Name "cargo-nextest")

try {
    if (-not $SkipBuild) {
        Write-Host "Building native agent-bus binaries (fast-release profile)..."
        Push-Location $rustCliDir
        try {
            & cargo build --profile fast-release --bins
            if ($LASTEXITCODE -ne 0) {
                throw "cargo build --profile fast-release --bins failed."
            }
        }
        finally {
            Pop-Location
        }

        foreach ($binaryName in @("agent-bus", "agent-bus-http", "agent-bus-mcp")) {
            $binaryPath = Find-AgentBusBuiltBinary -RustCliDir $rustCliDir -TargetDir $resolvedTargetDir -BinaryName $binaryName -Profile "fast-release"
            if (-not $binaryPath) {
                throw "Expected built binary '$binaryName' was not found in the fast-release target dir."
            }
        }
    }

    if (-not $SkipTests) {
        Write-Host "Running native test suite..."
        if ($useNextest) {
            & cargo nextest run --manifest-path $workspaceManifest --target-dir $resolvedTargetDir --workspace --lib --bins
            if ($LASTEXITCODE -ne 0) {
                throw "cargo nextest run --workspace --lib --bins failed."
            }

            & cargo nextest run --manifest-path $workspaceManifest --target-dir $resolvedTargetDir --test integration_test --test http_integration_test --test channel_integration_test -j 1
            if ($LASTEXITCODE -ne 0) {
                throw "cargo nextest run integration targets failed."
            }
        }
        else {
            & cargo test --manifest-path $workspaceManifest --workspace --lib --bins
            if ($LASTEXITCODE -ne 0) {
                throw "cargo test --workspace --lib --bins failed."
            }

            & cargo test --manifest-path $workspaceManifest --test integration_test --test http_integration_test --test channel_integration_test -- --test-threads=1
            if ($LASTEXITCODE -ne 0) {
                throw "cargo test integration targets failed."
            }
        }
    }

    if (-not $SkipHealth) {
        Write-Host "Running live Redis/PostgreSQL health check..."
        & $invokeScript "health" "--encoding" "json"
        if ($LASTEXITCODE -ne 0) {
            throw "agent-bus health validation failed."
        }
        Write-ServerVersionDiagnostics
    }

    if (-not $SkipSmoke) {
        Write-Host "Running CLI + HTTP functional smoke checks..."
        & $functionalSmokeScript -CliPath (Join-Path $HOME "bin/agent-bus.exe") -HttpBinaryPath (Join-Path $HOME "bin/agent-bus-http.exe") -HttpPort 8412
    }
}
finally {
    Write-AgentBusSccacheStats
    Restore-AgentBusRustBuildEnv -State $buildEnvState
}
