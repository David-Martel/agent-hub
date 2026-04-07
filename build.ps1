param(
    [switch]$DetailedEnvironment,
    [switch]$SkipEnvironmentCheck,
    [switch]$SkipFormat,
    [switch]$SkipClippy,
    [switch]$SkipUnitTests,
    [switch]$SkipIntegrationTests,
    [switch]$SkipSmoke,
    [switch]$Release,
    [switch]$FastRelease,
    [switch]$DisableSccache,
    [switch]$DisableNextest,
    [string]$TargetDir,
    [string]$TargetNamespace
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$rustCliDir = Join-Path $repoRoot "rust-cli"
$workspaceManifest = Join-Path $repoRoot "Cargo.toml"
$validateScript = Join-Path $repoRoot "scripts\validate-agent-bus.ps1"
$commonBuildScript = Join-Path $repoRoot "scripts\rust-build-common.ps1"
$originalCargoToolsEnforceQuality = $env:CARGOTOOLS_ENFORCE_QUALITY
$originalCargoToolsRunTestsAfterBuild = $env:CARGOTOOLS_RUN_TESTS_AFTER_BUILD
$originalCargoToolsRunDoctestsAfterBuild = $env:CARGOTOOLS_RUN_DOCTESTS_AFTER_BUILD
$originalCargoPreflight = $env:CARGO_PREFLIGHT

if (-not (Test-Path $rustCliDir)) {
    throw "rust-cli directory not found at $rustCliDir"
}

if (-not (Test-Path $commonBuildScript)) {
    throw "Common Rust build helper not found at $commonBuildScript"
}
. $commonBuildScript

try {
    Import-Module CargoTools -ErrorAction Stop
    $null = Initialize-CargoEnv
    $env:CARGOTOOLS_ENFORCE_QUALITY = "0"
    $env:CARGOTOOLS_RUN_TESTS_AFTER_BUILD = "0"
    $env:CARGOTOOLS_RUN_DOCTESTS_AFTER_BUILD = "0"
    $env:CARGO_PREFLIGHT = "0"
}
catch {
    Write-Warning "CargoTools module unavailable; continuing with plain cargo environment."
}

$resolvedTargetDir = Resolve-AgentBusTargetDir -RepoRoot $repoRoot -ExplicitTargetDir $TargetDir -ExplicitNamespace $TargetNamespace
$buildEnvState = Use-AgentBusRustBuildEnv `
    -RepoRoot $repoRoot `
    -TargetDir $resolvedTargetDir `
    -PreferSccache:(-not $DisableSccache) `
    -PreferLldLink `
    -PreferFastLink:($FastRelease -or -not $Release) `
    -EnableIncremental:($FastRelease -or -not $Release) `
    -ResetSccacheStats `
    -ShowSummary

$useNextest = (-not $DisableNextest) -and [bool](Get-AgentBusCommandPath -Name "cargo-nextest")

if (-not $SkipEnvironmentCheck) {
    Write-Host "Checking Rust build environment..."
    if ($DetailedEnvironment) {
        Test-BuildEnvironment -Detailed
    }
    else {
        Test-BuildEnvironment
    }
}

function Invoke-CargoStep {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [Parameter(Mandatory = $true)]
        [string]$Command,
        [string[]]$AdditionalArgs = @(),
        [string]$WorkDir = $repoRoot
    )

    Write-Host "`n==> $Label"
    Push-Location $WorkDir
    try {
        & cargo $Command @AdditionalArgs
        $exitCode = $LASTEXITCODE
        if ($exitCode -ne 0) {
            throw "Cargo step failed: $Label (exit code $exitCode)"
        }
    }
    finally {
        Pop-Location
    }
}

function Invoke-TestStep {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [string[]]$CargoArgs,
        [string[]]$NextestArgs,
        [switch]$AllowNextest
    )

    if ($AllowNextest -and $useNextest) {
        Invoke-CargoStep -Label "$Label (nextest)" -Command "nextest" -AdditionalArgs $NextestArgs
    }
    else {
        Invoke-CargoStep -Label $Label -Command "test" -AdditionalArgs $CargoArgs
    }
}

