$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot "rust-build-common.ps1")

$commonScriptText = Get-Content -LiteralPath (Join-Path $PSScriptRoot "rust-build-common.ps1") -Raw
foreach ($forbiddenDaemonMutation in @("--stop-server", "--start-server")) {
    if ($commonScriptText.Contains($forbiddenDaemonMutation, [System.StringComparison]::Ordinal)) {
        throw "Shared sccache daemon mutation is forbidden in repo build helpers: $forbiddenDaemonMutation"
    }
}

$transportFailures = @(
    "sccache: error: failed to execute compile"
    "sccache: error: timed out"
    "Failed to bind socket (os error 10048)"
    "An existing connection was forcibly closed by the remote host. (os error 10054)"
    "sccache server not running"
)

foreach ($sample in $transportFailures) {
    if (-not (Test-AgentBusSccacheTransportFailure -Output @($sample))) {
        throw "Expected sccache transport failure was not recognized: $sample"
    }
}

if (Test-AgentBusSccacheTransportFailure -Output @("ordinary Rust compiler error")) {
    throw "Ordinary compiler failures must not be classified as sccache transport failures."
}

$writableHealth = [pscustomobject]@{
    ok = $true
    maintenance = [pscustomobject]@{ write_blocked = $false }
}
if (-not (Test-AgentBusWritableHealth -Health $writableHealth)) {
    throw "Healthy writable service was rejected."
}

$blockedHealth = [pscustomobject]@{
    ok = $true
    maintenance = [pscustomobject]@{ write_blocked = $true }
}
if (Test-AgentBusWritableHealth -Health $blockedHealth) {
    throw "Maintenance-blocked service was accepted as writable."
}

$originalWrapper = $env:RUSTC_WRAPPER
try {
    $env:RUSTC_WRAPPER = "fixture-sccache"
    Disable-AgentBusSccacheForCargoSteps
    if (Test-Path Env:RUSTC_WRAPPER) {
        throw "Disable-AgentBusSccacheForCargoSteps did not clear RUSTC_WRAPPER."
    }
    if (-not $script:AgentBusDisableSccacheForCargoSteps) {
        throw "Disable-AgentBusSccacheForCargoSteps did not enable the Cargo config override."
    }
}
finally {
    if ([string]::IsNullOrEmpty($originalWrapper)) {
        Remove-Item Env:RUSTC_WRAPPER -ErrorAction SilentlyContinue
    }
    else {
        $env:RUSTC_WRAPPER = $originalWrapper
    }
}

Write-Output "Rust build helper regression fixtures passed."
