param(
    [switch]$SkipBuild,
    [switch]$SkipTests,
    [switch]$SkipHealth,
    [switch]$SkipSmoke,
    [string]$TargetDir,
    [string]$TargetNamespace,
    [switch]$DisableSccache,
    [switch]$DisableNextest,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
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

$resolvedTargetDir = Resolve-AgentBusTargetDir -RepoRoot $repoRoot -ExplicitTargetDir $TargetDir -ExplicitNamespace $TargetNamespace
$effectiveRedisUrl = if ($env:AGENT_BUS_REDIS_URL) { $env:AGENT_BUS_REDIS_URL } else { "redis://127.0.0.1:6380/0" }
$effectiveDatabaseUrl = if ($env:AGENT_BUS_DATABASE_URL) { $env:AGENT_BUS_DATABASE_URL } else { "postgresql://postgres@127.0.0.1:5300/redis_backend" }
$effectiveServerHost = if ($env:AGENT_BUS_SERVER_HOST) { $env:AGENT_BUS_SERVER_HOST } else { "localhost" }
if ($DryRun) {
    Write-Host "[DRY-RUN] Validation plan:" -ForegroundColor Cyan
    Write-Host "  - Workspace manifest: $workspaceManifest"
    Write-Host "  - Target dir: $resolvedTargetDir"
    Write-Host "  - Effective env defaults if currently absent:"
    Write-Host "      AGENT_BUS_REDIS_URL=$effectiveRedisUrl"
    Write-Host "      AGENT_BUS_DATABASE_URL=$effectiveDatabaseUrl"
    Write-Host "      AGENT_BUS_SERVER_HOST=$effectiveServerHost"
    if (-not $SkipBuild) {
        Write-Host "  - Run: cargo build --profile fast-release --bins"
    }
    if (-not $SkipTests) {
        Write-Host "  - Run workspace lib/bin tests and serial integration tests"
    }
    if (-not $SkipHealth) {
        Write-Host "  - Run live health through: $invokeScript"
    }
    if (-not $SkipSmoke) {
        Write-Host "  - Run functional smoke script: $functionalSmokeScript"
    }
    Write-Host "  No environment variables, target directories, sccache state, binaries, or services were changed."
    exit 0
}

if (-not $env:AGENT_BUS_REDIS_URL) {
    $env:AGENT_BUS_REDIS_URL = "redis://127.0.0.1:6380/0"
}
if (-not $env:AGENT_BUS_DATABASE_URL) {
    $env:AGENT_BUS_DATABASE_URL = "postgresql://postgres@127.0.0.1:5300/redis_backend"
}
if (-not $env:AGENT_BUS_SERVER_HOST) {
    $env:AGENT_BUS_SERVER_HOST = "localhost"
}

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
        Invoke-AgentBusCargo -Label "cargo build --profile fast-release --bins" -Command "build" -AdditionalArgs @("--profile", "fast-release", "--bins") -WorkDir $repoRoot

        foreach ($binaryName in @("agent-bus", "agent-bus-http", "agent-bus-mcp")) {
            $binaryPath = Find-AgentBusBuiltBinary -WorkspaceRoot $repoRoot -TargetDir $resolvedTargetDir -BinaryName $binaryName -Profile "fast-release"
            if (-not $binaryPath) {
                throw "Expected built binary '$binaryName' was not found in the fast-release target dir."
            }
        }
    }

    if (-not $SkipTests) {
        Invoke-AgentBusCargoTest `
            -Label "cargo test --workspace --lib --bins" `
            -CargoArgs @("--manifest-path", $workspaceManifest, "--workspace", "--lib", "--bins") `
            -NextestArgs @("run", "--manifest-path", $workspaceManifest, "--target-dir", $resolvedTargetDir, "--workspace", "--lib", "--bins") `
            -AllowNextest -UseNextest $useNextest

        Invoke-AgentBusCargoTest `
            -Label "cargo test integration (serial)" `
            -CargoArgs @("--manifest-path", $workspaceManifest, "--test", "integration_test", "--test", "http_integration_test", "--test", "channel_integration_test", "--", "--test-threads=1") `
            -NextestArgs @("run", "--manifest-path", $workspaceManifest, "--target-dir", $resolvedTargetDir, "--test", "integration_test", "--test", "http_integration_test", "--test", "channel_integration_test", "-j", "1") `
            -AllowNextest -UseNextest $useNextest
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