try {
    if (-not $SkipFormat) {
        Invoke-CargoStep -Label "cargo fmt --all --check" -Command "fmt" -AdditionalArgs @("--manifest-path", $workspaceManifest, "--all", "--check")
    }

    if (-not $SkipClippy) {
        Invoke-CargoStep -Label "cargo clippy --workspace --all-targets -- -D warnings" -Command "clippy" -AdditionalArgs @("--manifest-path", $workspaceManifest, "--workspace", "--all-targets", "--", "-D", "warnings")
    }

    if (-not $SkipUnitTests) {
        Invoke-TestStep `
            -Label "cargo test --workspace --lib --bins" `
            -CargoArgs @("--manifest-path", $workspaceManifest, "--workspace", "--lib", "--bins") `
            -NextestArgs @("run", "--manifest-path", $workspaceManifest, "--target-dir", $resolvedTargetDir, "--workspace", "--lib", "--bins") `
            -AllowNextest
    }

    if (-not $SkipIntegrationTests) {
        Invoke-TestStep `
            -Label "cargo test integration (serial)" `
            -CargoArgs @("--manifest-path", $workspaceManifest, "--test", "http_integration_test", "--test", "integration_test", "--test", "channel_integration_test", "--", "--test-threads=1") `
            -NextestArgs @("run", "--manifest-path", $workspaceManifest, "--target-dir", $resolvedTargetDir, "--test", "http_integration_test", "--test", "integration_test", "--test", "channel_integration_test", "-j", "1") `
            -AllowNextest
    }

    if ($Release -or $FastRelease) {
        if ($FastRelease) {
            Invoke-CargoStep -Label "cargo build --profile fast-release --bins" -Command "build" -AdditionalArgs @("--profile", "fast-release", "--bins") -WorkDir $rustCliDir
            $releaseTarget = Join-Path $resolvedTargetDir "fast-release"
        }
        else {
            Invoke-CargoStep -Label "cargo build --release --bins" -Command "build" -AdditionalArgs @("--release", "--bins") -WorkDir $rustCliDir
            $releaseTarget = Join-Path $resolvedTargetDir "release"
        }
        foreach ($binaryName in @("agent-bus", "agent-bus-http", "agent-bus-mcp")) {
            $binaryPath = Find-AgentBusBuiltBinary -RustCliDir $rustCliDir -TargetDir $resolvedTargetDir -BinaryName $binaryName -Profile (Split-Path $releaseTarget -Leaf)
            if (-not $binaryPath) {
                throw "Expected built binary '$binaryName' was not found under $releaseTarget"
            }
        }
        Write-Host "Build target directory: $releaseTarget"
    }

    if (-not $SkipSmoke) {
        if (-not (Test-Path $validateScript)) {
            throw "Validation script not found at $validateScript"
        }

        Write-Host "`n==> Local functional smoke"
        & $validateScript -SkipBuild -SkipTests -TargetDir $resolvedTargetDir -DisableSccache:$DisableSccache -DisableNextest:$DisableNextest
        if ($LASTEXITCODE -ne 0) {
            throw "Local functional smoke failed"
        }
    }

    Write-Host "`nBuild orchestration complete."
}
finally {
    Write-AgentBusSccacheStats
    Restore-AgentBusRustBuildEnv -State $buildEnvState
    $env:CARGOTOOLS_ENFORCE_QUALITY = $originalCargoToolsEnforceQuality
    $env:CARGOTOOLS_RUN_TESTS_AFTER_BUILD = $originalCargoToolsRunTestsAfterBuild
    $env:CARGOTOOLS_RUN_DOCTESTS_AFTER_BUILD = $originalCargoToolsRunDoctestsAfterBuild
    $env:CARGO_PREFLIGHT = $originalCargoPreflight
}
